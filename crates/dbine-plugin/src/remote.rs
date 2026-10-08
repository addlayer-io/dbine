//! The app side: a driver host as a child process, and `RemoteDriver` /
//! `RemoteSession`, which implement the driver contract by forwarding each
//! call to it. The rest of the app can't tell them from a built-in driver.

use crate::proto::{read_frame, write_frame, Call, DriverMeta, FromHost, Hello, Ready, Reply, ToHost, WireError, BATCH_WINDOW, PROTOCOL};
use dbine_driver::transfer::{self, BatchSinkRef, BatchSource, BucketSum, CloneScript, CopySpec, DeltaResult, DeltaSpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::serde_static::intern;
use dbine_driver::{
    async_trait, Capabilities, ColumnFilter, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver,
    DriverInfo, Error, KeyPage, KeyScan, KeySearch, MonitorSnapshot, ObjectRef, ProfiledStatement, ProfilerOptions, ProfilerStarted,
    MessageSinkRef, ProgressSinkRef, QueryOutcome, Result, RowChange, RowSinkRef, Session, SyncScript, TableChange, TableSchema,
};
use std::collections::HashMap;
use std::future::Future;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

enum Waiter {
    Async(tokio::sync::oneshot::Sender<Result<Reply>>),
    Blocking(std::sync::mpsc::Sender<Result<Reply>>),
}

impl Waiter {
    fn send(self, r: Result<Reply>) {
        match self {
            Waiter::Async(tx) => {
                let _ = tx.send(r);
            }
            Waiter::Blocking(tx) => {
                let _ = tx.send(r);
            }
        }
    }
}

/// Where a run's streamed rows go (the app's export / migration sink).
#[derive(Clone)]
struct SinkTarget {
    sink: RowSinkRef,
    /// Result sets before this run's first one.
    base: usize,
    error: Arc<Mutex<Option<String>>>,
}

/// Where a `live` run's messages and ended statements go.
#[derive(Clone, Default)]
struct Live {
    messages: Option<MessageSinkRef>,
    progress: Option<ProgressSinkRef>,
}

struct Pending {
    waiter: Waiter,
    sink: Option<SinkTarget>,
    live: Option<Live>,
    /// A transfer call's events, for the task that made the call.
    events: Option<tokio::sync::mpsc::UnboundedSender<Event>>,
}

/// What a transfer call receives while it runs.
enum Event {
    Begin(Vec<TransferColumn>),
    Batch(RowBatch),
    Ack,
    Committed(u64),
}

/// A running driver host.
pub struct Host {
    package: String,
    stdin: Mutex<BufWriter<ChildStdin>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next: AtomicU64,
    alive: AtomicBool,
    child: Mutex<Child>,
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn dead(package: &str) -> Error {
    Error::Connect(format!("el driver «{package}» se cerró inesperadamente (el detalle está en el log de DBine); volvé a conectar"))
}

impl Host {
    /// Start `exe` (the host of `package`) and greet it. Blocking: call it
    /// off the async runtime.
    pub fn spawn(package: &str, exe: &std::path::Path, args: &[String], components_dir: Option<PathBuf>) -> Result<Arc<Host>> {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console window per driver.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::Connect(format!(
                    "no se pudo iniciar el driver «{package}»: el sistema o el antivirus lo bloqueó ({e}). Permití {} y volvé a conectar.",
                    exe.display()
                ))
            } else {
                Error::Connect(format!("no se pudo iniciar el driver «{package}»: {e}"))
            }
        })?;
        let stdin = child.stdin.take().ok_or_else(|| dead(package))?;
        let stdout = child.stdout.take().ok_or_else(|| dead(package))?;
        let stderr = child.stderr.take().ok_or_else(|| dead(package))?;

        let log_pkg = package.to_string();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                tracing::info!(target: "dbine_plugin", "[{log_pkg}] {line}");
            }
        });

        let mut writer = BufWriter::new(stdin);
        let hello = Hello {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            components_dir: components_dir.map(|d| d.display().to_string()),
        };
        write_frame(&mut writer, &hello).map_err(|_| dead(package))?;
        let mut reader = BufReader::new(stdout);
        let ready: Ready = read_frame(&mut reader).ok().flatten().ok_or_else(|| dead(package))?;
        // The file's name and SHA-256 already pin the driver's version; what
        // must match is how they talk.
        if ready.protocol != PROTOCOL {
            let _ = child.kill();
            return Err(Error::Connect(format!(
                "el driver «{package}» ({}) habla el protocolo {} y esta versión de DBine el {PROTOCOL}",
                ready.version, ready.protocol
            )));
        }
        tracing::info!(target: "dbine_plugin", "driver «{package}» {} listo", ready.version);
        Ok(Host::attach(package, writer, reader, child))
    }

    /// A greeted host: `stdin` takes the calls, `reader` gives its frames.
    fn attach(package: &str, stdin: BufWriter<ChildStdin>, mut reader: impl std::io::Read + Send + 'static, child: Child) -> Arc<Host> {
        let host = Arc::new(Host {
            package: package.to_string(),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            child: Mutex::new(child),
        });
        let weak = Arc::downgrade(&host);
        std::thread::spawn(move || {
            loop {
                let msg: Option<FromHost> = read_frame(&mut reader).ok().flatten();
                let Some(host) = weak.upgrade() else { return };
                match msg {
                    Some(m) => host.dispatch(m),
                    None => {
                        host.fail_all();
                        return;
                    }
                }
            }
        });
        host
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The host's process id.
    pub fn pid(&self) -> u32 {
        self.child.lock().map(|c| c.id()).unwrap_or(0)
    }

    fn fail_all(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let pending: Vec<Pending> = self.pending.lock().unwrap().drain().map(|(_, p)| p).collect();
        for p in pending {
            p.waiter.send(Err(dead(&self.package)));
        }
    }

    fn dispatch(&self, msg: FromHost) {
        match msg {
            FromHost::Reply { id, result } => {
                if let Some(p) = self.pending.lock().unwrap().remove(&id) {
                    p.waiter.send(result.map_err(|w: WireError| Error::from(w)));
                }
            }
            FromHost::SinkBegin { id, index, columns } => self.sink_event(id, |t| t.sink.0.lock().expect("sink").begin(t.base + index as usize, &columns)),
            FromHost::SinkRow { id, index, row } => self.sink_event(id, |t| t.sink.0.lock().expect("sink").row(t.base + index as usize, &row)),
            FromHost::Progress(p) => dbine_driver::runtime::report_progress(&p),
            FromHost::Message { id, message } => {
                if let Some(s) = self.live(id).and_then(|l| l.messages) {
                    (s.0)(&message);
                }
            }
            FromHost::StatementEnded { id, end } => {
                if let Some(s) = self.live(id).and_then(|l| l.progress) {
                    (s.0)(&end);
                }
            }
            FromHost::BatchBegin { id, columns } => self.event(id, Event::Begin(columns)),
            FromHost::Batch { id, batch } => self.event(id, Event::Batch(batch)),
            FromHost::BatchAck { id } => self.event(id, Event::Ack),
            FromHost::Committed { id, rows } => self.event(id, Event::Committed(rows)),
        }
    }

    /// Never blocks: this runs on the host's reader thread.
    fn event(&self, id: u64, ev: Event) {
        if let Some(tx) = self.pending.lock().unwrap().get(&id).and_then(|p| p.events.clone()) {
            let _ = tx.send(ev);
        }
    }

    /// Start a transfer call; its events go to `events`.
    fn start(&self, call: Call, events: tokio::sync::mpsc::UnboundedSender<Event>) -> Result<(u64, tokio::sync::oneshot::Receiver<Result<Reply>>)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, Pending { waiter: Waiter::Async(tx), sink: None, live: None, events: Some(events) });
        if let Err(e) = self.send(&ToHost::Call { id, call }) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    fn sink_event(&self, id: u64, f: impl FnOnce(&SinkTarget) -> std::io::Result<()>) {
        let target = self.pending.lock().unwrap().get(&id).and_then(|p| p.sink.clone());
        let Some(t) = target else { return };
        if t.error.lock().unwrap().is_some() {
            return;
        }
        if let Err(e) = f(&t) {
            *t.error.lock().unwrap() = Some(e.to_string());
            let _ = self.send(&ToHost::SinkFailed { id, error: e.to_string() });
        }
    }

    fn send(&self, msg: &ToHost) -> Result<()> {
        if !self.is_alive() {
            return Err(dead(&self.package));
        }
        let mut w = self.stdin.lock().unwrap();
        write_frame(&mut *w, msg).map_err(|_| {
            self.alive.store(false, Ordering::Relaxed);
            dead(&self.package)
        })
    }

    fn live(&self, id: u64) -> Option<Live> {
        self.pending.lock().unwrap().get(&id).and_then(|p| p.live.clone())
    }

    fn register(&self, waiter: Waiter, sink: Option<SinkTarget>, live: Option<Live>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, Pending { waiter, sink, live, events: None });
        id
    }

    async fn call_with(&self, call: Call, sink: Option<SinkTarget>) -> Result<Reply> {
        self.call_live(call, sink, None).await
    }

    async fn call_live(&self, call: Call, sink: Option<SinkTarget>, live: Option<Live>) -> Result<Reply> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.register(Waiter::Async(tx), sink, live);
        // Dropped before the reply (the app gave up on it): stop it there too.
        struct Guard<'a> {
            host: &'a Host,
            id: u64,
            done: bool,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                if !self.done && self.host.pending.lock().unwrap().remove(&self.id).is_some() {
                    let _ = self.host.send(&ToHost::Abort { id: self.id });
                }
            }
        }
        let mut guard = Guard { host: self, id, done: false };
        if let Err(e) = self.send(&ToHost::Call { id, call }) {
            self.pending.lock().unwrap().remove(&id);
            guard.done = true;
            return Err(e);
        }
        let r = rx.await.unwrap_or_else(|_| Err(dead(&self.package)));
        guard.done = true;
        r
    }

    pub async fn call(&self, call: Call) -> Result<Reply> {
        self.call_with(call, None).await
    }

    /// For the contract's synchronous methods (`table_ddl`, `browse_query`…):
    /// waits on this thread, never on the async runtime.
    pub fn call_blocking(&self, call: Call) -> Result<Reply> {
        let (tx, rx) = std::sync::mpsc::channel();
        let id = self.register(Waiter::Blocking(tx), None, None);
        if let Err(e) = self.send(&ToHost::Call { id, call }) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        rx.recv().unwrap_or_else(|_| Err(dead(&self.package)))
    }
}

impl Host {
    /// Run a call that takes the app's batches (`BulkLoad`, `DeltaApply`):
    /// send them as the host takes them (at most `BATCH_WINDOW` ahead), pass
    /// its progress on, and return its reply.
    async fn with_batches(&self, call: Call, source: &mut dyn BatchSource, progress: transfer::Progress<'_>) -> Result<Reply> {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (id, mut reply) = self.start(call, etx)?;
        let mut guard = AbortGuard { host: self, id, done: false };
        let (mut credits, mut sent_all) = (BATCH_WINDOW, false);
        let r = loop {
            tokio::select! {
                biased;
                r = &mut reply => {
                    guard.done = true;
                    break r.unwrap_or_else(|_| Err(dead(&self.package)));
                }
                ev = erx.recv() => match ev {
                    Some(Event::Ack) => credits += 1,
                    Some(Event::Committed(n)) => progress(n),
                    Some(_) => {}
                    None => {
                        guard.done = true;
                        break (&mut reply).await.unwrap_or_else(|_| Err(dead(&self.package)));
                    }
                },
                b = source.next(), if !sent_all && credits > 0 => {
                    let last = b.is_none();
                    if let Err(e) = self.send(&ToHost::Batch { id, batch: b }) {
                        break Err(e);
                    }
                    if last {
                        sent_all = true;
                    } else {
                        credits -= 1;
                    }
                }
            }
        };
        while let Ok(ev) = erx.try_recv() {
            if let Event::Committed(n) = ev {
                progress(n);
            }
        }
        drop(guard);
        r
    }
}

/// Stops a transfer call on the host when its task gives up before the reply.
struct AbortGuard<'a> {
    host: &'a Host,
    id: u64,
    done: bool,
}

impl Drop for AbortGuard<'_> {
    fn drop(&mut self) {
        if !self.done && self.host.pending.lock().unwrap().remove(&self.id).is_some() {
            let _ = self.host.send(&ToHost::Abort { id: self.id });
        }
    }
}

fn count(r: Result<Reply>) -> Result<u64> {
    match r? {
        Reply::Count(n) => Ok(n),
        _ => Err(unexpected()),
    }
}

/// The host and id behind a session, for a native copy.
fn remote_of(s: &mut dyn Session) -> Result<(u64, Arc<Host>)> {
    let unsupported = || Error::Unsupported("la copia directa necesita dos sesiones del mismo driver".into());
    let r = s.as_any().ok_or_else(unsupported)?.downcast_mut::<RemoteSession>().ok_or_else(unsupported)?;
    Ok((r.id, r.host.clone()))
}

fn unexpected() -> Error {
    Error::State("respuesta inesperada del driver".into())
}

type Resolve = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<PathBuf>> + Send>> + Send + Sync>;

/// A driver crate's host, started on first use and again after it dies.
/// `resolve` gives the executable (downloading it if needed).
pub struct Launcher {
    package: String,
    resolve: Resolve,
    args: Vec<String>,
    components_dir: Option<PathBuf>,
    host: tokio::sync::Mutex<Option<Arc<Host>>>,
    current: Mutex<Option<Arc<Host>>>,
}

impl Launcher {
    pub fn new(package: &str, components_dir: Option<PathBuf>, resolve: Resolve) -> Arc<Self> {
        Arc::new(Launcher { package: package.to_string(), resolve, args: Vec::new(), components_dir, host: tokio::sync::Mutex::new(None), current: Mutex::new(None) })
    }

    /// A fixed executable (tests, `DBINE_DRIVERS_DIR`).
    pub fn at(package: &str, exe: PathBuf, args: Vec<String>, components_dir: Option<PathBuf>) -> Arc<Self> {
        let resolve: Resolve = Arc::new(move || {
            let exe = exe.clone();
            Box::pin(async move { Ok(exe) })
        });
        Arc::new(Launcher { package: package.to_string(), resolve, args, components_dir, host: tokio::sync::Mutex::new(None), current: Mutex::new(None) })
    }

    pub async fn get(&self) -> Result<Arc<Host>> {
        let mut slot = self.host.lock().await;
        if let Some(h) = slot.as_ref().filter(|h| h.is_alive()) {
            return Ok(h.clone());
        }
        let exe = (self.resolve)().await?;
        let (pkg, args, dir) = (self.package.clone(), self.args.clone(), self.components_dir.clone());
        let host = tokio::task::spawn_blocking(move || Host::spawn(&pkg, &exe, &args, dir))
            .await
            .map_err(|e| Error::State(e.to_string()))??;
        *slot = Some(host.clone());
        *self.current.lock().unwrap() = Some(host.clone());
        Ok(host)
    }

    /// The running host's process id, if one is running.
    pub fn pid(&self) -> Option<u32> {
        self.current.lock().unwrap().as_ref().filter(|h| h.is_alive()).map(|h| h.pid())
    }

    /// For synchronous callers: the running host, or start one on a thread
    /// of its own.
    pub fn get_blocking(self: &Arc<Self>) -> Result<Arc<Host>> {
        if let Some(h) = self.current.lock().unwrap().as_ref().filter(|h| h.is_alive()) {
            return Ok(h.clone());
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| Error::State(e.to_string()))?;
            rt.block_on(me.get())
        })
        .join()
        .map_err(|_| Error::State("no se pudo iniciar el driver".into()))?
    }
}

/// A driver served by a host process.
pub struct RemoteDriver {
    meta: DriverMeta,
    query_help: &'static str,
    script_separator: &'static str,
    launcher: Arc<Launcher>,
}

impl RemoteDriver {
    pub fn new(meta: DriverMeta, launcher: Arc<Launcher>) -> Self {
        RemoteDriver { query_help: intern(&meta.query_help), script_separator: intern(&meta.script_separator), meta, launcher }
    }

    pub fn package(&self) -> &str {
        &self.meta.package
    }

    fn blocking(&self, call: Call) -> Result<Reply> {
        self.launcher.get_blocking()?.call_blocking(call)
    }

    fn id(&self) -> String {
        self.meta.info.id.to_string()
    }
}

#[async_trait]
impl Driver for RemoteDriver {
    fn info(&self) -> &DriverInfo {
        &self.meta.info
    }
    fn query_help(&self) -> &'static str {
        self.query_help
    }
    fn supports_explain(&self) -> bool {
        self.meta.supports_explain
    }
    fn supports_profiler(&self) -> bool {
        self.meta.supports_profiler
    }
    fn supports_index_usage(&self) -> bool {
        self.meta.supports_index_usage
    }
    fn supports_index_toggle(&self) -> bool {
        self.meta.supports_index_toggle
    }
    fn key_search(&self) -> Option<KeySearch> {
        self.meta.key_search.clone()
    }
    fn capabilities(&self) -> Capabilities {
        self.meta.capabilities
    }
    fn designer(&self) -> Option<DesignerSpec> {
        self.meta.designer.clone()
    }
    fn create_templates(&self) -> Vec<CreateTemplate> {
        self.meta.create_templates.clone()
    }
    fn supports_schema_sync(&self) -> bool {
        self.meta.supports_schema_sync
    }
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        self.meta.script_dialect.unwrap_or_else(|| dbine_driver::ScriptDialect::for_hint(self.meta.info.dialect))
    }
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        self.meta.script_mode.unwrap_or_default()
    }
    /// The host's own cut (a driver may override `split_script`); a host
    /// that can't answer (published before the call, or failing) leaves it
    /// to the dialect, as before.
    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        match self.blocking(Call::SplitScript { driver: self.id(), text: text.to_string() }) {
            Ok(Reply::Units(units)) => units,
            _ => dbine_driver::sql::split_script(text, &self.script_dialect()),
        }
    }
    fn script_defaults(&self) -> dbine_driver::ScriptDefaults {
        self.meta.script_defaults.unwrap_or_else(|| dbine_driver::ScriptDefaults::for_language(self.meta.info.language))
    }
    fn supports_manual_transactions(&self) -> bool {
        self.meta.supports_manual_transactions
    }
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        self.meta.security.clone()
    }
    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        text(self.blocking(Call::SecurityScript { driver: self.id(), action: action.clone() })?)
    }
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        self.meta.backup.clone()
    }
    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        text(self.blocking(Call::BackupScript { driver: self.id(), action: action.clone() })?)
    }
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        self.meta.schema_spec.clone()
    }
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        self.meta.create_database_fields.clone()
    }
    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        text(self.blocking(Call::CreateDatabaseScript { driver: self.id(), name: name.to_string(), options: options.clone() })?)
    }
    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        text(self.blocking(Call::AlterDatabaseScript { driver: self.id(), database: database.to_string(), changes: changes.clone() })?)
    }
    fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        let database = database.map(str::to_string);
        text(self.blocking(Call::CreateSchemaScript { driver: self.id(), name: name.to_string(), owner: owner.map(str::to_string), database })?)
    }
    fn schema_owner_script(&self, database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        let call = Call::SchemaOwnerScript { driver: self.id(), database: database.map(str::to_string), name: name.to_string(), owner: owner.to_string() };
        match self.blocking(call) {
            Ok(Reply::MaybeText(s)) => Ok(s),
            // A host built before the call: the owner goes in the create, as it did there.
            Err(Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    fn schema_grant_script(&self, database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
        let call = Call::SchemaGrantScript {
            driver: self.id(),
            database: database.map(str::to_string),
            name: name.to_string(),
            privileges: privileges.to_vec(),
            to: to.to_string(),
            grantable,
        };
        match self.blocking(call) {
            Ok(r) => text(r),
            // A host built before the call (or a driver without schema grants,
            // which answers the same through `SecurityScript`).
            Err(Error::Unsupported(_)) => {
                let object = ObjectRef { kind: "schema".into(), schema: None, name: name.to_string() };
                let action = dbine_driver::SecurityAction::Grant { privileges: privileges.to_vec(), object: Some(object), to: to.to_string(), grantable };
                self.security_script(&action)
            }
            Err(e) => Err(e),
        }
    }
    fn drop_schema_script(&self, database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        let database = database.map(str::to_string);
        text(self.blocking(Call::DropSchemaScript { driver: self.id(), name: name.to_string(), cascade, database })?)
    }
    fn script_separator(&self) -> &'static str {
        self.script_separator
    }
    fn sync_script(&self, changes: &[TableChange]) -> Result<SyncScript> {
        match self.blocking(Call::SyncScript { driver: self.id(), changes: changes.to_vec() })? {
            Reply::Sync(s) => Ok(s),
            _ => Err(unexpected()),
        }
    }
    fn index_toggle_script(&self, table: &ObjectRef, index: &dbine_driver::IndexUsage, enable: bool) -> Result<SyncScript> {
        match self.blocking(Call::IndexToggleScript { driver: self.id(), table: table.clone(), index: index.clone(), enable })? {
            Reply::Sync(s) => Ok(s),
            _ => Err(unexpected()),
        }
    }
    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        text(self.blocking(Call::TableDdl { driver: self.id(), table: table.clone(), parts })?)
    }
    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        text(self.blocking(Call::InsertScript { driver: self.id(), target: target.clone(), columns: columns.to_vec(), rows: rows.to_vec() })?)
    }
    fn filtered_browse(&self, browse: &str, filters: &[ColumnFilter]) -> Result<String> {
        text(self.blocking(Call::FilteredBrowse { driver: self.id(), browse: browse.to_string(), filters: filters.to_vec() })?)
    }
    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        text(self.blocking(Call::UpdateScript { driver: self.id(), target: target.clone(), changes: changes.to_vec() })?)
    }
    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        text(self.blocking(Call::DeleteScript { driver: self.id(), target: target.clone(), keys: keys.to_vec() })?)
    }
    fn data_load_wrap(&self, table: &TableSchema) -> (String, String) {
        match self.blocking(Call::DataLoadWrap { driver: self.id(), table: table.clone() }) {
            Ok(Reply::Pair(a, b)) => (a, b),
            Ok(_) => (String::new(), String::new()),
            Err(e) => {
                tracing::warn!("data_load_wrap de {}: {e}", self.id());
                (String::new(), String::new())
            }
        }
    }
    fn supports_bulk_load(&self) -> bool {
        self.meta.supports_bulk_load
    }
    /// Only within its host: `copy_native` checks that both sessions are there.
    fn supports_native_copy(&self, _target: &str) -> bool {
        self.meta.native_copy
    }
    async fn copy_native(&self, source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: transfer::Progress<'_>) -> Result<u64> {
        let (from, host) = remote_of(source)?;
        let (to, other) = remote_of(target)?;
        if !Arc::ptr_eq(&host, &other) {
            return Err(Error::Unsupported("la copia directa necesita que origen y destino usen el mismo driver".into()));
        }
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (id, mut reply) = host.start(Call::CopyNative { driver: self.id(), from, to, spec: spec.clone() }, etx)?;
        let mut guard = AbortGuard { host: &host, id, done: false };
        let r = loop {
            tokio::select! {
                biased;
                r = &mut reply => break r.unwrap_or_else(|_| Err(dead(&host.package))),
                ev = erx.recv() => match ev {
                    Some(Event::Committed(n)) => progress(n),
                    Some(_) => {}
                    None => break (&mut reply).await.unwrap_or_else(|_| Err(dead(&host.package))),
                },
            }
        };
        guard.done = true;
        while let Ok(ev) = erx.try_recv() {
            if let Event::Committed(n) = ev {
                progress(n);
            }
        }
        count(r)
    }
    fn supports_clone(&self) -> bool {
        self.meta.supports_clone
    }
    async fn clone_script(&self, source: &mut dyn Session, target: &mut dyn Session, tables: &[ObjectRef]) -> Result<CloneScript> {
        let (from, host) = remote_of(source)?;
        let (to, other) = remote_of(target)?;
        if !Arc::ptr_eq(&host, &other) {
            return Err(Error::Unsupported("clonar necesita que origen y destino usen el mismo driver".into()));
        }
        match host.call(Call::CloneScript { driver: self.id(), from, to, tables: tables.to_vec() }).await? {
            Reply::Clone(c) => Ok(c),
            _ => Err(unexpected()),
        }
    }
    fn supports_delta(&self) -> bool {
        self.meta.supports_delta
    }
    fn delta_filter(&self, spec: &DeltaSpec, buckets: &[i64]) -> Result<String> {
        text(self.blocking(Call::DeltaFilter { driver: self.id(), spec: spec.clone(), buckets: buckets.to_vec() })?)
    }
    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let host = self.launcher.get().await?;
        match host.call(Call::Connect { driver: self.id(), config: cfg.clone(), database: database.map(String::from) }).await? {
            Reply::Session { id, interruptible } => Ok(Box::new(RemoteSession { host, id, interruptible })),
            _ => Err(unexpected()),
        }
    }
}

fn text(r: Reply) -> Result<String> {
    match r {
        Reply::Text(s) => Ok(s),
        _ => Err(unexpected()),
    }
}

/// A fallback read's batches on their way from the host's reader thread to
/// the task that reads: never blocks.
struct Relay {
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
    /// The app's sink failed: refuse the rest.
    stop: Arc<Mutex<Option<String>>>,
}

impl Relay {
    fn push(&self, ev: Event) -> std::io::Result<()> {
        if let Some(e) = self.stop.lock().unwrap().clone() {
            return Err(std::io::Error::other(e));
        }
        self.tx.send(ev).map_err(|_| std::io::Error::other("la lectura terminó"))
    }
}

impl transfer::BatchSink for Relay {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.push(Event::Begin(columns.to_vec()))
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.push(Event::Batch(batch))
    }
}

/// A session living in a host.
pub struct RemoteSession {
    host: Arc<Host>,
    id: u64,
    interruptible: bool,
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        let _ = self.host.send(&ToHost::Close { session: self.id });
    }
}

impl RemoteSession {
    /// A batch or its columns reaching the app's sink; each batch is
    /// acknowledged so the host sends the next.
    fn feed(&self, id: u64, ev: Event, sink: &BatchSinkRef, failed: &mut Option<String>, begun: &mut bool) {
        let r = match ev {
            Event::Begin(cols) => {
                *begun = true;
                if failed.is_some() {
                    return;
                }
                sink.lock().map_err(|_| std::io::Error::other("destino de lotes")).and_then(|mut s| s.begin(&cols))
            }
            Event::Batch(b) => {
                let r = if failed.is_some() { Ok(()) } else { sink.lock().map_err(|_| std::io::Error::other("destino de lotes")).and_then(|mut s| s.batch(b)) };
                let _ = self.host.send(&ToHost::BatchAck { id });
                r
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            if failed.is_none() {
                *failed = Some(e.to_string());
                let _ = self.host.send(&ToHost::SinkFailed { id, error: e.to_string() });
            }
        }
    }

    /// A batched read through `execute`, for hosts published before
    /// `ReadBatches`. Its rows arrive on the host's reader thread, which
    /// must never wait on the app's sink: every reply of this host goes
    /// through it (the target's, when both ends of a copy use the same
    /// driver), and the sink may be waiting for that target. So the batches
    /// are queued there and handed to the sink here, by the task that asked
    /// for them. Those hosts have no window for a run's rows, so the queue
    /// is unbounded: a newer host doesn't take this path.
    async fn read_via_execute(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(Mutex::new(None));
        let relay: BatchSinkRef = Arc::new(Mutex::new(Relay { tx, stop: stop.clone() }));
        let mut failed = None;
        let give = |ev: Event, failed: &mut Option<String>| {
            if failed.is_some() {
                return;
            }
            let r = sink.lock().map_err(|_| std::io::Error::other("destino de lotes")).and_then(|mut s| match ev {
                Event::Begin(cols) => s.begin(&cols),
                Event::Batch(b) => s.batch(b),
                _ => Ok(()),
            });
            if let Err(e) = r {
                // The relay refuses the rest, so the host stops sending them.
                *stop.lock().unwrap() = Some(e.to_string());
                *failed = Some(e.to_string());
            }
        };
        let r = {
            let read = transfer::read_via_execute(&mut *self, spec, relay);
            tokio::pin!(read);
            let mut open = true;
            loop {
                tokio::select! {
                    biased;
                    ev = rx.recv(), if open => match ev {
                        Some(ev) => give(ev, &mut failed),
                        None => open = false,
                    },
                    r = &mut read => break r,
                }
            }
        };
        while let Ok(ev) = rx.try_recv() {
            give(ev, &mut failed);
        }
        match failed {
            Some(e) => Err(Error::Query(e)),
            None => r,
        }
    }

    async fn run(&self, call: Call, out: &mut QueryOutcome) -> Result<()> {
        let sink = out.sink.clone().map(|sink| SinkTarget { sink, base: out.sink_base + out.results.len(), error: Arc::new(Mutex::new(None)) });
        let error_slot = sink.as_ref().map(|s| s.error.clone());
        let live = (out.message_sink.is_some() || out.progress_sink.is_some())
            .then(|| Live { messages: out.message_sink.clone(), progress: out.progress_sink.clone() });
        let reply = self.host.call_live(call, sink, live).await?;
        let Reply::Run(mut o, err) = reply else { return Err(unexpected()) };
        out.elapsed_ms += o.elapsed_ms;
        if out.error.is_none() {
            out.error = o.error.take();
        }
        out.absorb(o);
        if out.sink_error.is_none() {
            out.sink_error = error_slot.and_then(|e| e.lock().unwrap().clone());
        }
        match err {
            Some(w) => Err(Error::from(w)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl Session for RemoteSession {
    async fn server_version(&mut self) -> Result<String> {
        text(self.host.call(Call::ServerVersion { session: self.id }).await?)
    }
    async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self.host.call(Call::ListDatabases { session: self.id }).await? {
            Reply::Texts(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        match self.host.call(Call::ListObjects { session: self.id }).await? {
            Reply::Objects(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        match self.host.call(Call::Columns { session: self.id, obj: obj.clone() }).await? {
            Reply::Columns(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        match self.host.call(Call::Definition { session: self.id, obj: obj.clone() }).await? {
            Reply::MaybeText(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        match self.host.call_blocking(Call::BrowseQuery { session: self.id, obj: obj.clone(), limit }) {
            Ok(Reply::Text(s)) => s,
            Ok(_) => String::new(),
            Err(e) => {
                tracing::warn!("browse_query: {e}");
                String::new()
            }
        }
    }
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let call = Call::Execute {
            session: self.id,
            text: text.to_string(),
            max_rows: max_rows as u64,
            sink: out.sink.is_some(),
            continue_on_error: out.continue_on_error,
            live: out.message_sink.is_some() || out.progress_sink.is_some(),
        };
        self.run(call, out).await
    }
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let call = Call::Explain { session: self.id, text: text.to_string(), analyze, max_rows: max_rows as u64, sink: out.sink.is_some() };
        self.run(call, out).await
    }
    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        if !self.interruptible {
            return None;
        }
        let (host, session) = (self.host.clone(), self.id);
        Some(Arc::new(move || {
            let _ = host.send(&ToHost::Cancel { session });
        }))
    }
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        match self.host.call(Call::DatabaseSchema { session: self.id }).await? {
            Reply::Schema(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.host.call(Call::CreateDatabase { session: self.id, name: name.to_string() }).await.map(|_| ())
    }
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.host.call(Call::DropDatabase { session: self.id, name: name.to_string() }).await.map(|_| ())
    }
    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        match self.host.call(Call::Monitor { session: self.id }).await? {
            Reply::Monitor(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        match self.host.call(Call::Blocking { session: self.id }).await? {
            Reply::Blocking(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        match self.host.call(Call::Principals { session: self.id }).await? {
            Reply::Principals(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        match self.host.call(Call::Grants { session: self.id, principal: principal.to_string() }).await? {
            Reply::Grants(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        match self.host.call(Call::Backups { session: self.id, database: database.map(str::to_string) }).await? {
            Reply::Backups(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn kill_session(&mut self, id: &str) -> Result<()> {
        self.host.call(Call::KillSession { session: self.id, id: id.to_string() }).await.map(|_| ())
    }
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        match self.host.call(Call::Processes { session: self.id }).await? {
            Reply::Processes(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        match self.host.call(Call::CreateDatabaseChoices { session: self.id }).await? {
            Reply::Choices(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.host.call(Call::CreateDatabaseWith { session: self.id, name: name.to_string(), options: options.clone() }).await.map(|_| ())
    }
    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        match self.host.call(Call::DatabaseProperties { session: self.id, database: database.to_string() }).await? {
            Reply::DatabaseProperties(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.host.call(Call::AlterDatabase { session: self.id, database: database.to_string(), changes: changes.clone() }).await.map(|_| ())
    }
    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.host.call(Call::CancelQuery { session: self.id, id: id.to_string() }).await.map(|_| ())
    }
    async fn profiler_start(&mut self, opts: &ProfilerOptions) -> Result<ProfilerStarted> {
        match self.host.call(Call::ProfilerStart { session: self.id, opts: opts.clone() }).await? {
            Reply::ProfilerStarted(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn profiler_poll(&mut self) -> Result<Vec<ProfiledStatement>> {
        match self.host.call(Call::ProfilerPoll { session: self.id }).await? {
            Reply::Profiled(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn profiler_stop(&mut self) -> Result<()> {
        self.host.call(Call::ProfilerStop { session: self.id }).await.map(|_| ())
    }
    async fn scan_keys(&mut self, scan: &KeyScan) -> Result<KeyPage> {
        match self.host.call(Call::ScanKeys { session: self.id, scan: scan.clone() }).await? {
            Reply::Keys(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn read_batches(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (id, mut reply) = self.host.start(Call::ReadBatches { session: self.id, spec: spec.clone() }, etx)?;
        let mut guard = AbortGuard { host: &self.host, id, done: false };
        let (mut failed, mut begun) = (None, false);
        let r = loop {
            tokio::select! {
                biased;
                ev = erx.recv() => match ev {
                    Some(ev) => self.feed(id, ev, &sink, &mut failed, &mut begun),
                    None => break (&mut reply).await.unwrap_or_else(|_| Err(dead(&self.host.package))),
                },
                r = &mut reply => break r.unwrap_or_else(|_| Err(dead(&self.host.package))),
            }
        };
        guard.done = true;
        while let Ok(ev) = erx.try_recv() {
            self.feed(id, ev, &sink, &mut failed, &mut begun);
        }
        drop(guard);
        match r {
            // A driver published before batched reads: read through `execute`.
            Err(Error::Unsupported(_)) if !begun && spec.filter.is_none() => self.read_via_execute(spec, sink).await,
            r => {
                let n = count(r)?;
                match failed {
                    Some(e) => Err(Error::Query(e)),
                    None => Ok(n),
                }
            }
        }
    }
    /// Only for drivers whose manifest says `supports_bulk_load` (an older
    /// host wouldn't understand the batches).
    async fn bulk_load(&mut self, spec: &LoadSpec, columns: &[TransferColumn], source: &mut dyn BatchSource, progress: transfer::Progress<'_>) -> Result<u64> {
        count(self.host.with_batches(Call::BulkLoad { session: self.id, spec: spec.clone(), columns: columns.to_vec() }, source, progress).await)
    }
    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        match self.host.call(Call::KeyRange { session: self.id, table: table.clone(), column: column.to_string() }).await? {
            Reply::KeyRange(r) => Ok(r),
            _ => Err(unexpected()),
        }
    }
    async fn delta_summary(&mut self, spec: &DeltaSpec) -> Result<Vec<BucketSum>> {
        match self.host.call(Call::DeltaSummary { session: self.id, spec: spec.clone() }).await? {
            Reply::Buckets(v) => Ok(v),
            _ => Err(unexpected()),
        }
    }
    async fn delta_apply(
        &mut self,
        spec: &DeltaSpec,
        buckets: &[i64],
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: transfer::Progress<'_>,
    ) -> Result<DeltaResult> {
        let call = Call::DeltaApply { session: self.id, spec: spec.clone(), buckets: buckets.to_vec(), columns: columns.to_vec() };
        match self.host.with_batches(call, source, progress).await? {
            Reply::Delta(r) => Ok(r),
            _ => Err(unexpected()),
        }
    }
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        match self.host.call(Call::Permissions { session: self.id, database: database.map(str::to_string) }).await {
            Ok(Reply::Permissions(p)) => Ok(p),
            // A host built before the check: nothing known, everything stays on.
            Err(Error::Unsupported(_)) => Ok(dbine_driver::Permissions::default()),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn transaction_state(&mut self) -> Result<Option<dbine_driver::TxState>> {
        match self.host.call(Call::TransactionState { session: self.id }).await {
            Ok(Reply::TxState(t)) => Ok(t),
            // A host built before the call: not tracked.
            Err(Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        match self.host.call(Call::SetAutocommit { session: self.id, on }).await {
            Ok(_) => Ok(()),
            // A host built before the call: its sessions only autocommit.
            Err(Error::Unsupported(_)) if on => Ok(()),
            Err(e) => Err(e),
        }
    }
    async fn commit(&mut self) -> Result<()> {
        self.host.call(Call::Commit { session: self.id }).await.map(|_| ())
    }
    async fn rollback(&mut self) -> Result<()> {
        self.host.call(Call::Rollback { session: self.id }).await.map(|_| ())
    }
    async fn health_checks(&mut self, database: &str) -> Result<Vec<dbine_driver::health::HealthCheck>> {
        match self.host.call(Call::HealthChecks { session: self.id, database: database.to_string() }).await {
            Ok(Reply::HealthChecks(v)) => Ok(v),
            // A host built before the call: no checks of its own.
            Err(Error::Unsupported(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn search_code(&mut self, query: &dbine_driver::search::CodeSearch) -> Result<Option<dbine_driver::search::CodeSearchReport>> {
        match self.host.call(Call::SearchCode { session: self.id, query: query.clone() }).await {
            Ok(Reply::CodeSearch(r)) => Ok(r),
            // A host built before the call: the app scans the definitions.
            Err(Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        match self.host.call(Call::IndexUsage { session: self.id, table: table.clone() }).await {
            Ok(Reply::IndexUsage(r)) => Ok(r),
            // A host built before the call: not reported.
            Err(Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn dependents(&mut self, target: &dbine_driver::DependencyTarget, scan: &dbine_driver::DependencyScan) -> Result<dbine_driver::DependencyReport> {
        match self.host.call(Call::Dependents { session: self.id, target: target.clone(), scan: scan.clone() }).await {
            Ok(Reply::Dependents(r)) => Ok(r),
            // A host built before the call: the generic scan, through its other calls.
            Err(Error::Unsupported(_)) => dbine_driver::dependencies::scan(self, target, scan).await,
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        match self.host.call(Call::ListSchemas { session: self.id }).await {
            Ok(Reply::Schemas(v)) => Ok(v),
            // A host built before the call: the explorer derives the schemas from the objects.
            Err(Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => Err(unexpected()),
        }
    }
    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::proto::read_frame;
    use dbine_driver::transfer::BatchSink;
    use dbine_driver::ResultColumn;
    use std::os::unix::net::UnixStream;
    use std::sync::Condvar;
    use std::time::Duration;

    type Out = Arc<Mutex<UnixStream>>;

    fn reply(out: &Out, id: u64, result: std::result::Result<Reply, WireError>) {
        write_frame(&mut *out.lock().unwrap(), &FromHost::Reply { id, result }).unwrap();
    }

    /// A host published before `ReadBatches`: `cat` echoes the app's calls
    /// back to this thread, which answers them through `out`.
    fn old_host(rows: u64) -> Arc<Host> {
        let mut child = Command::new("cat").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let mut calls = BufReader::new(child.stdout.take().unwrap());
        let (theirs, ours) = UnixStream::pair().unwrap();
        let out: Out = Arc::new(Mutex::new(ours));
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = read_frame::<ToHost>(&mut calls) {
                let ToHost::Call { id, call } = msg else { continue };
                match call {
                    Call::ReadBatches { .. } => reply(&out, id, Err(WireError::from(&Error::Unsupported("versión vieja".into())))),
                    Call::BrowseQuery { .. } => reply(&out, id, Ok(Reply::Text("SELECT * FROM t".into()))),
                    Call::ServerVersion { .. } => reply(&out, id, Ok(Reply::Text("1".into()))),
                    Call::Execute { .. } => {
                        let out = out.clone();
                        std::thread::spawn(move || {
                            let columns = vec![ResultColumn { name: "id".into(), type_name: String::new() }];
                            let _ = write_frame(&mut *out.lock().unwrap(), &FromHost::SinkBegin { id, index: 0, columns });
                            for i in 0..rows {
                                let row = FromHost::SinkRow { id, index: 0, row: vec![serde_json::json!(i)] };
                                if write_frame(&mut *out.lock().unwrap(), &row).is_err() {
                                    return;
                                }
                            }
                            reply(&out, id, Ok(Reply::Run(QueryOutcome::default(), None)));
                        });
                    }
                    _ => {}
                }
            }
        });
        Host::attach("viejo", stdin, BufReader::new(theirs), child)
    }

    /// A sink that holds its first batch until the test lets it go, the way
    /// a migration's window holds the reader while the target writes.
    #[derive(Default)]
    struct Gate {
        state: Mutex<(bool, bool)>,
        cv: Condvar,
    }

    struct Held {
        gate: Arc<Gate>,
        rows: usize,
    }

    impl BatchSink for Held {
        fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
            Ok(())
        }
        fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
            let mut s = self.gate.state.lock().unwrap();
            s.0 = true;
            self.gate.cv.notify_all();
            while !s.1 {
                s = self.gate.cv.wait(s).unwrap();
            }
            self.rows += b.len();
            Ok(())
        }
    }

    /// The fallback read of an old host must not wait on the app's sink in
    /// the host's reader thread: another call on the same host (the
    /// target's insert, in a copy between two connections of one driver)
    /// still gets its reply while the sink is full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_old_hosts_fallback_read_leaves_the_reader_free() {
        let rows = 5_000;
        let host = old_host(rows);
        let mut reader = RemoteSession { host: host.clone(), id: 1, interruptible: false };
        let mut other = RemoteSession { host: host.clone(), id: 2, interruptible: false };
        let gate = Arc::new(Gate::default());
        let held = Arc::new(Mutex::new(Held { gate: gate.clone(), rows: 0 }));
        let sink: BatchSinkRef = held.clone();
        let read = tokio::spawn(async move {
            let spec = ReadSpec { table: ObjectRef { kind: "table".into(), schema: None, name: "t".into() }, columns: None, filter: None };
            reader.read_batches(&spec, sink).await
        });
        {
            let g = gate.clone();
            tokio::task::spawn_blocking(move || {
                let mut s = g.state.lock().unwrap();
                while !s.0 {
                    s = g.cv.wait(s).unwrap();
                }
            })
            .await
            .unwrap();
        }

        let answered = tokio::time::timeout(Duration::from_secs(5), other.server_version()).await;
        // Let the sink go before asserting, so a failure doesn't hang the test.
        gate.state.lock().unwrap().1 = true;
        gate.cv.notify_all();
        assert!(matches!(answered, Ok(Ok(ref v)) if v == "1"), "the other call got no reply: {answered:?}");
        assert_eq!(read.await.unwrap().unwrap(), rows);
        assert_eq!(held.lock().unwrap().rows as u64, rows);
    }
}

//! The host side: a driver crate as its own process, serving the app over
//! stdin / stdout (`dbine-plugin-host`). One host serves every session of
//! its drivers, each call in its own task, so a long query doesn't hold the
//! others and a cancel reaches the session while it runs.

use crate::proto::{
    read_frame, read_raw, unknown_call, write_frame, Call, DriverMeta, FromHost, Hello, Ready,
    Reply, ToHost, WireError, BATCH_WINDOW, PROTOCOL,
};
use dbine_driver::transfer::{BatchSink, BatchSinkRef, BatchSource, RowBatch, TransferColumn};
use dbine_driver::{
    Driver, Error, MessageSinkRef, ProgressSinkRef, QueryOutcome, ResultColumn, RowSink,
    RowSinkRef, Session,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufWriter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use tokio::task::AbortHandle;

/// Frames waiting for the writer: a full queue makes producers wait, so a
/// fast query doesn't outrun a slow export sink on the app side.
const QUEUE: usize = 256;

type Interrupter = Arc<dyn Fn() + Send + Sync>;

struct Slot {
    session: tokio::sync::Mutex<Box<dyn Session>>,
    /// Outside the session's lock: it has to work while `execute` holds it.
    interrupter: Option<Interrupter>,
}

struct CallCtl {
    abort: AbortHandle,
    session: Option<u64>,
    /// Set when the app's row sink failed: the rest of the rows are dropped.
    sink_failed: Arc<Mutex<Option<String>>>,
    /// A `ReadBatches` call's window (the app's `BatchAck`s refill it).
    credits: Option<Arc<Credits>>,
    /// A `BulkLoad` call's incoming batches.
    inbox: Option<tokio::sync::mpsc::UnboundedSender<Option<RowBatch>>>,
}

/// What a transfer call gets besides its message.
#[derive(Default)]
struct CallIo {
    credits: Option<Arc<Credits>>,
    inbox: Option<tokio::sync::mpsc::UnboundedReceiver<Option<RowBatch>>>,
}

/// Batches a `ReadBatches` call may still send before the app takes some.
struct Credits {
    n: Mutex<usize>,
    cv: Condvar,
}

impl Credits {
    fn new() -> Self {
        Credits {
            n: Mutex::new(BATCH_WINDOW),
            cv: Condvar::new(),
        }
    }
    fn release(&self) {
        *self.n.lock().unwrap() += 1;
        self.cv.notify_one();
    }
    /// Wait for room; stops when the call failed or was cancelled. The wait
    /// happens off the runtime's workers (see [`off_worker`]).
    fn acquire(&self, failed: &Mutex<Option<String>>) -> io::Result<()> {
        {
            let mut n = self.n.lock().unwrap();
            if *n > 0 && failed.lock().unwrap().is_none() {
                *n -= 1;
                return Ok(());
            }
        }
        off_worker(|| self.wait(failed))
    }
    fn wait(&self, failed: &Mutex<Option<String>>) -> io::Result<()> {
        let mut n = self.n.lock().unwrap();
        loop {
            if let Some(e) = failed.lock().unwrap().clone() {
                return Err(io::Error::other(e));
            }
            if *n > 0 {
                *n -= 1;
                return Ok(());
            }
            n = self
                .cv
                .wait_timeout(n, std::time::Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }
}

/// Run a wait that may be long (the app's `BatchAck`, room in the writer's
/// queue) without holding a runtime worker. Drivers call the sinks
/// synchronously from their async code: a worker parked there can't run
/// the tasks whose progress would end the wait (a bulk load on this same
/// host sending the acks the app's reader is waiting for), and with as many
/// parked reads as workers the host would hang.
fn off_worker<R>(f: impl FnOnce() -> R) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        // A driver's own current-thread runtime, or no runtime at all.
        _ => f(),
    }
}

/// Queue a frame for the writer; a full queue is waited for off the workers.
fn send(tx: &SyncSender<FromHost>, msg: FromHost) -> io::Result<()> {
    use std::sync::mpsc::TrySendError;
    let closed = || io::Error::other("la app cerró el canal");
    match tx.try_send(msg) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(msg)) => off_worker(|| tx.send(msg).is_ok())
            .then_some(())
            .ok_or_else(closed),
        Err(TrySendError::Disconnected(_)) => Err(closed()),
    }
}

struct State {
    package: String,
    drivers: Vec<Arc<dyn Driver>>,
    sessions: Mutex<HashMap<u64, Arc<Slot>>>,
    next_session: AtomicU64,
    calls: Mutex<HashMap<u64, CallCtl>>,
    tx: SyncSender<FromHost>,
}

/// Serve `drivers` (the crate `package`) until the app closes stdin.
/// Never returns.
pub fn run(package: &str, drivers: Vec<Arc<dyn Driver>>) -> ! {
    // Only protocol frames may reach the real stdout: C libraries and stray
    // prints go to stderr, which the app logs.
    let proto_out = take_stdout();
    std::panic::set_hook(Box::new(|info| eprintln!("panic: {info}")));
    let code = serve(package, drivers, proto_out);
    std::process::exit(code)
}

fn serve(package: &str, drivers: Vec<Arc<dyn Driver>>, proto_out: std::fs::File) -> i32 {
    let mut out = BufWriter::new(proto_out);
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let hello: Hello = match read_frame(&mut input) {
        Ok(Some(h)) => h,
        _ => return 1,
    };
    if hello.protocol != PROTOCOL {
        eprintln!(
            "protocolo {} pedido; este driver habla el {PROTOCOL}",
            hello.protocol
        );
        return 2;
    }
    if let Some(dir) = hello.components_dir.filter(|d| !d.is_empty()) {
        dbine_driver::runtime::set_components_dir(dir.into());
    }
    let version = option_env!("DBINE_DRIVER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let ready = Ready {
        protocol: PROTOCOL,
        version: version.into(),
        drivers: drivers.iter().map(|d| d.info().id.to_string()).collect(),
    };
    if write_frame(&mut out, &ready).is_err() {
        return 1;
    }

    let (tx, rx) = sync_channel::<FromHost>(QUEUE);
    let writer = std::thread::spawn(move || {
        for msg in rx {
            if write_frame(&mut out, &msg).is_err() {
                break;
            }
        }
    });
    // The sink is set once for the process; its sender is taken back at the
    // end so the writer thread can finish.
    let progress_tx = Arc::new(Mutex::new(Some(tx.clone())));
    let sink_tx = progress_tx.clone();
    dbine_driver::runtime::set_progress_sink(move |p| {
        if let Some(t) = sink_tx.lock().unwrap().as_ref() {
            let _ = send(t, FromHost::Progress(p.clone()));
        }
    });

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("no se pudo iniciar el runtime: {e}");
            return 1;
        }
    };
    let st = Arc::new(State {
        package: package.to_string(),
        drivers,
        sessions: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
        calls: Mutex::new(HashMap::new()),
        tx,
    });

    loop {
        let body = match read_raw(&mut input) {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                eprintln!("canal con la app: {e}");
                break;
            }
        };
        let msg: ToHost = match rmp_serde::from_slice(&body) {
            Ok(m) => m,
            // A newer app asking for something this driver version doesn't have.
            Err(e) => match unknown_call(&body) {
                Some((id, name)) => {
                    let error = WireError::from(&Error::Unsupported(format!(
                        "esta versión del driver no tiene «{name}»"
                    )));
                    let _ = st.tx.send(FromHost::Reply {
                        id,
                        result: Err(error),
                    });
                    continue;
                }
                // Something newer than a call this host can't follow: skip it
                // (frames are delimited, the stream stays in step).
                None => {
                    eprintln!("mensaje de la app ilegible: {e}");
                    continue;
                }
            },
        };
        match msg {
            ToHost::Call { id, call } => {
                let session = call.session();
                let sink_failed = Arc::new(Mutex::new(None));
                let task_st = st.clone();
                let task_failed = sink_failed.clone();
                let mut io = CallIo::default();
                let mut inbox = None;
                match &call {
                    Call::ReadBatches { .. } => io.credits = Some(Arc::new(Credits::new())),
                    Call::BulkLoad { .. } | Call::DeltaApply { .. } | Call::WriteRows { .. } => {
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        inbox = Some(tx);
                        io.inbox = Some(rx);
                    }
                    _ => {}
                }
                let credits = io.credits.clone();
                let handle = rt.spawn(async move {
                    let result = task_st
                        .handle(id, call, task_failed, io)
                        .await
                        .map_err(|e| WireError::from(&e));
                    task_st.calls.lock().unwrap().remove(&id);
                    let _ = send(&task_st.tx, FromHost::Reply { id, result });
                });
                if !handle.is_finished() {
                    st.calls.lock().unwrap().insert(
                        id,
                        CallCtl {
                            abort: handle.abort_handle(),
                            session,
                            sink_failed,
                            credits,
                            inbox,
                        },
                    );
                }
            }
            ToHost::Cancel { session } => {
                let slot = st.sessions.lock().unwrap().get(&session).cloned();
                if let Some(i) = slot.and_then(|s| s.interrupter.clone()) {
                    i();
                }
            }
            ToHost::Abort { id } => {
                if let Some(c) = st.calls.lock().unwrap().remove(&id) {
                    // Wakes a transfer waiting for room.
                    *c.sink_failed.lock().unwrap() = Some("cancelado".into());
                    c.abort.abort();
                }
            }
            ToHost::Close { session } => {
                // Like dropping the session in the app: whatever it runs stops.
                let running: Vec<AbortHandle> = {
                    let mut calls = st.calls.lock().unwrap();
                    let ids: Vec<u64> = calls
                        .iter()
                        .filter(|(_, c)| c.session == Some(session))
                        .map(|(id, _)| *id)
                        .collect();
                    ids.iter()
                        .filter_map(|id| calls.remove(id))
                        .map(|c| {
                            *c.sink_failed.lock().unwrap() = Some("cancelado".into());
                            c.abort
                        })
                        .collect()
                };
                for a in running {
                    a.abort();
                }
                st.sessions.lock().unwrap().remove(&session);
            }
            ToHost::SinkFailed { id, error } => {
                if let Some(c) = st.calls.lock().unwrap().get(&id) {
                    *c.sink_failed.lock().unwrap() = Some(error);
                }
            }
            ToHost::Batch { id, batch } => {
                if let Some(tx) = st
                    .calls
                    .lock()
                    .unwrap()
                    .get(&id)
                    .and_then(|c| c.inbox.clone())
                {
                    let _ = tx.send(batch);
                }
            }
            ToHost::BatchAck { id } => {
                if let Some(c) = st
                    .calls
                    .lock()
                    .unwrap()
                    .get(&id)
                    .and_then(|c| c.credits.clone())
                {
                    c.release();
                }
            }
        }
    }
    // The app is gone (or closed us): stop every call like `Close` does (a
    // read waiting for acks that will never come included), drop every
    // session, then leave.
    let calls: Vec<CallCtl> = st.calls.lock().unwrap().drain().map(|(_, c)| c).collect();
    for c in &calls {
        *c.sink_failed.lock().unwrap() = Some("la app se cerró".into());
        c.abort.abort();
    }
    drop(calls);
    st.sessions.lock().unwrap().clear();
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    drop(st);
    progress_tx.lock().unwrap().take();
    // A driver stuck in synchronous code may still hold a sender: don't wait
    // for the writer forever, the process ends anyway.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !writer.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    0
}

impl Call {
    fn session(&self) -> Option<u64> {
        use Call::*;
        match self {
            ServerVersion { session }
            | ListDatabases { session }
            | ListObjects { session }
            | Columns { session, .. }
            | Definition { session, .. }
            | BrowseQuery { session, .. }
            | Execute { session, .. }
            | Explain { session, .. }
            | DatabaseSchema { session }
            | CreateDatabase { session, .. }
            | DropDatabase { session, .. }
            | Monitor { session }
            | Blocking { session }
            | Principals { session }
            | Grants { session, .. }
            | Backups { session, .. }
            | KillSession { session, .. }
            | ProfilerStart { session, .. }
            | ProfilerPoll { session }
            | ProfilerStop { session }
            | ScanKeys { session, .. }
            | ReadBatches { session, .. }
            | BulkLoad { session, .. }
            | KeyRange { session, .. }
            | DeltaSummary { session, .. }
            | DeltaApply { session, .. }
            | WriteRows { session, .. }
            | Permissions { session, .. }
            | ListSchemas { session }
            | IndexUsage { session, .. }
            | Dependents { session, .. }
            | Processes { session }
            | CancelQuery { session, .. }
            | CreateDatabaseChoices { session }
            | CreateDatabaseWith { session, .. }
            | DatabaseProperties { session, .. }
            | AlterDatabase { session, .. }
            | SearchCode { session, .. }
            | HealthChecks { session, .. }
            | RowEstimates { session }
            | ObjectComments { session }
            | UnmappedLogins { session }
            | RunReadOnly { session, .. }
            | ExecuteBound { session, .. } => Some(*session),
            CloneScript { from, .. } => Some(*from),
            // Closing the source stops the copy (the target's close waits for it).
            CopyNative { from, .. } => Some(*from),
            _ => None,
        }
    }
}

/// Rows of a run going back to the app as they come.
struct Forwarder {
    id: u64,
    tx: SyncSender<FromHost>,
    failed: Arc<Mutex<Option<String>>>,
}

impl Forwarder {
    fn send(&self, msg: FromHost) -> io::Result<()> {
        if let Some(e) = self.failed.lock().unwrap().clone() {
            return Err(io::Error::other(e));
        }
        send(&self.tx, msg)
    }
}

impl RowSink for Forwarder {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> io::Result<()> {
        self.send(FromHost::SinkBegin {
            id: self.id,
            index: index as u64,
            columns: columns.to_vec(),
        })
    }
    fn row(&mut self, index: usize, row: &[Value]) -> io::Result<()> {
        self.send(FromHost::SinkRow {
            id: self.id,
            index: index as u64,
            row: row.to_vec(),
        })
    }
}

/// A `ReadBatches` call's batches going back to the app, at most
/// `BATCH_WINDOW` not yet taken.
struct BatchForwarder {
    id: u64,
    tx: SyncSender<FromHost>,
    credits: Arc<Credits>,
    failed: Arc<Mutex<Option<String>>>,
}

impl BatchSink for BatchForwarder {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        if let Some(e) = self.failed.lock().unwrap().clone() {
            return Err(io::Error::other(e));
        }
        send(
            &self.tx,
            FromHost::BatchBegin {
                id: self.id,
                columns: columns.to_vec(),
            },
        )
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.credits.acquire(&self.failed)?;
        send(&self.tx, FromHost::Batch { id: self.id, batch })
    }
}

/// A `BulkLoad` call's batches as they arrive from the app.
struct Inbox {
    id: u64,
    rx: tokio::sync::mpsc::UnboundedReceiver<Option<RowBatch>>,
    tx: SyncSender<FromHost>,
}

#[dbine_driver::async_trait]
impl BatchSource for Inbox {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.rx.recv().await {
            Some(Some(b)) => {
                let _ = send(&self.tx, FromHost::BatchAck { id: self.id });
                Some(b)
            }
            _ => None,
        }
    }
}

impl State {
    fn driver(&self, id: &str) -> Result<&Arc<dyn Driver>, Error> {
        self.drivers
            .iter()
            .find(|d| d.info().id == id)
            .ok_or_else(|| Error::State(format!("este proceso no sirve el driver «{id}»")))
    }

    fn slot(&self, session: u64) -> Result<Arc<Slot>, Error> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session)
            .cloned()
            .ok_or_else(|| Error::State("la sesión ya se cerró".into()))
    }

    fn outcome(&self, id: u64, sink: bool, failed: &Arc<Mutex<Option<String>>>) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        if sink {
            out.sink = Some(RowSinkRef(Arc::new(Mutex::new(Forwarder {
                id,
                tx: self.tx.clone(),
                failed: failed.clone(),
            }))));
        }
        out
    }

    async fn handle(
        &self,
        id: u64,
        call: Call,
        sink_failed: Arc<Mutex<Option<String>>>,
        mut io: CallIo,
    ) -> Result<Reply, Error> {
        Ok(match call {
            Call::Manifest => Reply::Manifest(
                self.drivers
                    .iter()
                    .map(|d| DriverMeta::of(&self.package, d.as_ref()))
                    .collect(),
            ),
            Call::Connect {
                driver,
                config,
                database,
            } => {
                let s = self
                    .driver(&driver)?
                    .connect(&config, database.as_deref())
                    .await?;
                let interrupter = s.interrupter();
                let interruptible = interrupter.is_some();
                let tls_pin_seen = s.tls_pin_seen();
                let sid = self.next_session.fetch_add(1, Ordering::Relaxed);
                self.sessions.lock().unwrap().insert(
                    sid,
                    Arc::new(Slot {
                        session: tokio::sync::Mutex::new(s),
                        interrupter,
                    }),
                );
                Reply::Session {
                    id: sid,
                    interruptible,
                    tls_pin_seen,
                }
            }
            Call::SyncScript { driver, changes } => {
                Reply::Sync(self.driver(&driver)?.sync_script(&changes)?)
            }
            Call::IndexToggleScript {
                driver,
                table,
                index,
                enable,
            } => Reply::Sync(
                self.driver(&driver)?
                    .index_toggle_script(&table, &index, enable)?,
            ),
            Call::RenameScript { driver, request } => {
                Reply::Sync(self.driver(&driver)?.rename_script(&request)?)
            }
            Call::MapLoginScript {
                driver,
                login,
                user,
                default_schema,
            } => Reply::Text(self.driver(&driver)?.map_login_script(
                &login,
                &user,
                default_schema.as_deref(),
            )?),
            Call::RenameDatabaseScript {
                driver,
                database,
                new_name,
                objects,
            } => Reply::Sync(
                self.driver(&driver)?
                    .rename_database_script(&database, &new_name, &objects)?,
            ),
            Call::TableDdl {
                driver,
                table,
                parts,
            } => Reply::Text(self.driver(&driver)?.table_ddl(&table, parts)?),
            Call::InsertScript {
                driver,
                target,
                columns,
                rows,
            } => Reply::Text(
                self.driver(&driver)?
                    .insert_script(&target, &columns, &rows)?,
            ),
            Call::FilteredBrowse {
                driver,
                browse,
                filters,
            } => Reply::Text(self.driver(&driver)?.filtered_browse(&browse, &filters)?),
            Call::ParamLiteral { driver, value } => {
                Reply::Text(self.driver(&driver)?.param_literal(&value)?)
            }
            Call::SortedBrowse {
                driver,
                query,
                sort,
            } => Reply::Text(self.driver(&driver)?.sorted_browse(&query, &sort)?),
            Call::AggregateQuery {
                driver,
                query,
                spec,
            } => Reply::Text(self.driver(&driver)?.aggregate_query(&query, &spec)?),
            Call::UpdateScript {
                driver,
                target,
                changes,
            } => Reply::Text(self.driver(&driver)?.update_script(&target, &changes)?),
            Call::DeleteScript {
                driver,
                target,
                keys,
            } => Reply::Text(self.driver(&driver)?.delete_script(&target, &keys)?),
            Call::SecurityScript { driver, action } => {
                Reply::Text(self.driver(&driver)?.security_script(&action)?)
            }
            Call::BackupScript { driver, action } => {
                Reply::Text(self.driver(&driver)?.backup_script(&action)?)
            }
            Call::CreateDatabaseScript {
                driver,
                name,
                options,
            } => Reply::Text(
                self.driver(&driver)?
                    .create_database_script(&name, &options)?,
            ),
            Call::AlterDatabaseScript {
                driver,
                database,
                changes,
            } => Reply::Text(
                self.driver(&driver)?
                    .alter_database_script(&database, &changes)?,
            ),
            Call::CreateSchemaScript {
                driver,
                name,
                owner,
                database,
            } => Reply::Text(self.driver(&driver)?.create_schema_script(
                database.as_deref(),
                &name,
                owner.as_deref(),
            )?),
            Call::DropSchemaScript {
                driver,
                name,
                cascade,
                database,
            } => Reply::Text(self.driver(&driver)?.drop_schema_script(
                database.as_deref(),
                &name,
                cascade,
            )?),
            Call::SchemaOwnerScript {
                driver,
                database,
                name,
                owner,
            } => Reply::MaybeText(self.driver(&driver)?.schema_owner_script(
                database.as_deref(),
                &name,
                &owner,
            )?),
            Call::SchemaGrantScript {
                driver,
                database,
                name,
                privileges,
                to,
                grantable,
            } => Reply::Text(self.driver(&driver)?.schema_grant_script(
                database.as_deref(),
                &name,
                &privileges,
                &to,
                grantable,
            )?),
            Call::DataLoadWrap { driver, table } => {
                let (a, b) = self.driver(&driver)?.data_load_wrap(&table);
                Reply::Pair(a, b)
            }
            Call::ServerVersion { session } => Reply::Text(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .server_version()
                    .await?,
            ),
            Call::ListDatabases { session } => Reply::Texts(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .list_databases()
                    .await?,
            ),
            Call::ListObjects { session } => Reply::Objects(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .list_objects()
                    .await?,
            ),
            Call::Columns { session, obj } => Reply::Columns(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .columns(&obj)
                    .await?,
            ),
            Call::Definition { session, obj } => Reply::MaybeText(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .definition(&obj)
                    .await?,
            ),
            Call::BrowseQuery {
                session,
                obj,
                limit,
            } => Reply::Text(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .browse_query(&obj, limit),
            ),
            Call::Execute {
                session,
                text,
                max_rows,
                sink,
                continue_on_error,
                live,
            } => {
                let slot = self.slot(session)?;
                let mut out = self.outcome(id, sink, &sink_failed);
                out.continue_on_error = continue_on_error;
                if live {
                    let tx = self.tx.clone();
                    out.message_sink = Some(MessageSinkRef(Arc::new(
                        move |m: &dbine_driver::Message| {
                            let _ = send(
                                &tx,
                                FromHost::Message {
                                    id,
                                    message: m.clone(),
                                },
                            );
                        },
                    )));
                    let tx = self.tx.clone();
                    out.progress_sink = Some(ProgressSinkRef(Arc::new(
                        move |e: &dbine_driver::StatementEnd| {
                            let _ = send(&tx, FromHost::StatementEnded { id, end: e.clone() });
                        },
                    )));
                }
                let r = slot
                    .session
                    .lock()
                    .await
                    .execute(&text, max_rows as usize, &mut out)
                    .await;
                run_reply(out, r)
            }
            Call::Explain {
                session,
                text,
                analyze,
                max_rows,
                sink,
            } => {
                let slot = self.slot(session)?;
                let mut out = self.outcome(id, sink, &sink_failed);
                let r = slot
                    .session
                    .lock()
                    .await
                    .explain(&text, analyze, max_rows as usize, &mut out)
                    .await;
                run_reply(out, r)
            }
            Call::ExecuteBound {
                session,
                statement,
                max_rows,
                sink,
            } => {
                let slot = self.slot(session)?;
                let mut out = self.outcome(id, sink, &sink_failed);
                let r = slot
                    .session
                    .lock()
                    .await
                    .execute_bound(&statement, max_rows as usize, &mut out)
                    .await;
                run_reply(out, r)
            }
            Call::RunReadOnly {
                session,
                statement,
                max_rows,
                sink,
            } => {
                let slot = self.slot(session)?;
                let mut out = self.outcome(id, sink, &sink_failed);
                let r = slot
                    .session
                    .lock()
                    .await
                    .run_read_only(&statement, max_rows as usize, &mut out)
                    .await;
                run_reply(out, r)
            }
            Call::DatabaseSchema { session } => Reply::Schema(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .database_schema()
                    .await?,
            ),
            Call::CreateDatabase { session, name } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .create_database(&name)
                    .await?;
                Reply::Unit
            }
            Call::DropDatabase { session, name } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .drop_database(&name)
                    .await?;
                Reply::Unit
            }
            Call::Monitor { session } => {
                Reply::Monitor(self.slot(session)?.session.lock().await.monitor().await?)
            }
            Call::Blocking { session } => {
                Reply::Blocking(self.slot(session)?.session.lock().await.blocking().await?)
            }
            Call::Principals { session } => Reply::Principals(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .principals()
                    .await?,
            ),
            Call::UnmappedLogins { session } => Reply::Texts(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .unmapped_logins()
                    .await?,
            ),
            Call::Grants { session, principal } => Reply::Grants(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .grants(&principal)
                    .await?,
            ),
            Call::Backups { session, database } => Reply::Backups(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .backups(database.as_deref())
                    .await?,
            ),
            Call::KillSession { session, id } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .kill_session(&id)
                    .await?;
                Reply::Unit
            }
            Call::ProfilerStart { session, opts } => Reply::ProfilerStarted(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .profiler_start(&opts)
                    .await?,
            ),
            Call::ProfilerPoll { session } => Reply::Profiled(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .profiler_poll()
                    .await?,
            ),
            Call::ProfilerStop { session } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .profiler_stop()
                    .await?;
                Reply::Unit
            }
            Call::ScanKeys { session, scan } => Reply::Keys(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .scan_keys(&scan)
                    .await?,
            ),
            Call::ReadBatches { session, spec } => {
                let slot = self.slot(session)?;
                let credits = io
                    .credits
                    .take()
                    .unwrap_or_else(|| Arc::new(Credits::new()));
                let sink: BatchSinkRef = Arc::new(Mutex::new(BatchForwarder {
                    id,
                    tx: self.tx.clone(),
                    credits,
                    failed: sink_failed.clone(),
                }));
                let n = slot.session.lock().await.read_batches(&spec, sink).await?;
                Reply::Count(n)
            }
            Call::BulkLoad {
                session,
                spec,
                columns,
            } => {
                let slot = self.slot(session)?;
                let rx = io
                    .inbox
                    .take()
                    .ok_or_else(|| Error::State("carga sin canal de lotes".into()))?;
                let mut inbox = Inbox {
                    id,
                    rx,
                    tx: self.tx.clone(),
                };
                let tx = self.tx.clone();
                let progress = move |rows: u64| {
                    let _ = send(&tx, FromHost::Committed { id, rows });
                };
                let n = slot
                    .session
                    .lock()
                    .await
                    .bulk_load(&spec, &columns, &mut inbox, &progress)
                    .await?;
                Reply::Count(n)
            }
            Call::CopyNative {
                driver,
                from,
                to,
                spec,
            } => {
                if from == to {
                    return Err(Error::State("origen y destino son la misma sesión".into()));
                }
                let d = self.driver(&driver)?.clone();
                let (a, b) = (self.slot(from)?, self.slot(to)?);
                // Always the same order, so two copies can't wait on each other.
                let (mut src, mut dst) = if from < to {
                    let src = a.session.lock().await;
                    (src, b.session.lock().await)
                } else {
                    let dst = b.session.lock().await;
                    (a.session.lock().await, dst)
                };
                let tx = self.tx.clone();
                let progress = move |rows: u64| {
                    let _ = send(&tx, FromHost::Committed { id, rows });
                };
                Reply::Count(
                    d.copy_native(&mut **src, &mut **dst, &spec, &progress)
                        .await?,
                )
            }
            Call::CloneScript {
                driver,
                from,
                to,
                tables,
            } => {
                if from == to {
                    return Err(Error::State("origen y destino son la misma sesión".into()));
                }
                let d = self.driver(&driver)?.clone();
                let (a, b) = (self.slot(from)?, self.slot(to)?);
                let (mut src, mut dst) = if from < to {
                    let src = a.session.lock().await;
                    (src, b.session.lock().await)
                } else {
                    let dst = b.session.lock().await;
                    (a.session.lock().await, dst)
                };
                Reply::Clone(d.clone_script(&mut **src, &mut **dst, &tables).await?)
            }
            Call::DeltaFilter {
                driver,
                spec,
                buckets,
            } => Reply::Text(self.driver(&driver)?.delta_filter(&spec, &buckets)?),
            Call::KeyRange {
                session,
                table,
                column,
            } => {
                let slot = self.slot(session)?;
                let r = slot.session.lock().await.key_range(&table, &column).await?;
                Reply::KeyRange(r)
            }
            Call::DeltaSummary { session, spec } => {
                let slot = self.slot(session)?;
                let r = slot.session.lock().await.delta_summary(&spec).await?;
                Reply::Buckets(r)
            }
            Call::DeltaApply {
                session,
                spec,
                buckets,
                columns,
            } => {
                let slot = self.slot(session)?;
                let rx = io
                    .inbox
                    .take()
                    .ok_or_else(|| Error::State("sincronización sin canal de lotes".into()))?;
                let mut inbox = Inbox {
                    id,
                    rx,
                    tx: self.tx.clone(),
                };
                let tx = self.tx.clone();
                let progress = move |rows: u64| {
                    let _ = send(&tx, FromHost::Committed { id, rows });
                };
                let r = slot
                    .session
                    .lock()
                    .await
                    .delta_apply(&spec, &buckets, &columns, &mut inbox, &progress)
                    .await?;
                Reply::Delta(r)
            }
            Call::WriteRows {
                session,
                spec,
                columns,
            } => {
                let slot = self.slot(session)?;
                let rx = io
                    .inbox
                    .take()
                    .ok_or_else(|| Error::State("escritura sin canal de lotes".into()))?;
                let mut inbox = Inbox {
                    id,
                    rx,
                    tx: self.tx.clone(),
                };
                let tx = self.tx.clone();
                let progress = move |rows: u64| {
                    let _ = send(&tx, FromHost::Committed { id, rows });
                };
                let r = slot
                    .session
                    .lock()
                    .await
                    .write_rows(&spec, &columns, &mut inbox, &progress)
                    .await?;
                Reply::Written(r)
            }
            Call::Permissions { session, database } => Reply::Permissions(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .permissions(database.as_deref())
                    .await?,
            ),
            Call::ListSchemas { session } => Reply::Schemas(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .list_schemas()
                    .await?,
            ),
            Call::TransactionState { session } => Reply::TxState(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .transaction_state()
                    .await?,
            ),
            Call::SetAutocommit { session, on } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .set_autocommit(on)
                    .await?;
                Reply::Unit
            }
            Call::Commit { session } => {
                self.slot(session)?.session.lock().await.commit().await?;
                Reply::Unit
            }
            Call::Rollback { session } => {
                self.slot(session)?.session.lock().await.rollback().await?;
                Reply::Unit
            }
            Call::SplitScript { driver, text } => {
                Reply::Units(self.driver(&driver)?.split_script(&text))
            }
            Call::IndexUsage { session, table } => Reply::IndexUsage(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .index_usage(&table)
                    .await?,
            ),
            Call::Dependents {
                session,
                target,
                scan,
            } => Reply::Dependents(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .dependents(&target, &scan)
                    .await?,
            ),
            Call::Processes { session } => {
                Reply::Processes(self.slot(session)?.session.lock().await.processes().await?)
            }
            Call::CreateDatabaseChoices { session } => Reply::Choices(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .create_database_choices()
                    .await?,
            ),
            Call::HealthChecks { session, database } => Reply::HealthChecks(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .health_checks(&database)
                    .await?,
            ),
            Call::RowEstimates { session } => Reply::RowEstimates(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .row_estimates()
                    .await?,
            ),
            Call::ObjectComments { session } => Reply::ObjectComments(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .object_comments()
                    .await?,
            ),
            Call::SearchCode { session, query } => Reply::CodeSearch(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .search_code(&query)
                    .await?,
            ),
            Call::DatabaseProperties { session, database } => Reply::DatabaseProperties(
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .database_properties(&database)
                    .await?,
            ),
            Call::AlterDatabase {
                session,
                database,
                changes,
            } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .alter_database(&database, &changes)
                    .await?;
                Reply::Unit
            }
            Call::CreateDatabaseWith {
                session,
                name,
                options,
            } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .create_database_with(&name, &options)
                    .await?;
                Reply::Unit
            }
            Call::CancelQuery { session, id } => {
                self.slot(session)?
                    .session
                    .lock()
                    .await
                    .cancel_query(&id)
                    .await?;
                Reply::Unit
            }
        })
    }
}

/// A run's outcome with its error, if any: what ran before a failing
/// statement stays in the outcome, as with a driver in the app.
fn run_reply(mut out: QueryOutcome, r: Result<(), Error>) -> Reply {
    out.sink = None;
    Reply::Run(out, r.err().map(|e| WireError::from(&e)))
}

#[cfg(unix)]
fn take_stdout() -> std::fs::File {
    use std::os::fd::FromRawFd;
    // SAFETY: fd 1 and 2 are the process's standard streams; `dup` gives a
    // new descriptor this File owns, then fd 1 points at stderr.
    unsafe {
        let fd = libc::dup(1);
        libc::dup2(2, 1);
        std::fs::File::from_raw_fd(fd)
    }
}

#[cfg(windows)]
fn take_stdout() -> std::fs::File {
    use std::os::windows::io::FromRawHandle;
    // SAFETY: as on unix, through the C runtime's descriptors, plus the
    // process's standard output handle (what Rust's own stdout uses).
    unsafe {
        let fd = libc::dup(1);
        let handle = libc::get_osfhandle(fd);
        libc::dup2(2, 1);
        let err = windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
        );
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
            err,
        );
        std::fs::File::from_raw_handle(handle as _)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A read waiting for the app's acks must not hold a runtime worker:
    /// on a host with a single worker, another call (the bulk load whose
    /// progress frees the app, which then sends the ack) still runs.
    #[test]
    fn a_read_waiting_for_acks_leaves_the_workers_free() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let credits = Arc::new(Credits::new());
        let failed = Arc::new(Mutex::new(None));
        for _ in 0..BATCH_WINDOW {
            credits.acquire(&failed).unwrap();
        }
        let (tx, _rx) = sync_channel::<FromHost>(QUEUE);
        let mut fwd = BatchForwarder {
            id: 1,
            tx,
            credits: credits.clone(),
            failed,
        };
        let reading = rt.spawn(async move { fwd.batch(RowBatch::default()) });
        std::thread::sleep(Duration::from_millis(200));

        let (otx, orx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let _ = otx.send(());
        });
        let ran = orx.recv_timeout(Duration::from_secs(5)).is_ok();
        // Let the read go before asserting, so a failure doesn't hang the runtime.
        credits.release();
        rt.block_on(reading).unwrap().unwrap();
        assert!(ran, "the waiting read held the only worker");
    }

    /// With no runtime (or a driver's own current-thread one) the helpers
    /// just run.
    #[test]
    fn waits_work_outside_a_multi_thread_runtime() {
        let (tx, rx) = sync_channel::<FromHost>(1);
        send(&tx, FromHost::BatchAck { id: 1 }).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let credits = Credits::new();
        rt.block_on(async {
            credits.acquire(&Mutex::new(None)).unwrap();
            assert!(matches!(rx.recv().unwrap(), FromHost::BatchAck { id: 1 }));
            send(&tx, FromHost::BatchAck { id: 2 }).unwrap();
        });
        drop(rx);
        assert!(send(&tx, FromHost::BatchAck { id: 3 }).is_err());
    }

    // ---- Every call reaches the driver or session it names ---------------

    use dbine_driver::{
        ColumnInfo, ConnectionConfig, DbObject, DdlParts, DriverInfo, Family, IndexUsage, Language,
        ObjectRef, StatementResult, SyncScript, TableChange, TableSchema,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;

    /// A driver that answers each script with what it was asked.
    struct Echo(DriverInfo);

    fn echo() -> Arc<dyn Driver> {
        Arc::new(Echo(DriverInfo {
            id: "echo",
            name: "Echo",
            family: Family::Relational,
            language: Language::Sql,
            dialect: "postgres",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
            tls: Default::default(),
            tls_port: None,
        }))
    }

    #[dbine_driver::async_trait]
    impl Driver for Echo {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(
            &self,
            cfg: &ConnectionConfig,
            database: Option<&str>,
        ) -> Result<Box<dyn Session>, Error> {
            if cfg.host == "down" {
                return Err(Error::Connect("no responde".into()));
            }
            Ok(Box::new(EchoSession {
                database: database.unwrap_or("main").to_string(),
                interrupted: Arc::new(AtomicBool::new(false)),
            }))
        }
        fn map_login_script(
            &self,
            login: &str,
            user: &str,
            default_schema: Option<&str>,
        ) -> Result<String, Error> {
            Ok(format!("map {login} {user} {default_schema:?}"))
        }
        fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String, Error> {
            Ok(format!("security {action:?}"))
        }
        fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String, Error> {
            Ok(format!("backup {action:?}"))
        }
        fn create_database_script(
            &self,
            name: &str,
            options: &BTreeMap<String, String>,
        ) -> Result<String, Error> {
            Ok(format!("create {name} {options:?}"))
        }
        fn alter_database_script(
            &self,
            database: &str,
            changes: &BTreeMap<String, String>,
        ) -> Result<String, Error> {
            Ok(format!("alter {database} {changes:?}"))
        }
        fn create_schema_script(
            &self,
            database: Option<&str>,
            name: &str,
            owner: Option<&str>,
        ) -> Result<String, Error> {
            Ok(format!("schema {database:?} {name} {owner:?}"))
        }
        fn schema_owner_script(
            &self,
            database: Option<&str>,
            name: &str,
            owner: &str,
        ) -> Result<Option<String>, Error> {
            Ok(Some(format!("owner {database:?} {name} {owner}")))
        }
        fn drop_schema_script(
            &self,
            database: Option<&str>,
            name: &str,
            cascade: bool,
        ) -> Result<String, Error> {
            Ok(format!("drop {database:?} {name} {cascade}"))
        }
        fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String, Error> {
            Ok(format!("ddl {} create={}", table.name, parts.create))
        }
        fn data_load_wrap(&self, table: &TableSchema) -> (String, String) {
            (format!("before {}", table.name), "after".into())
        }
        fn sync_script(&self, changes: &[TableChange]) -> Result<SyncScript, Error> {
            Ok(SyncScript {
                statements: vec![format!("{} changes", changes.len())],
                warnings: vec![],
            })
        }
        fn index_toggle_script(
            &self,
            table: &ObjectRef,
            index: &IndexUsage,
            enable: bool,
        ) -> Result<SyncScript, Error> {
            Ok(SyncScript {
                statements: vec![format!("{} {} {enable}", table.name, index.name)],
                warnings: vec![],
            })
        }
        fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<SyncScript, Error> {
            Ok(SyncScript {
                statements: vec![format!("rename to {}", req.new_name)],
                warnings: vec![],
            })
        }
        fn rename_database_script(
            &self,
            database: &str,
            new_name: &str,
            objects: &[dbine_driver::rename::DatabaseObject],
        ) -> Result<SyncScript, Error> {
            Ok(SyncScript {
                statements: vec![format!("{database} {new_name} {}", objects.len())],
                warnings: vec![],
            })
        }
        fn param_literal(&self, value: &dbine_driver::ParamValue) -> Result<String, Error> {
            Ok(format!("lit {value:?}"))
        }
    }

    struct EchoSession {
        database: String,
        interrupted: Arc<AtomicBool>,
    }

    #[dbine_driver::async_trait]
    impl Session for EchoSession {
        async fn server_version(&mut self) -> Result<String, Error> {
            Ok("echo 1.0".into())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>, Error> {
            Ok(vec![self.database.clone(), "otra".into()])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>, Error> {
            Ok(vec![])
        }
        async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>, Error> {
            Err(Error::Query(format!("no existe {}", obj.name)))
        }
        async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>, Error> {
            Ok(Some(format!("def {}", obj.name)))
        }
        fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
            format!("SELECT * FROM {} LIMIT {limit}", obj.name)
        }
        async fn execute(
            &mut self,
            text: &str,
            _max_rows: usize,
            out: &mut QueryOutcome,
        ) -> Result<(), Error> {
            let columns = vec![ResultColumn {
                name: "n".into(),
                type_name: "int".into(),
            }];
            let rows = vec![vec![json!(1)], vec![json!(2)]];
            match &out.sink {
                Some(s) => {
                    let mut s = s.0.lock().unwrap();
                    s.begin(0, &columns)?;
                    for r in &rows {
                        s.row(0, r)?;
                    }
                    out.results.push(StatementResult {
                        columns,
                        ..Default::default()
                    });
                }
                None => out.results.push(StatementResult {
                    columns,
                    rows,
                    ..Default::default()
                }),
            }
            if text == "fail" {
                return Err(Error::Query("falló".into()));
            }
            Ok(())
        }
        fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
            let flag = self.interrupted.clone();
            Some(Arc::new(move || flag.store(true, Ordering::SeqCst)))
        }
        async fn create_database(&mut self, name: &str) -> Result<(), Error> {
            if name.is_empty() {
                return Err(Error::Query("sin nombre".into()));
            }
            Ok(())
        }
        async fn drop_database(&mut self, _name: &str) -> Result<(), Error> {
            Ok(())
        }
        async fn unmapped_logins(&mut self) -> Result<Vec<String>, Error> {
            Ok(vec!["huérfano".into()])
        }
        async fn kill_session(&mut self, id: &str) -> Result<(), Error> {
            match id {
                "1" => Ok(()),
                _ => Err(Error::Query(format!("no hay sesión {id}"))),
            }
        }
        async fn cancel_query(&mut self, _id: &str) -> Result<(), Error> {
            Ok(())
        }
        async fn alter_database(
            &mut self,
            _database: &str,
            _changes: &BTreeMap<String, String>,
        ) -> Result<(), Error> {
            Ok(())
        }
        async fn create_database_with(
            &mut self,
            _name: &str,
            _options: &BTreeMap<String, String>,
        ) -> Result<(), Error> {
            Ok(())
        }
        async fn set_autocommit(&mut self, _on: bool) -> Result<(), Error> {
            Ok(())
        }
        async fn commit(&mut self) -> Result<(), Error> {
            Ok(())
        }
        async fn rollback(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    fn state() -> (Arc<State>, std::sync::mpsc::Receiver<FromHost>) {
        let (tx, rx) = sync_channel::<FromHost>(QUEUE);
        (
            Arc::new(State {
                package: "echo-pkg".into(),
                drivers: vec![echo()],
                sessions: Mutex::new(HashMap::new()),
                next_session: AtomicU64::new(1),
                calls: Mutex::new(HashMap::new()),
                tx,
            }),
            rx,
        )
    }

    /// `call` (as the app sends it, in JSON form) through the host.
    async fn call(st: &State, id: u64, call: serde_json::Value) -> Result<Reply, Error> {
        let call: Call = serde_json::from_value(call).expect("a valid call");
        st.handle(id, call, Arc::new(Mutex::new(None)), CallIo::default())
            .await
    }

    /// The reply's text form (what the assertions look into).
    async fn answer(st: &State, c: serde_json::Value) -> String {
        format!("{:?}", call(st, 1, c).await)
    }

    async fn connect(st: &State) -> u64 {
        match call(
            st,
            1,
            json!({"Connect": {"driver": "echo", "config": {"driver": "echo", "host": "h"}, "database": "ventas"}}),
        )
        .await
        {
            Ok(Reply::Session {
                id, interruptible, ..
            }) => {
                assert!(interruptible);
                id
            }
            _ => panic!("expected a session"),
        }
    }

    fn table() -> serde_json::Value {
        json!({"kind": "table", "schema": "s", "name": "clientes", "columns": []})
    }

    fn obj() -> serde_json::Value {
        json!({"kind": "table", "schema": "s", "name": "clientes"})
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_calls_reach_the_driver_with_their_arguments() {
        let (st, _rx) = state();
        let d = "echo";
        for (c, want) in [
            (
                json!({"MapLoginScript": {"driver": d, "login": "l", "user": "u", "default_schema": "dbo"}}),
                "map l u Some(\\\"dbo\\\")",
            ),
            (
                json!({"SecurityScript": {"driver": d, "action": {"action": "create_role", "name": "lectores"}}}),
                "CreateRole { name: \\\"lectores\\\" }",
            ),
            (
                json!({"BackupScript": {"driver": d, "action": {"action": "delete", "source": "/b.bak"}}}),
                "Delete { source: \\\"/b.bak\\\" }",
            ),
            (
                json!({"CreateDatabaseScript": {"driver": d, "name": "nueva", "options": {"k": "v"}}}),
                "create nueva {\\\"k\\\": \\\"v\\\"}",
            ),
            (
                json!({"AlterDatabaseScript": {"driver": d, "database": "db", "changes": {"x": "1"}}}),
                "alter db {\\\"x\\\": \\\"1\\\"}",
            ),
            (
                json!({"CreateSchemaScript": {"driver": d, "name": "rep", "owner": "ana", "database": "db"}}),
                "schema Some(\\\"db\\\") rep Some(\\\"ana\\\")",
            ),
            (
                json!({"SchemaOwnerScript": {"driver": d, "database": null, "name": "rep", "owner": "bot"}}),
                "owner None rep bot",
            ),
            (
                json!({"DropSchemaScript": {"driver": d, "name": "rep", "cascade": true}}),
                "drop None rep true",
            ),
            (
                json!({"TableDdl": {"driver": d, "table": table(), "parts": {"create": true}}}),
                "ddl clientes create=true",
            ),
            (
                json!({"DataLoadWrap": {"driver": d, "table": table()}}),
                "Pair(\"before clientes\", \"after\")",
            ),
            (
                json!({"SyncScript": {"driver": d, "changes": [{"op": "drop", "table": table()}]}}),
                "1 changes",
            ),
            (
                json!({"IndexToggleScript": {"driver": d, "table": obj(), "index": {"name": "ix", "kind": "B"}, "enable": false}}),
                "clientes ix false",
            ),
            (
                json!({"RenameScript": {"driver": d, "request": {"target": {"what": "schema", "schema": "s"}, "new_name": "t"}}}),
                "rename to t",
            ),
            (
                json!({"RenameDatabaseScript": {"driver": d, "database": "a", "new_name": "b", "objects": [{"kind": "table", "name": "x"}]}}),
                "a b 1",
            ),
            (
                json!({"ParamLiteral": {"driver": d, "value": {"type": "bool", "value": true}}}),
                "lit Bool(true)",
            ),
            // The contract's own defaults, through the same path.
            (
                json!({"SchemaGrantScript": {"driver": d, "database": null, "name": "rep", "privileges": ["USAGE"], "to": "r", "grantable": false}}),
                "security Grant",
            ),
            (
                json!({"InsertScript": {"driver": d, "target": obj(), "columns": ["a"], "rows": [[1]]}}),
                "INSERT INTO",
            ),
            (
                json!({"UpdateScript": {"driver": d, "target": obj(), "changes": [{"key": [["id", 1]], "set": [["a", 2]]}]}}),
                "UPDATE",
            ),
            (
                json!({"DeleteScript": {"driver": d, "target": obj(), "keys": [[["id", 1]]]}}),
                "DELETE FROM",
            ),
            (
                json!({"FilteredBrowse": {"driver": d, "browse": "SELECT * FROM t", "filters": [{"column": "a", "op": "is_null"}]}}),
                "IS NULL",
            ),
            (
                json!({"SortedBrowse": {"driver": d, "query": "SELECT * FROM t", "sort": [{"column": "a", "descending": true}]}}),
                "DESC",
            ),
            (
                json!({"AggregateQuery": {"driver": d, "query": "SELECT * FROM t", "spec": {"x": "a", "measures": [{"func": "count"}]}}}),
                "COUNT",
            ),
            (
                json!({"SplitScript": {"driver": d, "text": "SELECT 1; SELECT 2;"}}),
                "Units",
            ),
        ] {
            let got = answer(&st, c.clone()).await;
            assert!(got.starts_with("Ok("), "{c}");
            assert!(got.contains(want), "{c}");
        }
        // A driver this process doesn't serve.
        let got = answer(
            &st,
            json!({"TableDdl": {"driver": "otro", "table": table(), "parts": {}}}),
        )
        .await;
        assert!(got.contains("no sirve el driver «otro»"));
        // The manifest describes what it serves.
        match call(&st, 1, json!("Manifest")).await {
            Ok(Reply::Manifest(m)) => {
                assert_eq!(m.len(), 1);
                assert_eq!((m[0].package.as_str(), m[0].info.id), ("echo-pkg", "echo"));
            }
            _ => panic!("unexpected reply"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_calls_reach_the_session() {
        let (st, _rx) = state();
        let s = connect(&st).await;
        assert_eq!(
            connect(&st).await,
            s + 1,
            "each connection a session of its own"
        );
        for (c, want) in [
            (
                json!({"ServerVersion": {"session": s}}),
                "Text(\"echo 1.0\")",
            ),
            (
                json!({"ListDatabases": {"session": s}}),
                "Texts([\"ventas\", \"otra\"])",
            ),
            (json!({"ListObjects": {"session": s}}), "Objects([])"),
            (
                json!({"Definition": {"session": s, "obj": obj()}}),
                "MaybeText(Some(\"def clientes\"))",
            ),
            (
                json!({"BrowseQuery": {"session": s, "obj": obj(), "limit": 7}}),
                "SELECT * FROM clientes LIMIT 7",
            ),
            (
                json!({"CreateDatabase": {"session": s, "name": "x"}}),
                "Ok(Unit)",
            ),
            (
                json!({"DropDatabase": {"session": s, "name": "x"}}),
                "Ok(Unit)",
            ),
            (
                json!({"UnmappedLogins": {"session": s}}),
                "Texts([\"huérfano\"])",
            ),
            (
                json!({"KillSession": {"session": s, "id": "1"}}),
                "Ok(Unit)",
            ),
            (
                json!({"CancelQuery": {"session": s, "id": "1"}}),
                "Ok(Unit)",
            ),
            (
                json!({"SetAutocommit": {"session": s, "on": false}}),
                "Ok(Unit)",
            ),
            (json!({"Commit": {"session": s}}), "Ok(Unit)"),
            (json!({"Rollback": {"session": s}}), "Ok(Unit)"),
            (
                json!({"AlterDatabase": {"session": s, "database": "x", "changes": {}}}),
                "Ok(Unit)",
            ),
            (
                json!({"CreateDatabaseWith": {"session": s, "name": "x", "options": {}}}),
                "Ok(Unit)",
            ),
        ] {
            let got = answer(&st, c.clone()).await;
            assert!(got.contains(want), "{c}");
        }
        // The session's own errors come back as they are.
        let got = answer(&st, json!({"Columns": {"session": s, "obj": obj()}})).await;
        assert!(got.contains("no existe clientes"));
        let got = answer(&st, json!({"KillSession": {"session": s, "id": "9"}})).await;
        assert!(got.contains("no hay sesión 9"));
        let got = answer(&st, json!({"CreateDatabase": {"session": s, "name": ""}})).await;
        assert!(got.contains("sin nombre"));
        // What the session doesn't do: the contract's refusal.
        for c in [
            json!({"Monitor": {"session": s}}),
            json!({"Blocking": {"session": s}}),
            json!({"Principals": {"session": s}}),
            json!({"Grants": {"session": s, "principal": "ana"}}),
            json!({"Backups": {"session": s, "database": null}}),
            json!({"ProfilerStart": {"session": s, "opts": {"database": "x", "change_server": false}}}),
            json!({"ProfilerPoll": {"session": s}}),
            json!({"ProfilerStop": {"session": s}}),
            json!({"ScanKeys": {"session": s, "scan": {"count": 10}}}),
            json!({"Permissions": {"session": s, "database": null}}),
            json!({"Processes": {"session": s}}),
            json!({"CreateDatabaseChoices": {"session": s}}),
            json!({"DatabaseProperties": {"session": s, "database": "x"}}),
            json!({"HealthChecks": {"session": s, "database": "x"}}),
            json!({"RowEstimates": {"session": s}}),
            json!({"ObjectComments": {"session": s}}),
            json!({"SearchCode": {"session": s, "query": {"text": "x"}}}),
            json!({"IndexUsage": {"session": s, "table": obj()}}),
            json!({"KeyRange": {"session": s, "table": obj(), "column": "id"}}),
            json!({"TransactionState": {"session": s}}),
            json!({"ListSchemas": {"session": s}}),
            json!({"DatabaseSchema": {"session": s}}),
        ] {
            // Some have a default answer (an empty list, `None`); none fails
            // in any other way than the contract's own.
            let got = answer(&st, c.clone()).await;
            assert!(got.starts_with("Ok(") || got.contains("Unsupported"), "{c}");
        }
        // A session that's gone.
        let got = answer(&st, json!({"ServerVersion": {"session": 999}})).await;
        assert!(got.contains("la sesión ya se cerró"));
        // A driver that can't connect.
        let got = answer(
            &st,
            json!({"Connect": {"driver": "echo", "config": {"driver": "echo", "host": "down"}, "database": null}}),
        )
        .await;
        assert!(got.contains("no responde"));
        assert_eq!(st.sessions.lock().unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_stream_to_the_app_or_come_back_whole() {
        let (st, rx) = state();
        let s = connect(&st).await;
        // Without a sink: the rows in the reply.
        match call(
            &st,
            5,
            json!({"Execute": {"session": s, "text": "SELECT", "max_rows": 10, "sink": false}}),
        )
        .await
        {
            Ok(Reply::Run(out, None)) => {
                assert_eq!(out.results[0].rows.len(), 2);
                assert!(out.sink.is_none());
            }
            _ => panic!("unexpected reply"),
        }
        // With a sink: each row goes to the app as it comes, tagged with
        // the call's id, and the reply carries none.
        match call(
            &st,
            6,
            json!({"Execute": {"session": s, "text": "SELECT", "max_rows": 10, "sink": true, "live": true}}),
        )
        .await
        {
            Ok(Reply::Run(out, None)) => assert!(out.results[0].rows.is_empty()),
            _ => panic!("unexpected reply"),
        }
        let frames: Vec<FromHost> = rx.try_iter().collect();
        assert!(
            matches!(&frames[0], FromHost::SinkBegin { id: 6, index: 0, columns } if columns[0].name == "n"),
            "{frames:?}"
        );
        let rows: Vec<&Vec<Value>> = frames
            .iter()
            .filter_map(|f| match f {
                FromHost::SinkRow { id: 6, row, .. } => Some(row),
                _ => None,
            })
            .collect();
        assert_eq!(rows, [&vec![json!(1)], &vec![json!(2)]]);
        // An error keeps what ran before it.
        match call(
            &st,
            7,
            json!({"Execute": {"session": s, "text": "fail", "max_rows": 10, "sink": false, "continue_on_error": true}}),
        )
        .await
        {
            Ok(Reply::Run(out, Some(e))) => {
                assert_eq!(out.results.len(), 1);
                assert!(format!("{e:?}").contains("falló"));
            }
            _ => panic!("unexpected reply"),
        }
        // The app's sink failed: the run stops at its next row.
        let failed = Arc::new(Mutex::new(Some("disco lleno".to_string())));
        let c: Call = serde_json::from_value(
            json!({"Execute": {"session": s, "text": "SELECT", "max_rows": 10, "sink": true}}),
        )
        .unwrap();
        match st.handle(8, c, failed, CallIo::default()).await {
            Ok(Reply::Run(_, Some(e))) => {
                assert!(format!("{e:?}").contains("disco lleno"))
            }
            _ => panic!("unexpected reply"),
        }
        // Explain, bound statements and server-enforced reads are runs
        // too: here the contract's refusal comes back inside the run.
        for c in [
            json!({"Explain": {"session": s, "text": "SELECT", "analyze": false, "max_rows": 10, "sink": false}}),
            json!({"RunReadOnly": {"session": s, "statement": "SELECT", "max_rows": 10, "sink": false}}),
        ] {
            let got = answer(&st, c.clone()).await;
            assert!(got.starts_with("Ok(Run("), "{c}");
        }
        // A bulk load without its channel of batches is refused.
        let got = answer(
            &st,
            json!({"BulkLoad": {"session": s, "spec": {"table": obj(), "columns": ["a"], "commit_rows": 10, "commit_bytes": 1000}, "columns": []}}),
        )
        .await;
        assert!(got.contains("carga sin canal de lotes"));
        // The session's interrupter is kept apart, for a cancel mid-run.
        let slot = st.slot(s).unwrap();
        (slot.interrupter.as_ref().expect("interruptible"))();
    }
}

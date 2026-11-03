//! kdb+ (KX) over q's IPC protocol (default port 5000), with TLS as q's
//! `:tcps://` connections. Queries are q: each statement of a script goes
//! to the server as a string it evaluates (`value`, through `.z.pg`).
//!
//! The explorer lists the root namespace's tables, views, functions and
//! variables, and those of the user's namespaces (`.ns.t`, `.u` of
//! kdb+tick); kdb+'s own (`.q`, `.Q`, `.h`, `.j`, `.o`, `.z`) are left out.
//! Tables of a database on disk show apart: partitioned (an HDB's, read a
//! partition at a time) and splayed (a file per column). Both are read
//! only in the grid: their rows change by rewriting their files with q.
//!
//! Live tests need a licensed kdb+ (see `tests/live.rs`).

use dbine_driver::async_trait;
use dbine_driver::{
    kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, Language, ObjectKindInfo, ObjectRef,
    ParamValue, QueryOutcome, Result, RowChange, ScriptError, ScriptStatement, Session,
    TableSchema, TlsLevel, TlsSupport,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub mod cells;
pub mod conn;
pub mod edit;
pub mod guard;
pub mod ipc;
mod monitor;
pub mod script;
mod steps;
pub mod sync;

use ipc::K;

/// Variables that aren't tables, views or functions (lists, dictionaries,
/// atoms).
pub const VARIABLE: &str = "variable";
/// A partitioned table (`.Q.qp` is `1b`): an HDB's, one directory per
/// date (month, year or int) with a file per column.
pub const PARTITIONED: &str = "partitioned_table";
/// A splayed table (`.Q.qp` is `0b`): on disk, a file per column.
pub const SPLAYED: &str = "splayed_table";

/// The explorer in one round trip: a table of `ns` (`.` for the root),
/// `k` (the kind) and `n` (the name) for the root and every namespace but
/// kdb+'s own. Each namespace is read under protection (one that fails
/// leaves only itself out), views with `\b` before anything else (reading
/// a view's value would compute it), each other object by its value:
/// functions by type, tables by `.Q.qt`, and `.Q.qp` tells partitioned
/// (`1b`) and splayed (`0b`) tables from those in memory (`0`).
pub const EXPLORER: &str = concat!(
    // kdb+'s own namespaces (and the empty name) are left out.
    "{nsl:(enlist enlist \".\"),\".\",/:string (key `) except `q`Q`h`j`o`z,`$\"\";",
    "f:{[ns] s:`$ns; b:@[system;\"b \",ns;`$()]; n:(key s) except b,`$\"\";",
    "k:{$[100h<=type x;`function;@[.Q.qt;x;0b];$[1b~p:@[.Q.qp;x;0];`partitioned;0b~p;`splayed;`table];`variable]} each s@n;",
    "([] ns:(count[n]+count b)#`$ns; k:(`$()),k,count[b]#`view; n:(`$()),n,b)};",
    "raze {[f;ns] @[f;ns;{([] ns:`$(); k:`$(); n:`$())}]}[f] each nsl}[]"
);

/// Results bigger than this (as `-22!` measures them on the server) come
/// back to the grid as their first rows and their count, instead of whole.
const SHOWN_BYTES: u64 = 4 << 20;

/// The marker of a result cut on the server: `(marker; count; first rows)`.
const PARTIAL: &str = "dbine.partial";

/// The body of a lambda that evaluates its string inside `reval`. `reval`
/// takes a parse tree: a statement that parses to a constant (`1`,
/// `1 2 3`, `"abc"`) is a `'type` there, and is already its own value.
const REVAL: &str = "p:parse x;$[(type p)in 0 -11h;reval p;p]";

/// `expr` (a statement's evaluation) as sent for the grid: when its result
/// is a list, table or dictionary of more than `rows` items and more than
/// [`SHOWN_BYTES`] serialized, the server answers its first `rows` items
/// and its count instead (strings, atoms and functions as they are).
pub fn guarded(expr: &str, rows: usize) -> String {
    format!(
        "{{[n;b;r] t:type r; $[(t<0)|(t>99)|t=10h; r; (n<c:count r)&b< -22!r; (`{PARTIAL};c;n sublist r); r]}}[{rows};{SHOWN_BYTES}] {expr}"
    )
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(KdbDriver { info: info() })]
}

/// q's `tcps`: a `:tcps://host:port` handle.
fn tls_field() -> Field {
    Field::tls_switch(
        "tcps",
        "tcps",
        TlsLevel::VerifyFull,
        TlsLevel::Off,
        true,
        ["false", "false", "true", "true"],
    )
    .help("Cifra la conexión con TLS (:tcps://) y verifica el certificado del servidor.")
}

/// q's `SSL_VERIFY_SERVER` (`YES` / `NO`).
fn verify_field() -> Field {
    Field::tls_skip_verify(
        "SSL_VERIFY_SERVER",
        "SSL_VERIFY_SERVER",
        true,
        ["true", "true", "true", "false"],
    )
    .help("Desmarcado (SSL_VERIFY_SERVER=NO) no verifica el certificado: DBine acepta el primero que ve y después solo ese.")
}

/// q's `SSL_CA_CERT_FILE`.
fn ca_field() -> Field {
    Field::tls_ca_file("SSL_CA_CERT_FILE", "SSL_CA_CERT_FILE").help(
        "Archivo PEM con la CA que firmó el certificado del servidor; vacío = las autoridades del sistema.",
    )
}

fn info() -> DriverInfo {
    DriverInfo {
        tls: TlsSupport::Configurable,
        tls_port: None,
        id: "kdb",
        name: "KDB+",
        family: Family::TimeSeries,
        // q has no editor language of its own yet: plain text, one
        // statement per line (`dialect` tells the app it's q).
        language: Language::Redis,
        dialect: "q",
        default_port: 5000,
        fields: vec![
            Field::host(),
            Field::port().placeholder("5000"),
            Field::username().help("Vacío si el proceso no pide usuario (sin -u ni -U)."),
            Field::password(),
            tls_field(),
            verify_field(),
            ca_field(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::new(PARTITIONED, "Tablas particionadas", true, true, true),
            ObjectKindInfo::new(SPLAYED, "Tablas splayed", true, true, true),
            ObjectKindInfo::new(kinds::VIEW, "Vistas", true, true, true),
            ObjectKindInfo::new(kinds::FUNCTION, "Funciones", false, false, true),
            ObjectKindInfo::new(VARIABLE, "Variables", false, true, false),
        ],
    }
}

const HELP: &str =
    "Consultas en q, una por línea (una línea que empieza con espacio sigue la anterior):\n\
select from trade where date=last date, sym=`AAPL\n\
select avg price by sym from trade\n\
meta trade | tables[] | count trade\n\
\\v (variables) | \\f (funciones) | \\b (vistas)\n\
Un comentario empieza con / al principio de la línea o después de un espacio.";

pub struct KdbDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for KdbDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        HELP
    }

    /// A kdb+ process is one namespace tree (nothing to create or drop):
    /// its memory and connections show in the Monitor, and a connection
    /// can be closed.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            monitor: true,
            processes: true,
            kill_session: true,
            ..Capabilities::default()
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(edit::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        edit::templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        edit::table_ddl(table, parts)
    }

    /// Tables in memory change with q per column (`update`, `delete`,
    /// casts, `xkey`); tables on disk are refused ([`sync`]).
    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(
        &self,
        changes: &[dbine_driver::TableChange],
    ) -> Result<dbine_driver::SyncScript> {
        sync::script(changes)
    }

    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        Some(sync::rename_spec())
    }

    fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<dbine_driver::SyncScript> {
        sync::rename(req)
    }

    fn insert_script(
        &self,
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<String> {
        writable(target)?;
        edit::insert_script(&target.name, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        writable(target)?;
        edit::update_script(&target.name, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        writable(target)?;
        edit::delete_script(&target.name, keys)
    }

    fn filtered_browse(
        &self,
        browse: &str,
        filters: &[dbine_driver::ColumnFilter],
    ) -> Result<String> {
        edit::filtered_browse(browse, filters)
    }

    fn sorted_browse(&self, query: &str, sort: &[dbine_driver::SortKey]) -> Result<String> {
        edit::sorted_browse(query, sort)
    }

    fn split_script(&self, text: &str) -> Vec<ScriptStatement> {
        script::split(text)
    }

    fn param_literal(&self, value: &ParamValue) -> Result<String> {
        Ok(edit::param_literal(value))
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        _database: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let host = if cfg.host.trim().is_empty() {
            "localhost".to_string()
        } else {
            cfg.host.trim().to_string()
        };
        let (tls, watch) = tls(cfg)?;
        let pinned = dbine_tls::pin_of(cfg).is_some();
        let target = conn::Target {
            host,
            port: cfg.port_or(5000),
            user: cfg.username_or_empty().to_string(),
            password: cfg.password_or_empty().to_string(),
            tls,
            timeout: Duration::from_secs(20),
        };
        let c = conn::Conn::open(&target, false)
            .await
            .map_err(|e| match &watch {
                Some(w) => w.explain(e),
                None => e,
            })?;
        let mut s = KdbSession {
            target,
            conn: Some(c),
            after_cancel: false,
            read_only: cfg.read_only,
            reval: false,
            cancel: Arc::new(Cancel::default()),
            tls_pin: if pinned {
                None
            } else {
                watch.and_then(|w| w.seen())
            },
        };
        // `reval` (kdb+ 3.6+) evaluates without writes: what read-only
        // connections and MCP reads run inside.
        s.reval = matches!(
            s.q(&format!("@[{{{REVAL};1b}};\"1+1\";0b]")).await,
            Ok(K::Bool(true))
        );
        Ok(Box::new(s))
    }
}

/// Only tables in memory take edited rows.
fn writable(target: &ObjectRef) -> Result<()> {
    if target.kind == kinds::TABLE || target.kind.is_empty() {
        Ok(())
    } else if target.kind == PARTITIONED || target.kind == SPLAYED {
        Err(Error::Unsupported(
            "en kdb+ no se editan filas de una tabla particionada o splayed: cada columna es un archivo en disco, que se cambia reescribiéndolo con q".into(),
        ))
    } else {
        Err(Error::Unsupported(
            "en kdb+ solo se editan filas de tablas: una vista se calcula y una variable se cambia con q".into(),
        ))
    }
}

/// The TLS settings: none, verified (by the system's or the given CA),
/// or, with `SSL_VERIFY_SERVER` off, pinned to the first certificate seen.
fn tls(cfg: &ConnectionConfig) -> Result<(Option<conn::Tls>, Option<dbine_tls::PinWatch>)> {
    if cfg.tls_value(&tls_field()) != Some("true") {
        return Ok((None, None));
    }
    let verify = verify_field();
    let skip = cfg.tls_value(&verify).is_some_and(|v| verify.tls_skips(v));
    let name = cfg.tls_server_name().trim_matches(['[', ']']).to_string();
    let server_name = rustls::pki_types::ServerName::try_from(name)
        .map_err(|e| Error::Connect(format!("nombre de servidor inválido: {e}")))?;
    let (config, watch) = if skip {
        let (c, w) = dbine_tls::pinned_config(dbine_tls::pin_of(cfg))?;
        (c, Some(w))
    } else if let Some(path) = cfg.tls_value(&ca_field()) {
        let mut roots = rustls::RootCertStore::empty();
        dbine_tls::add_roots(
            &mut roots,
            dbine_tls::read_ca_file(path, "SSL_CA_CERT_FILE")?,
            "SSL_CA_CERT_FILE",
        )?;
        (dbine_tls::verified_config(roots)?, None)
    } else {
        (dbine_tls::verified_config(dbine_tls::system_roots())?, None)
    };
    Ok((
        Some(conn::Tls {
            config: Arc::new(config),
            server_name,
        }),
        watch,
    ))
}

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

pub struct KdbSession {
    target: conn::Target,
    /// `None` after a cancel or a lost connection: the next request
    /// connects again.
    conn: Option<conn::Conn>,
    /// The last connection was dropped by a cancel while kdb+ ran a query:
    /// until a new one opens, a timeout means it's still running it.
    after_cancel: bool,
    read_only: bool,
    /// The server has `reval`.
    reval: bool,
    cancel: Arc<Cancel>,
    tls_pin: Option<String>,
}

/// What a read-only connection runs on a server without `reval`, where
/// nothing on the server keeps a statement from writing: the browses DBine
/// writes for an object (they read it, and nothing else) and the system
/// commands that only show something (the guard has already checked those).
/// q is a whole language: any other statement could write through a path
/// no word list sees (a handle, `.q` lookups, a string evaluated).
fn unenforced_read(stmt: &str) -> bool {
    let s = stmt.trim();
    s.starts_with('\\') || edit::plain_browse(s)
}

fn no_reval() -> String {
    "Conexión de solo lectura: este servidor kdb+ no tiene reval (kdb+ 3.6 o posterior), así que no puede asegurar que una consulta solo lea. Acá solo se abren tablas y variables y se ejecutan los comandos de sistema que muestran algo; para otras consultas usá una conexión que pueda escribir o kdb+ 3.6 o posterior.".into()
}

/// A q error as the console shows it (`'type`, `'length`).
fn q_error(e: &str) -> Error {
    Error::Statement(Box::new(
        ScriptError::new(format!("'{e}")).with_code(e.to_string()),
    ))
}

impl KdbSession {
    /// Run `text` and return what it gave; a q error is an `Err`. A
    /// connection found dropped (the request couldn't be written, or it
    /// closed before a byte of the answer) is opened again and the request
    /// sent once more: kdb+ can't have run it.
    async fn q(&mut self, text: &str) -> Result<K> {
        let mut retried = false;
        loop {
            let fresh = self.conn.is_none();
            if fresh {
                self.conn = Some(conn::Conn::open(&self.target, self.after_cancel).await?);
                self.after_cancel = false;
            }
            let cancel = self.cancel.clone();
            let notified = cancel.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if cancel.flag.load(Ordering::SeqCst) {
                self.conn = None;
                return Err(Error::Cancelled);
            }
            let r = {
                let c = self.conn.as_mut().expect("connected above");
                tokio::select! {
                    r = c.call(text) => Some(r),
                    _ = &mut notified => None,
                }
            };
            return match r {
                None => {
                    self.conn = None;
                    self.after_cancel = true;
                    Err(Error::Cancelled)
                }
                Some(Ok(K::Error(e))) => Err(q_error(&e)),
                Some(Ok(k)) => Ok(k),
                Some(Err(f)) => {
                    self.conn = None;
                    if f.unsent && !fresh && !retried {
                        retried = true;
                        continue;
                    }
                    Err(f.error)
                }
            };
        }
    }

    /// One statement's evaluation: inside `reval` on a read-only
    /// connection (system commands are already limited to those that
    /// show; without `reval`, [`Session::execute`] lets through only what
    /// [`unenforced_read`] accepts).
    fn eval(&self, stmt: &str) -> String {
        if self.read_only && self.reval && !stmt.trim_start().starts_with('\\') {
            format!("{{{REVAL}}} {}", edit::qstr(stmt))
        } else {
            format!("value {}", edit::qstr(stmt))
        }
    }

    /// Run `expr` and put its result in `out`. For the grid (a row limit
    /// and no row sink: export, charts and tasks have one, or no limit) a
    /// big result is cut on the server ([`guarded`]); its count still
    /// shows, as a partial result.
    async fn emit(&mut self, expr: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if max_rows == usize::MAX || out.sink.is_some() {
            let k = self.q(expr).await?;
            cells::emit(k, max_rows, out);
            return Ok(());
        }
        match self.q(&guarded(expr, max_rows)).await? {
            K::List(mut l) if l.len() == 3 && l[0] == K::Symbol(PARTIAL.into()) => {
                let rows = l.pop().expect("3 items");
                let total = match l[1] {
                    K::Long(n) => n.max(0) as u64,
                    _ => 0,
                };
                cells::emit(rows, max_rows, out);
                if let Some(r) = out.results.last_mut() {
                    if total > r.total_rows {
                        r.total_rows = total;
                        r.truncated = true;
                    }
                }
            }
            k => cells::emit(k, max_rows, out),
        }
        Ok(())
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let expr = self.eval(stmt);
        self.emit(&expr, max_rows, out).await
    }
}

/// `name` in namespace `ns` (the root's, `.`: as is).
fn qualified(ns: &str, name: &str) -> String {
    if ns == "." || ns.is_empty() {
        name.to_string()
    } else {
        format!("{ns}.{name}")
    }
}

/// A table of any kind: in memory, partitioned or splayed.
fn is_table(kind: &str) -> bool {
    kind == kinds::TABLE || kind == PARTITIONED || kind == SPLAYED
}

/// The objects of [`EXPLORER`]'s table, namespace by namespace (the root
/// first): tables, views, functions, then the other variables.
fn explorer_objects(k: K) -> Vec<DbObject> {
    let K::Table(names, cols) = k else {
        return Vec::new();
    };
    let col = |n: &str| {
        names
            .iter()
            .position(|c| c == n)
            .and_then(|i| cols[i].symbols())
            .unwrap_or_default()
    };
    let (ns, kind, name) = (col("ns"), col("k"), col("n"));
    let mut spaces: Vec<&str> = Vec::new();
    let mut rows: Vec<(usize, usize, DbObject)> = Vec::new();
    for ((ns, kind), name) in ns.iter().zip(&kind).zip(&name) {
        let (rank, kind) = match kind.as_str() {
            "table" => (0, kinds::TABLE),
            "partitioned" => (1, PARTITIONED),
            "splayed" => (2, SPLAYED),
            "view" => (3, kinds::VIEW),
            "function" => (4, kinds::FUNCTION),
            _ => (5, VARIABLE),
        };
        let space = match spaces.iter().position(|s| s == ns) {
            Some(i) => i,
            None => {
                spaces.push(ns);
                spaces.len() - 1
            }
        };
        rows.push((
            space,
            rank,
            DbObject {
                kind: kind.into(),
                schema: None,
                name: qualified(ns, name),
                parent: None,
            },
        ));
    }
    // The root first, then the namespaces as listed.
    rows.sort_by_key(|(space, rank, _)| (usize::from(spaces[*space] != "."), *space, *rank));
    rows.into_iter().map(|(_, _, o)| o).collect()
}

#[async_trait]
impl Session for KdbSession {
    fn tls_pin_seen(&self) -> Option<String> {
        self.tls_pin.clone()
    }

    async fn server_version(&mut self) -> Result<String> {
        let k = self.q("(.z.K;.z.k)").await?;
        let (v, d) = (k.item(0), k.item(1));
        Ok(format!(
            "kdb+ {} ({})",
            v.map(|v| match v {
                K::Float(f) => f.to_string(),
                other => cells::cell(&other).to_string(),
            })
            .unwrap_or_default(),
            match d {
                Some(K::Date(d)) => cells::date(d),
                _ => String::new(),
            }
        ))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        Ok(explorer_objects(self.q(EXPLORER).await?))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if !is_table(&obj.kind) && obj.kind != kinds::VIEW {
            return Ok(Vec::new());
        }
        self.cancel.flag.store(false, Ordering::SeqCst);
        let r = edit::qref(&obj.name);
        let meta = self.q(&format!("0!meta {r}")).await?;
        let keys = self
            .q(&format!("@[keys;{r};`$()]"))
            .await?
            .symbols()
            .unwrap_or_default();
        let K::Table(names, cols) = meta else {
            return Ok(Vec::new());
        };
        let col = |n: &str| names.iter().position(|c| c == n).map(|i| &cols[i]);
        let (Some(c), Some(t)) = (col("c"), col("t")) else {
            return Ok(Vec::new());
        };
        Ok((0..c.count())
            .filter_map(|i| {
                let name = c.item(i)?.text()?;
                let ty = match t.item(i)? {
                    K::Char(ch) => ch as char,
                    _ => ' ',
                };
                Some(ColumnInfo {
                    primary_key: keys.contains(&name),
                    name,
                    data_type: cells::meta_type(ty),
                    nullable: true,
                    auto_increment: false,
                    default_value: None,
                })
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let sym = edit::qsym(&obj.name);
        match obj.kind.as_str() {
            kinds::FUNCTION => Ok(match self.q(&format!("get {sym}")).await? {
                K::Lambda(_, src) => Some(src),
                K::Function(f) => Some(f),
                other => other.text(),
            }),
            kinds::VIEW => Ok(self.q(&format!("view {sym}")).await?.text()),
            k if is_table(k) => {
                let cols = self.columns(obj).await?;
                let keys: Vec<String> = cols
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect();
                let t = TableSchema {
                    name: obj.name.clone(),
                    primary_key: (!keys.is_empty()).then_some(dbine_driver::KeyDef {
                        name: None,
                        columns: keys,
                    }),
                    columns: cols
                        .into_iter()
                        .map(|c| dbine_driver::ColumnDef {
                            name: c.name,
                            data_type: c.data_type,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                };
                let parts = DdlParts {
                    drop: false,
                    if_exists: false,
                    create: true,
                    indexes: false,
                    foreign_keys: false,
                };
                edit::table_ddl(&t, parts).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        edit::browse(&obj.kind, &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let stmts = script::split(text);
        if self.read_only {
            for st in &stmts {
                let why = guard::refusal(&st.text)
                    .or_else(|| (!self.reval && !unenforced_read(&st.text)).then(no_reval));
                if let Some(why) = why {
                    return Err(ScriptError::new(why)
                        .at_offset(st.start)
                        .at_line(st.line)
                        .into());
                }
            }
        }
        let own = out.current_statement.is_none();
        for (i, st) in stmts.iter().enumerate() {
            let step = steps::Step::start(out, own, i, st.start, st.line);
            let r = self.run(&st.text, max_rows, out).await;
            step.end(out, r)?;
        }
        Ok(())
    }

    /// A read the server enforces: one statement, inside `reval`.
    async fn run_read_only(
        &mut self,
        statement: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> Result<()> {
        if !self.reval {
            return Err(Error::Unsupported(
                "este servidor kdb+ no tiene reval (kdb+ 3.6 o posterior): DBine no puede asegurar en el servidor que una consulta solo lea".into(),
            ));
        }
        let stmts = script::split(statement);
        let [one] = stmts.as_slice() else {
            return Err(Error::Query(
                "una lectura de solo lectura es una sola sentencia de q".into(),
            ));
        };
        if let Some(why) = guard::refusal(&one.text) {
            return Err(Error::Query(why));
        }
        if one.text.trim_start().starts_with('\\') {
            return Err(Error::Query(
                "los comandos de sistema no se ejecutan como lecturas de solo lectura".into(),
            ));
        }
        self.cancel.flag.store(false, Ordering::SeqCst);
        let expr = format!("{{{REVAL}}} {}", edit::qstr(&one.text));
        self.emit(&expr, max_rows, out).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let cancel = self.cancel.clone();
        Some(Arc::new(move || {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_waiters();
        }))
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut s = monitor::snapshot(self.q(monitor::SNAPSHOT).await?);
        let procs = monitor::processes(self.q(monitor::CONNECTIONS).await?);
        s.tables.push(monitor::connections_table(&procs));
        Ok(s)
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        Ok(monitor::processes(self.q(monitor::CONNECTIONS).await?))
    }

    /// Close a connection (`hclose`): its handle, as listed.
    async fn kill_session(&mut self, id: &str) -> Result<()> {
        let h: i32 = id
            .trim()
            .parse()
            .map_err(|_| Error::Query(format!("handle de kdb+ inválido: {id}")))?;
        if self.read_only {
            return Err(Error::Query(
                "Conexión de solo lectura: no se cierran conexiones de otros.".into(),
            ));
        }
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.q(&format!("hclose {h}i")).await.map(|_| ())
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Unsupported(
            "kdb+ no permite detener desde un cliente la consulta de otro: se puede cerrar su conexión".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(opts: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn one_driver_with_a_stable_id() {
        let d = drivers();
        assert_eq!(d.len(), 1);
        let i = d[0].info();
        assert_eq!(
            (i.id, i.name, i.default_port, i.dialect),
            ("kdb", "KDB+", 5000, "q")
        );
        assert!(d[0].capabilities().monitor);
    }

    #[test]
    fn new_connections_verify_and_options_apply() {
        let i = info();
        // A new connection: TLS on, verified.
        let new = cfg(&[("tcps", "true"), ("SSL_VERIFY_SERVER", "true")]);
        assert_eq!(new.tls_level(&i.fields), Some(TlsLevel::VerifyFull));
        let (t, w) = tls(&ConnectionConfig {
            host: "db.example".into(),
            ..new
        })
        .unwrap();
        assert!(t.is_some() && w.is_none());
        // SSL_VERIFY_SERVER=NO pins.
        let pinned = ConnectionConfig {
            host: "db.example".into(),
            ..cfg(&[("tcps", "true"), ("SSL_VERIFY_SERVER", "false")])
        };
        let (t, w) = tls(&pinned).unwrap();
        assert!(t.is_some() && w.is_some());
        // Plain TCP.
        let (t, _) = tls(&cfg(&[("tcps", "false")])).unwrap();
        assert!(t.is_none());
        // A CA file that isn't there says which option.
        let bad = ConnectionConfig {
            host: "h".into(),
            ..cfg(&[("tcps", "true"), ("SSL_CA_CERT_FILE", "/nope/ca.pem")])
        };
        let e = tls(&bad).err().unwrap().to_string();
        assert!(e.contains("SSL_CA_CERT_FILE"), "{e}");
    }

    #[test]
    fn explorer_rows_become_objects() {
        let syms = |v: &[&str]| {
            K::Vector(
                0,
                ipc::Vector::Symbol(v.iter().map(|s| s.to_string()).collect()),
            )
        };
        let t = K::Table(
            vec!["ns".into(), "k".into(), "n".into()],
            vec![
                syms(&[".u", ".", ".", ".", "."]),
                syms(&["function", "variable", "partitioned", "table", "view"]),
                syms(&["upd", "x", "quotes", "trade", "v"]),
            ],
        );
        let got: Vec<(String, String)> = explorer_objects(t)
            .into_iter()
            .map(|o| (o.kind, o.name))
            .collect();
        assert_eq!(
            got,
            [
                ("table".to_string(), "trade".to_string()),
                (PARTITIONED.into(), "quotes".into()),
                ("view".into(), "v".into()),
                (VARIABLE.into(), "x".into()),
                ("function".into(), ".u.upd".into()),
            ]
        );
        assert!(explorer_objects(K::Nil).is_empty());
        // kdb+'s own namespaces are the ones left out, and only those.
        assert!(EXPLORER.contains("(key `) except `q`Q`h`j`o`z,`$\"\";"));
    }

    #[test]
    fn the_grid_guard_wraps_the_statement() {
        let g = guarded("value \"til 3\"", 5000);
        assert!(g.starts_with("{[n;b;r] "), "{g}");
        assert!(g.ends_with("}[5000;4194304] value \"til 3\""), "{g}");
        assert!(g.contains("-22!r") && g.contains("`dbine.partial"), "{g}");
    }

    #[test]
    fn without_reval_only_browses_and_show_commands_pass() {
        let d = KdbDriver { info: info() };
        // What DBine writes to open an object, sorted or not.
        for (kind, name) in [
            (kinds::TABLE, "trade"),
            (kinds::VIEW, "v1"),
            (SPLAYED, "daily"),
            (PARTITIONED, "quotes"),
            (VARIABLE, ".u.w"),
            (kinds::TABLE, ".myns.t2"),
        ] {
            let b = edit::browse(kind, name, 100);
            assert!(unenforced_read(&b), "{b}");
            let key = dbine_driver::SortKey {
                column: "px".into(),
                descending: true,
            };
            if kind != VARIABLE {
                let sorted = d.sorted_browse(&b, &[key]).unwrap();
                assert!(unenforced_read(&sorted), "{sorted}");
            }
        }
        for s in ["\\v", "\\f .myns", "\\w"] {
            assert!(unenforced_read(s), "{s}");
        }
        // Anything else, even what reads like a read: nothing on the
        // server would stop it from writing.
        for s in [
            "select from trade",
            "select[100] from trade where px>1",
            "select[100] from (get `$ \"a b\")",
            "100 sublist f[]",
            "select from .Q.ind[quotes; `long$til 100&count quotes]; 0 \"a:1\"",
            "select[100] from trade;0 \"a:1\"",
            "count trade",
            "0 \"a:1\"",
            ".q[`set][`a;1]",
            "get \"a:1\"",
            "1+1",
        ] {
            assert!(!unenforced_read(s), "{s}");
        }
        assert!(no_reval().contains("reval"));
    }

    #[test]
    fn only_tables_take_edits() {
        let r = |kind: &str| ObjectRef {
            kind: kind.into(),
            schema: None,
            name: "t".into(),
        };
        assert!(writable(&r(kinds::TABLE)).is_ok());
        assert!(writable(&r(kinds::VIEW)).is_err());
        assert!(writable(&r(VARIABLE)).is_err());
        for k in [PARTITIONED, SPLAYED] {
            let e = writable(&r(k)).unwrap_err().to_string();
            assert!(e.contains("particionada o splayed"), "{e}");
            let d = KdbDriver { info: info() };
            assert!(d
                .insert_script(&r(k), &["a".into()], &[vec![Value::from(1)]])
                .is_err());
        }
        let d = KdbDriver { info: info() };
        assert!(d.update_script(&r(kinds::VIEW), &[]).is_err());
        assert!(d.delete_script(&r(PARTITIONED), &[]).is_err());
    }
}

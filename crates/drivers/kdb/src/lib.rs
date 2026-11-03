//! kdb+ (KX) over q's IPC protocol (default port 5000), with TLS as q's
//! `:tcps://` connections. Queries are q: each statement of a script goes
//! to the server as a string it evaluates (`value`, through `.z.pg`).
//!
//! The explorer lists the root namespace's tables, views, functions and
//! variables, and those of the user's namespaces (`.ns.t`); single-letter
//! namespaces (`.q`, `.Q`, `.h`, `.z`…) belong to kdb+ and are left out.
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

use ipc::K;

/// Variables that aren't tables, views or functions (lists, dictionaries,
/// atoms).
pub const VARIABLE: &str = "variable";

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

    /// Changing a table's columns in place takes code per change (`update
    /// newcol:… from `t`, `delete col from `t`), and on a database on disk
    /// a rewrite of its splayed files: not offered yet.
    fn sync_script(
        &self,
        _changes: &[dbine_driver::TableChange],
    ) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported(
            "DBine todavía no genera los cambios de estructura de una tabla de kdb+: en una base en disco cada columna es un archivo que hay que reescribir".into(),
        ))
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
        let c = conn::Conn::open(&target).await.map_err(|e| match &watch {
            Some(w) => w.explain(e),
            None => e,
        })?;
        let mut s = KdbSession {
            target,
            conn: Some(c),
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
            s.q("@[{reval parse x;1b};\"1\";0b]").await,
            Ok(K::Bool(true))
        );
        Ok(Box::new(s))
    }
}

/// Only tables take edited rows.
fn writable(target: &ObjectRef) -> Result<()> {
    if target.kind == kinds::TABLE || target.kind.is_empty() {
        Ok(())
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
    read_only: bool,
    /// The server has `reval`.
    reval: bool,
    cancel: Arc<Cancel>,
    tls_pin: Option<String>,
}

/// A q error as the console shows it (`'type`, `'length`).
fn q_error(e: &str) -> Error {
    Error::Statement(Box::new(
        ScriptError::new(format!("'{e}")).with_code(e.to_string()),
    ))
}

impl KdbSession {
    /// Run `text` and return what it gave; a q error is an `Err`.
    async fn q(&mut self, text: &str) -> Result<K> {
        if self.conn.is_none() {
            self.conn = Some(conn::Conn::open(&self.target).await?);
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
        match r {
            None => {
                self.conn = None;
                Err(Error::Cancelled)
            }
            Some(Ok(K::Error(e))) => Err(q_error(&e)),
            Some(Ok(k)) => Ok(k),
            Some(Err(e)) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    /// A list of names the server answered.
    async fn names(&mut self, text: &str) -> Result<Vec<String>> {
        Ok(self.q(text).await?.symbols().unwrap_or_default())
    }

    /// One statement as sent: inside `reval` on a read-only connection
    /// (system commands are already limited to those that show).
    fn wire(&self, stmt: &str) -> String {
        if self.read_only && self.reval && !stmt.trim_start().starts_with('\\') {
            format!("reval parse {}", edit::qstr(stmt))
        } else {
            stmt.to_string()
        }
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let k = self.q(&self.wire(stmt)).await?;
        cells::emit(k, max_rows, out);
        Ok(())
    }

    /// The user's namespaces: `.ns` for each `ns` of more than one letter.
    async fn namespaces(&mut self) -> Result<Vec<String>> {
        Ok(self
            .names("{x where 1<count each string x} key `")
            .await?
            .into_iter()
            .map(|n| format!(".{n}"))
            .collect())
    }
}

/// `name` in namespace `ns` (the root's: as is).
fn qualified(ns: Option<&str>, name: &str) -> String {
    match ns {
        None => name.to_string(),
        Some(ns) => format!("{ns}.{name}"),
    }
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
        let mut out = Vec::new();
        let mut spaces: Vec<Option<String>> = vec![None];
        spaces.extend(self.namespaces().await?.into_iter().map(Some));
        let views = self.names("system \"b\"").await.unwrap_or_default();
        for ns in spaces {
            let arg = ns.as_deref().map_or(String::new(), |n| format!(" {n}"));
            let sym = ns.as_deref().map_or("`.".to_string(), |n| format!("`{n}"));
            let tables = self.names(&format!("tables {sym}")).await?;
            let functions = self.names(&format!("system \"f{arg}\"")).await?;
            let variables = self.names(&format!("system \"v{arg}\"")).await?;
            let mut push = |kind: &str, name: &str| {
                out.push(DbObject {
                    kind: kind.into(),
                    schema: None,
                    name: qualified(ns.as_deref(), name),
                    parent: None,
                })
            };
            for t in &tables {
                push(kinds::TABLE, t);
            }
            if ns.is_none() {
                for v in &views {
                    push(kinds::VIEW, v);
                }
            }
            for f in &functions {
                push(kinds::FUNCTION, f);
            }
            for v in variables.iter().filter(|v| {
                !tables.contains(v)
                    && !functions.contains(v)
                    && (ns.is_some() || !views.contains(v))
            }) {
                push(VARIABLE, v);
            }
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if obj.kind != kinds::TABLE && obj.kind != kinds::VIEW {
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
            kinds::TABLE => {
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
                if let Some(why) = guard::refusal(&st.text) {
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
        let k = self
            .q(&format!("reval parse {}", edit::qstr(&one.text)))
            .await?;
        cells::emit(k, max_rows, out);
        Ok(())
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
    fn only_tables_take_edits() {
        let r = |kind: &str| ObjectRef {
            kind: kind.into(),
            schema: None,
            name: "t".into(),
        };
        assert!(writable(&r(kinds::TABLE)).is_ok());
        assert!(writable(&r(kinds::VIEW)).is_err());
        assert!(writable(&r(VARIABLE)).is_err());
        let d = KdbDriver { info: info() };
        assert!(d.update_script(&r(kinds::VIEW), &[]).is_err());
    }
}

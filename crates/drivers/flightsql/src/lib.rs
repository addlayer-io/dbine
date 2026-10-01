//! Arrow Flight SQL (gRPC): any server that speaks it — GizmoSQL / DuckDB,
//! Dremio (port 32010), InfluxDB 3, Apache Doris, Arrow's example servers…
//! through `arrow-flight`'s `FlightSqlServiceClient` over a tonic channel.
//!
//! Authentication is the Flight handshake with basic credentials (the
//! server answers a bearer token) or a token given directly. Queries run as
//! `CommandStatementQuery` (`GetFlightInfo` then `DoGet` of each endpoint,
//! streamed batch by batch); other statements as `CommandStatementUpdate`.
//! The catalog comes from the standard metadata commands (`GetCatalogs`,
//! `GetTables` with schemas). Cancel drops the stream and sends
//! `CancelFlightInfo` (falling back to the older `CancelQuery` action).
//!
//! Plans and the monitor depend on the engine behind: `EXPLAIN (FORMAT
//! JSON)` trees (DuckDB) or text plans otherwise; DuckDB, Dremio and
//! InfluxDB 3 system tables for the monitor, plus the server's `SqlInfo`.

mod cells;
mod plan;
mod transfer;

use arrow_array::RecordBatch;
use arrow_flight::sql::client::FlightSqlServiceClient;
use arrow_flight::sql::{CommandGetDbSchemas, CommandGetTables, SqlInfo};
use arrow_flight::{Action, CancelFlightInfoRequest, FlightInfo};
use dbine_driver::sql::{
    quote_ident, split_script, split_statements, Quote, ScriptDefaults, ScriptDialect, ScriptMode, StatementKind,
};
use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnInfo, ConnectionConfig, DbObject, Driver, DriverInfo, Error, Family, Field, FieldKind,
    Language, Metric, MetricUnit, MonitorSnapshot, MonitorTable, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn,
    ScriptError, Session, TxState,
};
use futures::TryStreamExt;
use prost::Message;
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(FlightSqlDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "flightsql",
        name: "Arrow Flight SQL",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "standard",
        default_port: 31337,
        fields: vec![
            Field::host(),
            Field::port().placeholder("31337").help("GizmoSQL: 31337; Dremio: 32010; InfluxDB 3: 8181."),
            Field { label: "Catálogo", placeholder: "(el predeterminado)", ..Field::database() },
            Field::username(),
            Field::password(),
            Field::new("token", "Token", FieldKind::Password)
                .secret()
                .help("Token bearer en lugar de usuario y contraseña (InfluxDB 3, Dremio PAT…)."),
            Field::new("headers", "Encabezados", FieldKind::Textarea)
                .placeholder("database: mibase")
                .help("Encabezados gRPC extra, uno por línea (clave: valor). InfluxDB 3 pide `database`.")
                .advanced(),
            Field::encrypt(),
            Field::read_only(),
        ],
        databases_label: "Catálogos",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views()],
    }
}

pub struct FlightSqlDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for FlightSqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// No schema to sync: the DDL depends on the engine behind the protocol.
    fn sync_script(&self, _changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported("Flight SQL es un protocolo, no un motor: el DDL para cambiar el esquema depende de la base que está detrás (DuckDB, Dremio, InfluxDB 3, Doris…) y varias no lo aceptan por Flight SQL; conectate con el driver propio de ese motor para sincronizar esquemas".into()))
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One Flight SQL command per statement (as before, now one call each).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: false, confirm_unsafe_dml: true }
    }

    /// Flight SQL transactions (`BeginTransaction` / `EndTransaction`), on
    /// servers that have them.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// `CommandStatementIngest` (Flight SQL bulk ingest), else a prepared
    /// `INSERT` bound with whole batches (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Flight SQL has no command to create catalogs; the monitor reads
    /// what the engine behind exposes.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Default::default() }
    }

    /// Standard SQL `CREATE SCHEMA` / `DROP SCHEMA … [CASCADE]`, which the
    /// engine behind runs or refuses (DuckDB/GizmoSQL runs them; InfluxDB 3
    /// has no DDL). Flight SQL can't list users, so no owner and no grants.
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(dbine_driver::SchemaSpec { owner: false, owner_kinds: dbine_driver::SchemaOwnerKinds::Both, cascade: true, privileges: Vec::new(), grant_option: true })
    }

    fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        if owner.is_some() {
            return Err(Error::Unsupported("por Flight SQL DBine no conoce los usuarios del motor: creá el esquema sin dueño".into()));
        }
        Ok(format!("CREATE SCHEMA {}", schema_path(database, name)))
    }

    fn drop_schema_script(&self, database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        Ok(format!("DROP SCHEMA {}{}", schema_path(database, name), if cascade { " CASCADE" } else { "" }))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let url = format!("{scheme}://{host}:{}", cfg.port_or(31337));
        let mut ep = Endpoint::from_shared(url.clone())
            .map_err(Error::connect)?
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(30));
        if cfg.encrypt {
            // tonic verifies certificates against the web PKI roots; it has
            // no switch to skip verification, so the form has no "trust" box.
            let tls = ClientTlsConfig::new().with_webpki_roots().domain_name(host.to_string());
            ep = ep.tls_config(tls).map_err(Error::connect)?;
        }
        let channel = tokio::time::timeout(Duration::from_secs(20), ep.connect())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| Error::Connect(format!("{url}: {e}")))?;
        let headers: Vec<(String, String)> = cfg
            .option("headers")
            .unwrap_or("")
            .lines()
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        let mut conn = Conn { channel, token: cfg.option("token").map(str::to_string), headers };
        if conn.token.is_none() {
            if let Some(user) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
                let mut c = conn.client();
                let r = tokio::time::timeout(Duration::from_secs(20), c.handshake(user, cfg.password_or_empty()))
                    .await
                    .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?;
                r.map_err(auth_error)?;
                conn.token = c.token().cloned();
            }
        }
        let catalog = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let mut s = FlightSession {
            conn: Arc::new(conn),
            catalog: catalog.map(str::to_string),
            server: ServerInfo::default(),
            cancel: Arc::new(Cancel::default()),
            rt: tokio::runtime::Handle::current(),
            tx: None,
            dirty: false,
            failed: false,
        };
        s.server = s.sql_info().await.unwrap_or_default();
        // Something every server answers: the catalogs (or a trivial query).
        let check = async {
            match s.conn.client().get_catalogs().await {
                Ok(info) => s.batches(info).await.map(|_| ()),
                Err(_) => s.rows("SELECT 1").await.map(|_| ()),
            }
        };
        tokio::time::timeout(Duration::from_secs(20), check)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        tokio::time::timeout(Duration::from_secs(20), s.use_catalog())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    channel: Channel,
    token: Option<String>,
    headers: Vec<(String, String)>,
}

/// A client that takes messages of any size: tonic's 4 MiB default fails a
/// batch with wide rows (a 6 MB BLOB, 4096 rows of 4 KB text), and the
/// server, not the client, decides how big its batches are.
fn flight_client(channel: Channel) -> FlightSqlServiceClient<Channel> {
    let inner = arrow_flight::flight_service_client::FlightServiceClient::new(channel).max_decoding_message_size(usize::MAX);
    FlightSqlServiceClient::new_from_inner(inner)
}

impl Conn {
    /// A client on the shared channel with the session's token and headers.
    fn client(&self) -> FlightSqlServiceClient<Channel> {
        let mut c = flight_client(self.channel.clone());
        if let Some(t) = &self.token {
            c.set_token(t.clone());
        }
        for (k, v) in &self.headers {
            c.set_header(k.clone(), v.clone());
        }
        c
    }
}

fn auth_error(e: impl std::fmt::Display) -> Error {
    let m = e.to_string();
    if m.contains("Unauthenticated") || m.contains("PermissionDenied") || m.to_ascii_lowercase().contains("auth") {
        Error::AuthFailed(m)
    } else {
        Error::Connect(m)
    }
}

/// The server refuses statements until a rollback (DuckDB's "Current
/// transaction is aborted", PostgreSQL's "current transaction is aborted").
fn is_aborted(m: &str) -> bool {
    m.to_ascii_lowercase().contains("transaction is aborted")
}

/// A statement that failed (or was cancelled) after its columns arrived
/// leaves no empty result set next to its error.
fn drop_empty_result(out: &mut QueryOutcome, before: usize) {
    if out.results.len() > before && out.results[before..].iter().all(|r| r.total_rows == 0 && r.rows_affected.is_none()) {
        out.results.truncate(before);
    }
}

fn flight_error(e: impl std::fmt::Display) -> Error {
    let m = e.to_string();
    if m.contains("Unauthenticated") {
        Error::AuthFailed(m)
    } else if m.contains("Cancelled") && m.contains("status") {
        Error::Cancelled
    } else if m.contains("transport error") || m.contains("Unavailable") {
        Error::Connect(m)
    } else {
        // Strip tonic's wrapping down to the server's message.
        let msg = m.split("message: \"").nth(1).and_then(|r| r.split("\", details").next()).map(|s| s.replace("\\n", "\n").replace("\\\"", "\"")).unwrap_or(m.clone());
        // The engine's error class ("Catalog Error: …" on DuckDB), else the
        // gRPC status ("InvalidArgument"…).
        // GizmoSQL wraps it ("Can't prepare statement: '…' - Error:
        // Catalog Error: …"): the message starts at the class.
        let (class, msg) = match engine_class(&msg) {
            Some((at, class)) => (Some(class), msg[at..].trim_end_matches('"').to_string()),
            None => (None, msg),
        };
        let status = m.split("status: ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_alphanumeric()).next()).filter(|s| !s.is_empty());
        let mut e = ScriptError::new(msg.clone());
        if let Some(code) = class.or(status.map(str::to_string)) {
            e = e.with_code(code);
        }
        if let Some(line) = msg.lines().find_map(|l| l.strip_prefix("LINE ")?.split_once(':')?.0.parse::<u32>().ok()) {
            e = e.at_line(line);
        }
        e.into()
    }
}

/// Where `<Class> Error: ` starts in `msg`, and the class ("Catalog",
/// "Parser"…).
fn engine_class(msg: &str) -> Option<(usize, String)> {
    msg.match_indices(" Error: ").chain(msg.starts_with("Error: ").then_some((0, "")).into_iter()).find_map(|(i, _)| {
        let before = &msg[..i];
        let start = before.rfind(|c: char| !c.is_ascii_alphanumeric()).map_or(0, |p| p + 1);
        let class = &before[start..];
        (!class.is_empty()).then(|| (start, class.to_string()))
    })
}

/// `;` outside quotes and comments; `$$` strings (DuckDB behind GizmoSQL).
fn dialect() -> ScriptDialect {
    ScriptDialect { dollar_quotes: true, compound_blocks: false, backtick_idents: false, ..ScriptDialect::generic() }
}

/// The statements of a script, comments left in place (so the server's
/// line numbers hold), with the line each starts on (0-based).
fn statements(sql: &str) -> Vec<(String, u32)> {
    split_script(sql, &dialect()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand).map(|s| (s.text, s.line - 1)).collect()
}

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
    /// The query in flight.
    info: Mutex<Option<FlightInfo>>,
}

impl Cancel {
    async fn run<T>(&self, f: impl Future<Output = Result<T>>) -> Result<T> {
        let woken = self.notify.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        if self.flag.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            r = f => r,
            _ = woken => Err(Error::Cancelled),
        }
    }
}

/// What `GetSqlInfo` says about the server.
#[derive(Default, Debug, Clone)]
struct ServerInfo {
    name: String,
    version: String,
    arrow_version: String,
    read_only: Option<bool>,
}

impl ServerInfo {
    fn engine(&self) -> Engine {
        let n = self.name.to_ascii_lowercase();
        if n.contains("duckdb") || n.contains("gizmo") || n.contains("sqlite") {
            Engine::DuckDb
        } else if n.contains("dremio") {
            Engine::Dremio
        } else if n.contains("influx") || n.contains("iox") || n.contains("datafusion") {
            Engine::DataFusion
        } else {
            Engine::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    DuckDb,
    Dremio,
    DataFusion,
    Other,
}

pub struct FlightSession {
    conn: Arc<Conn>,
    catalog: Option<String>,
    server: ServerInfo,
    cancel: Arc<Cancel>,
    rt: tokio::runtime::Handle,
    /// Manual transactions: the open Flight SQL transaction.
    tx: Option<tonic::codegen::Bytes>,
    /// Something changed in it.
    dirty: bool,
    /// A statement failed and the server aborted the transaction (DuckDB:
    /// "Current transaction is aborted"): only Rollback gets out of it.
    failed: bool,
}

/// `"catalog"."schema"` (standard SQL) with the catalog the explorer menu
/// was opened on, so the script lands there whatever catalog the session
/// runs in; a bare name on servers without catalogs.
fn schema_path(database: Option<&str>, name: &str) -> String {
    match database {
        Some(c) => format!("{}.{}", quote_ident(Quote::Double, c), quote_ident(Quote::Double, name)),
        None => quote_ident(Quote::Double, name),
    }
}

fn first_word(stmt: &str) -> String {
    let s = stmt.trim_start().trim_start_matches('(');
    s.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

/// Statements that return rows (queries); the rest go as updates.
fn is_query(stmt: &str) -> bool {
    matches!(
        first_word(stmt).as_str(),
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "PRAGMA" | "FROM" | "SUMMARIZE" | "CALL"
    )
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

impl FlightSession {
    /// Every batch of every endpoint of a FlightInfo.
    async fn batches(&self, info: FlightInfo) -> Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        self.stream(info, |b| {
            out.push(b);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Stream the endpoints of `info` batch by batch into `f`.
    async fn stream(&self, info: FlightInfo, mut f: impl FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        for ep in info.endpoint.iter() {
            let Some(ticket) = ep.ticket.clone() else { continue };
            // An endpoint elsewhere needs its own channel.
            let remote = ep.location.iter().map(|l| l.uri.clone()).find(|u| !u.is_empty() && !u.starts_with("arrow-flight-reuse-connection"));
            let mut client = match remote {
                Some(uri) => {
                    let uri = uri.replace("grpc+tcp://", "http://").replace("grpc+tls://", "https://").replace("grpc://", "http://");
                    let ch = Endpoint::from_shared(uri).map_err(Error::connect)?.connect().await.map_err(Error::connect)?;
                    let mut c = flight_client(ch);
                    if let Some(t) = &self.conn.token {
                        c.set_token(t.clone());
                    }
                    c
                }
                None => self.conn.client(),
            };
            let mut s = self.cancel.run(async { client.do_get(ticket).await.map_err(flight_error) }).await?;
            loop {
                let next = self.cancel.run(async { s.try_next().await.map_err(flight_error) }).await?;
                match next {
                    Some(b) => f(b)?,
                    None => break,
                }
            }
        }
        Ok(())
    }

    /// Rows of a query as JSON cells (for catalog and monitor queries).
    async fn rows(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut c = self.conn.client();
        let info = self.cancel.run(async { c.execute(sql.to_string(), None).await.map_err(flight_error) }).await?;
        let schema = info.clone().try_decode_schema().ok();
        let mut cols: Vec<String> = schema.map(|s| s.fields().iter().map(|f| f.name().clone()).collect()).unwrap_or_default();
        let mut rows = Vec::new();
        self.stream(info, |b| {
            if cols.is_empty() {
                cols = b.schema().fields().iter().map(|f| f.name().clone()).collect();
            }
            for r in 0..b.num_rows() {
                rows.push(b.columns().iter().map(|a| cells::cell(a.as_ref(), r)).collect());
            }
            Ok(())
        })
        .await?;
        Ok((cols, rows))
    }

    async fn records(&self, sql: &str) -> Result<Vec<serde_json::Map<String, Value>>> {
        let (cols, rows) = self.rows(sql).await?;
        Ok(rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect())
    }

    async fn sql_info(&self) -> Result<ServerInfo> {
        let mut c = self.conn.client();
        let info = c
            .get_sql_info(vec![SqlInfo::FlightSqlServerName, SqlInfo::FlightSqlServerVersion, SqlInfo::FlightSqlServerArrowVersion, SqlInfo::FlightSqlServerReadOnly])
            .await
            .map_err(flight_error)?;
        let mut out = ServerInfo::default();
        for b in self.batches(info).await? {
            for (id, v) in cells::sql_info_rows(&b) {
                match id {
                    x if x == SqlInfo::FlightSqlServerName as u32 => out.name = text(&v),
                    x if x == SqlInfo::FlightSqlServerVersion as u32 => out.version = text(&v),
                    x if x == SqlInfo::FlightSqlServerArrowVersion as u32 => out.arrow_version = text(&v),
                    x if x == SqlInfo::FlightSqlServerReadOnly as u32 => out.read_only = v.as_bool(),
                    _ => {}
                }
            }
        }
        Ok(out)
    }

    /// Whether the error `e`, in a manual transaction, left it aborted. The
    /// server's text says so on the next statement only ("Current
    /// transaction is aborted"), so a trivial query in the same transaction
    /// finds out; a cancel or a lost connection proves nothing.
    async fn aborted(&mut self, e: Option<&Error>) -> bool {
        match e {
            Some(Error::Cancelled | Error::Connect(_) | Error::AuthFailed(_)) | None => return false,
            Some(e) if is_aborted(&e.to_string()) => return true,
            Some(_) => {}
        }
        let mut probe = QueryOutcome::default();
        match self.run("SELECT 1", 1, &mut probe).await {
            Ok(()) => false,
            Err(e) => is_aborted(&e.to_string()),
        }
    }

    /// A new Flight SQL transaction.
    async fn begin(&self) -> Result<tonic::codegen::Bytes> {
        let mut c = self.conn.client();
        c.begin_transaction().await.map_err(|e| {
            let m = flight_error(e).to_string();
            Error::Unsupported(format!("este servidor Flight SQL no admite transacciones: {m}"))
        })
    }

    /// Commit or roll back the open transaction.
    async fn end(&mut self, commit: bool) -> Result<()> {
        let Some(id) = self.tx.take() else { return Ok(()) };
        let how = if commit { arrow_flight::sql::EndTransaction::Commit } else { arrow_flight::sql::EndTransaction::Rollback };
        let mut c = self.conn.client();
        let r = c.end_transaction(id, how).await.map_err(flight_error);
        self.dirty = false;
        self.failed = false;
        r
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let tx = self.tx.clone();
        if !is_query(stmt) {
            let mut c = self.conn.client();
            let r = self.cancel.run(async { c.execute_update(stmt.to_string(), tx.clone()).await.map_err(flight_error) }).await;
            match r {
                Ok(n) => {
                    out.push_affected(n.max(0) as u64);
                    if self.tx.is_some() {
                        self.dirty = true;
                    }
                    return Ok(());
                }
                // Servers without updates answer them as queries.
                Err(e) if e.is_query() && (e.to_string().contains("Unimplemented") || e.to_string().to_ascii_lowercase().contains("not implemented")) => {}
                Err(e) => return Err(e),
            }
        }
        let mut c = self.conn.client();
        let info = self.cancel.run(async { c.execute(stmt.to_string(), tx).await.map_err(flight_error) }).await?;
        *self.cancel.info.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());
        let schema = info.clone().try_decode_schema().ok();
        let mut started = false;
        if let Some(s) = &schema {
            out.begin_result(s.fields().iter().map(|f| ResultColumn { name: f.name().clone(), type_name: f.data_type().to_string() }).collect());
            started = true;
        }
        let r = self
            .stream(info, |b| {
                if !started {
                    out.begin_result(b.schema().fields().iter().map(|f| ResultColumn { name: f.name().clone(), type_name: f.data_type().to_string() }).collect());
                    started = true;
                }
                for row in 0..b.num_rows() {
                    out.push_row(b.columns().iter().map(|a| cells::cell(a.as_ref(), row)).collect(), max_rows);
                }
                Ok(())
            })
            .await;
        *self.cancel.info.lock().unwrap_or_else(|e| e.into_inner()) = None;
        r?;
        if !started {
            out.begin_result(Vec::new());
        }
        Ok(())
    }

    fn qualified(&self, schema: Option<&str>, name: &str) -> String {
        let mut parts = Vec::new();
        if let Some(c) = self.catalog.as_deref().filter(|c| !c.is_empty()) {
            parts.push(quote_ident(Quote::Double, c));
        }
        if let Some(s) = schema.filter(|s| !s.is_empty()) {
            parts.push(quote_ident(Quote::Double, s));
        }
        parts.push(quote_ident(Quote::Double, name));
        parts.join(".")
    }

    /// Makes the session's catalog the one unqualified statements run in.
    /// Flight SQL has no standard way to pick it, so without this a
    /// DuckDB-backed server (GizmoSQL) runs `CREATE SCHEMA x` or
    /// `DROP SCHEMA x CASCADE` in its default catalog, not the one open in
    /// the explorer. DuckDB's `USE` lasts for the server-side session (each
    /// DBine session has its own). Other engines are left as they are.
    async fn use_catalog(&self) -> Result<()> {
        let Some(c) = self.catalog.as_deref().filter(|c| !c.is_empty()) else { return Ok(()) };
        if self.server.engine() != Engine::DuckDb {
            return Ok(());
        }
        // SQLite behind GizmoSQL has no `current_database()` and one catalog.
        let Ok((_, rows)) = self.rows("SELECT current_database()").await else { return Ok(()) };
        if rows.first().and_then(|r| r.first()).map(text).as_deref() == Some(c) {
            return Ok(());
        }
        let mut cl = self.conn.client();
        let stmt = format!("USE {}", quote_ident(Quote::Double, c));
        self.cancel
            .run(async { cl.execute_update(stmt, None).await.map_err(flight_error) })
            .await
            .map(|_| ())
            .map_err(|e| Error::Connect(format!("no se pudo abrir el catálogo «{c}»: {e}")))
    }

    /// After a `USE`: the catalog the server now runs in (DuckDB-backed
    /// servers say it with `current_database()`), so the explorer and the
    /// tab follow it. Servers without it keep the session's.
    async fn follow_use(&mut self, out: &mut QueryOutcome) {
        let Ok((_, rows)) = self.rows("SELECT current_database()").await else { return };
        let Some(now) = rows.first().and_then(|r| r.first()).map(text).filter(|c| !c.is_empty()) else { return };
        if self.catalog.as_deref() != Some(now.as_str()) {
            self.catalog = Some(now.clone());
            out.database = Some(now);
        }
    }

    /// Tables with their Arrow schemas (`GetTables` with `include_schema`).
    async fn tables(&self, schema: Option<&str>, name: Option<&str>, with_schema: bool) -> Result<Vec<(Option<String>, String, String, Option<arrow_schema::Schema>)>> {
        let mut c = self.conn.client();
        let req = CommandGetTables {
            catalog: self.catalog.clone(),
            db_schema_filter_pattern: schema.map(like_escape),
            table_name_filter_pattern: name.map(like_escape),
            table_types: Vec::new(),
            include_schema: with_schema,
        };
        let info = self.cancel.run(async { c.get_tables(req).await.map_err(flight_error) }).await?;
        let mut out = Vec::new();
        for b in self.batches(info).await? {
            let col = |n: &str| b.column_by_name(n).cloned();
            let (sch, tname, ttype, tschema) = (col("db_schema_name"), col("table_name"), col("table_type"), col("table_schema"));
            for r in 0..b.num_rows() {
                let s = sch.as_ref().map(|a| cells::cell(a.as_ref(), r)).map(|v| text(&v)).filter(|s| !s.is_empty());
                let n = tname.as_ref().map(|a| text(&cells::cell(a.as_ref(), r))).unwrap_or_default();
                let t = ttype.as_ref().map(|a| text(&cells::cell(a.as_ref(), r))).unwrap_or_default();
                let arrow = tschema.as_ref().and_then(|a| cells::binary_at(a.as_ref(), r)).and_then(|bytes| arrow_ipc::convert::try_schema_from_ipc_buffer(&bytes).ok());
                out.push((s, n, t, arrow));
            }
        }
        Ok(out)
    }

    /// DuckDB-style JSON plan (`EXPLAIN (FORMAT JSON)`), else the text plan.
    async fn plan_for(&self, stmt: &str, analyze: bool) -> Result<dbine_driver::Plan> {
        if self.server.engine() == Engine::DuckDb || self.server.engine() == Engine::Other {
            let q = if analyze { format!("EXPLAIN (ANALYZE, FORMAT JSON) {stmt}") } else { format!("EXPLAIN (FORMAT JSON) {stmt}") };
            if let Ok((_, rows)) = self.rows(&q).await {
                if let Some(json) = rows.iter().filter_map(|r| r.last()).map(text).find(|t| t.trim_start().starts_with(['[', '{'])) {
                    if let Ok(v) = serde_json::from_str::<Value>(&json) {
                        return Ok(plan::from_json(stmt, &v, analyze));
                    }
                }
            }
        }
        let q = match (analyze, self.server.engine()) {
            (_, Engine::Dremio) => format!("EXPLAIN PLAN FOR {stmt}"),
            (true, _) => format!("EXPLAIN ANALYZE {stmt}"),
            (false, _) => format!("EXPLAIN {stmt}"),
        };
        let (_, rows) = self.rows(&q).await?;
        let raw = rows.iter().filter_map(|r| r.last()).map(text).collect::<Vec<_>>().join("\n");
        let mut p = dbine_driver::plan::plan_from_text(stmt, &raw, analyze && self.server.engine() != Engine::Dremio);
        if p.root.op.is_empty() {
            p.root.op = "PLAN".into();
        }
        Ok(p)
    }
}

/// A literal name as a `LIKE` pattern (Flight SQL filters are patterns).
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[async_trait]
impl Session for FlightSession {
    async fn server_version(&mut self) -> Result<String> {
        if self.server.name.is_empty() {
            self.server = self.sql_info().await.unwrap_or_default();
        }
        Ok(match (self.server.name.as_str(), self.server.version.as_str()) {
            ("", "") => "Flight SQL".into(),
            (n, v) => format!("{n} {v}").trim().to_string(),
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut c = self.conn.client();
        let info = self.cancel.run(async { c.get_catalogs().await.map_err(flight_error) }).await?;
        let mut out = Vec::new();
        for b in self.batches(info).await? {
            if let Some(a) = b.column_by_name("catalog_name").or_else(|| b.columns().first()) {
                for r in 0..b.num_rows() {
                    let v = text(&cells::cell(a.as_ref(), r));
                    if !v.is_empty() && !v.starts_with("_gizmosql") && v != "system" && v != "temp" {
                        out.push(v);
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(String::new());
        }
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rows = self.tables(None, None, false).await?;
        Ok(rows
            .into_iter()
            .filter(|(s, ..)| !s.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("information_schema") || s == "pg_catalog" || s == "sys"))
            .map(|(schema, name, ty, _)| DbObject {
                kind: if ty.to_ascii_uppercase().contains("VIEW") { kinds::VIEW } else { kinds::TABLE }.into(),
                schema,
                name,
                parent: None,
            })
            .collect())
    }

    /// `GetDbSchemas` of the session's catalog, so an empty schema (one just
    /// made with "Nuevo esquema…") shows too. `None` when the server doesn't
    /// answer it: the explorer derives the schemas from the tables.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let mut c = self.conn.client();
        let req = CommandGetDbSchemas { catalog: self.catalog.clone(), db_schema_filter_pattern: None };
        let Ok(info) = self.cancel.run(async { c.get_db_schemas(req).await.map_err(flight_error) }).await else { return Ok(None) };
        let mut out: Vec<dbine_driver::SchemaInfo> = Vec::new();
        for b in self.batches(info).await? {
            let Some(a) = b.column_by_name("db_schema_name") else { continue };
            for r in 0..b.num_rows() {
                let name = text(&cells::cell(a.as_ref(), r));
                if name.is_empty() || out.iter().any(|s| s.name == name) {
                    continue;
                }
                let system = name.eq_ignore_ascii_case("information_schema") || name == "pg_catalog" || name == "sys";
                out.push(dbine_driver::SchemaInfo { name, system });
            }
        }
        Ok(Some(out))
    }

    /// SQL types and nullability from `information_schema.columns` (DuckDB,
    /// DataFusion, Dremio, Doris…); else the Arrow schema of `GetTables`.
    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let mut sql = format!(
            "SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns WHERE table_name = {}",
            lit(&obj.name)
        );
        if let Some(sc) = obj.schema() {
            sql.push_str(&format!(" AND table_schema = {}", lit(sc)));
        }
        if let Some(c) = self.catalog.as_deref().filter(|c| !c.is_empty()) {
            sql.push_str(&format!(" AND table_catalog = {}", lit(c)));
        }
        sql.push_str(" ORDER BY ordinal_position");
        if let Ok((_, rows)) = self.rows(&sql).await {
            if !rows.is_empty() {
                return Ok(rows
                    .into_iter()
                    .map(|r| ColumnInfo {
                        name: r.first().map(text).unwrap_or_default(),
                        data_type: r.get(1).map(text).unwrap_or_default(),
                        nullable: r.get(2).map(text).is_none_or(|n| n != "NO"),
                        primary_key: false,
                        auto_increment: false,
                        default_value: r.get(3).map(text).filter(|d| !d.is_empty()),
                    })
                    .collect());
            }
        }
        let rows = self.tables(obj.schema(), Some(&obj.name), true).await?;
        if let Some((.., Some(schema))) = rows.into_iter().find(|(_, n, ..)| *n == obj.name) {
            return Ok(schema
                .fields()
                .iter()
                .map(|f| ColumnInfo {
                    name: f.name().clone(),
                    data_type: f.metadata().get("ARROW:FLIGHT:SQL:TYPE_NAME").cloned().unwrap_or_else(|| f.data_type().to_string()),
                    nullable: f.is_nullable(),
                    primary_key: false,
                    auto_increment: false,
                    default_value: None,
                })
                .collect());
        }
        // Servers that don't return schemas: those of an empty query.
        let mut c = self.conn.client();
        let q = format!("SELECT * FROM {} LIMIT 0", self.qualified(obj.schema(), &obj.name));
        let info = self.cancel.run(async { c.execute(q, None).await.map_err(flight_error) }).await?;
        let schema = info.try_decode_schema().map_err(flight_error)?;
        Ok(schema
            .fields()
            .iter()
            .map(|f| ColumnInfo { name: f.name().clone(), data_type: f.data_type().to_string(), nullable: f.is_nullable(), primary_key: false, auto_increment: false, default_value: None })
            .collect())
    }

    /// View source where the engine exposes it (DuckDB's `duckdb_views()`,
    /// `information_schema.views` elsewhere).
    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if obj.kind != kinds::VIEW {
            return Ok(None);
        }
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let sql = match self.server.engine() {
            Engine::DuckDb => format!("SELECT sql FROM duckdb_views() WHERE view_name = {} AND schema_name = {}", lit(&obj.name), lit(obj.schema().unwrap_or("main"))),
            _ => format!(
                "SELECT view_definition FROM information_schema.views WHERE table_name = {} AND table_schema = {}",
                lit(&obj.name),
                lit(obj.schema().unwrap_or(""))
            ),
        };
        match self.rows(&sql).await {
            Ok((_, rows)) => Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).map(|v| text(&v)).filter(|s| !s.is_empty()).map(|d| {
                if d.trim_start().to_ascii_uppercase().starts_with("CREATE") {
                    d
                } else {
                    format!("CREATE VIEW {} AS\n{d};", self.qualified(obj.schema(), &obj.name))
                }
            })),
            Err(e) if e.is_query() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT *\nFROM {}\nLIMIT {limit}", self.qualified(obj.schema(), &obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for (stmt, line) in statements(text) {
            let before = out.results.len();
            let r = self.run(&stmt, max_rows, out).await;
            if r.is_err() {
                drop_empty_result(out, before);
                if self.tx.is_some() && !self.failed {
                    self.failed = self.aborted(r.as_ref().err()).await;
                }
            }
            r.map_err(|e| match e {
                // Lines of the statement become lines of the text.
                Error::Statement(mut se) => {
                    se.line = Some(se.line.unwrap_or(1) + line);
                    Error::Statement(se)
                }
                e => e,
            })?;
            if first_word(&stmt) == "USE" {
                self.follow_use(out).await;
            }
        }
        Ok(())
    }

    /// `Open` when, in manual mode, something changed in the transaction;
    /// `Failed` when an error aborted it.
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(Some(match self.tx {
            Some(_) if self.failed => TxState::Failed,
            Some(_) if self.dirty => TxState::Open,
            _ => TxState::Idle,
        }))
    }

    /// Off: a Flight SQL transaction (`BeginTransaction`) holds every
    /// statement until Commit / Rollback; servers without transactions say
    /// so. On: the pending transaction is committed.
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        match (on, self.tx.is_some()) {
            (false, false) => {
                self.tx = Some(self.begin().await?);
                self.dirty = false;
            }
            (true, true) => {
                self.end(true).await?;
                self.tx = None;
            }
            _ => {}
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        if self.tx.is_some() {
            self.end(true).await?;
            self.tx = Some(self.begin().await?);
        }
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        if self.tx.is_some() {
            self.end(false).await?;
            self.tx = Some(self.begin().await?);
        }
        Ok(())
    }

    /// Estimated: the engine's `EXPLAIN` (a JSON tree on DuckDB, text
    /// elsewhere). Actual: queries run, then `EXPLAIN ANALYZE` (it runs them
    /// again: only reads get there); other statements just run.
    async fn explain(&mut self, script: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for stmt in split_statements(script) {
            let read = matches!(first_word(&stmt).as_str(), "SELECT" | "WITH" | "VALUES" | "FROM" | "TABLE");
            if analyze {
                self.run(&stmt, max_rows, out).await?;
                if read {
                    let p = self.plan_for(&stmt, true).await?;
                    out.plans.push(p);
                }
                continue;
            }
            if read || matches!(first_word(&stmt).as_str(), "INSERT" | "UPDATE" | "DELETE") {
                let p = self.plan_for(&stmt, false).await?;
                out.plans.push(p);
            } else {
                out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.chars().take(80).collect::<String>()));
            }
        }
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let cancel = self.cancel.clone();
        let conn = self.conn.clone();
        let rt = self.rt.clone();
        Some(Arc::new(move || {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_waiters();
            let Some(info) = cancel.info.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                let mut c = conn.client();
                let body = CancelFlightInfoRequest { info: Some(info.clone()) }.encode_to_vec();
                if c.do_action(Action::new("CancelFlightInfo", body)).await.is_err() {
                    // Flight SQL before 15: ActionCancelQueryRequest.
                    let old = arrow_flight::sql::ActionCancelQueryRequest { info: info.encode_to_vec().into() };
                    let any = arrow_flight::sql::Any::pack(&old).map(|a| a.encode_to_vec()).unwrap_or_default();
                    if let Err(e) = c.do_action(Action::new("CancelQuery", any)).await {
                        tracing::debug!("flight sql cancel failed: {e}");
                    }
                }
            });
        }))
    }

    /// Arrow record batches straight into typed cells.
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.transfer_load(spec, source, progress).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        if self.server.name.is_empty() {
            self.server = self.sql_info().await.unwrap_or_default();
        }
        for (k, v) in [("Servidor", &self.server.name), ("Versión", &self.server.version), ("Versión de Arrow", &self.server.arrow_version)] {
            if !v.is_empty() {
                snap.info.push((k.into(), v.clone()));
            }
        }
        if let Some(ro) = self.server.read_only {
            snap.info.push(("Solo lectura (servidor)".into(), if ro { "sí" } else { "no" }.into()));
        }
        use MetricUnit::*;
        match self.server.engine() {
            Engine::DuckDb => {
                let size = self.records("SELECT * FROM pragma_database_size()").await.unwrap_or_default();
                let mem = self.records("SELECT tag, memory_usage_bytes, temporary_storage_bytes FROM duckdb_memory() ORDER BY memory_usage_bytes DESC").await;
                let settings = self
                    .records("SELECT name, value FROM duckdb_settings() WHERE name IN ('threads', 'memory_limit', 'max_memory', 'temp_directory', 'access_mode', 'TimeZone')")
                    .await
                    .unwrap_or_default();
                let sum = |rows: &[serde_json::Map<String, Value>], c: &str| -> Option<f64> {
                    let v: Vec<f64> = rows.iter().filter_map(|r| num(r.get(c))).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                let mem_rows = mem.as_deref().unwrap_or(&[]);
                let limit = settings.iter().find(|r| r.get("name").map(text).as_deref() == Some("memory_limit")).and_then(|r| r.get("value")).map(text).and_then(|v| parse_size(&v));
                let blocks = |c: &str| sum(&size, c);
                snap.metrics = vec![
                    Metric::new("mem_used", "Memoria de DuckDB", "Memoria", Bytes, sum(mem_rows, "memory_usage_bytes")).max(limit),
                    Metric::new("temp_used", "Almacenamiento temporal", "Memoria", Bytes, sum(mem_rows, "temporary_storage_bytes")),
                    Metric::new("storage_used", "Espacio usado", "Almacenamiento", Bytes, blocks("used_blocks").zip(blocks("block_size")).map(|(u, b)| u * b / size.len().max(1) as f64)),
                    Metric::new("wal_size", "WAL", "Almacenamiento", Bytes, size.iter().filter_map(|r| r.get("wal_size").map(text).and_then(|v| parse_size(&v))).reduce(|a, b| a + b)),
                ];
                let mut t = MonitorTable::new("databases", "Bases y tamaños", &["base", "tamaño", "bloques usados", "bloques libres", "WAL", "memoria"]);
                for r in size.iter().take(200) {
                    t.rows.push(["database_name", "database_size", "used_blocks", "free_blocks", "wal_size", "memory_usage"].iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect());
                }
                snap.tables.push(t);
                let mut t = MonitorTable::new("memory", "Memoria por componente", &["componente", "memoria", "temporal"]);
                for r in mem_rows.iter().take(200) {
                    t.rows.push(["tag", "memory_usage_bytes", "temporary_storage_bytes"].iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect());
                }
                snap.tables.push(t);
                for r in &settings {
                    snap.info.push((r.get("name").map(text).unwrap_or_default(), r.get("value").map(text).unwrap_or_default()));
                }
                snap.notes.push("DuckDB corre dentro del servidor Flight SQL: no expone CPU, conexiones ni consultas en curso por SQL.".into());
            }
            Engine::Dremio => {
                let nodes = self.records("SELECT * FROM sys.nodes").await.unwrap_or_default();
                let memory = self.records("SELECT * FROM sys.memory").await.unwrap_or_default();
                let jobs = self
                    .records("SELECT job_id, user_name, status, submitted_ts, query FROM sys.jobs WHERE status NOT IN ('COMPLETED', 'FAILED', 'CANCELED') LIMIT 200")
                    .await
                    .ok();
                let sum = |rows: &[serde_json::Map<String, Value>], c: &str| -> Option<f64> {
                    let v: Vec<f64> = rows.iter().filter_map(|r| num(r.get(c))).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                snap.metrics = vec![
                    Metric::new("cpu", "CPU de los nodos", "CPU", Percent, sum(&nodes, "cpu").map(|s| s / nodes.len().max(1) as f64)),
                    Metric::new("mem_used", "Heap usado", "Memoria", Bytes, sum(&memory, "heap_current")).max(sum(&memory, "heap_max")),
                    Metric::new("direct_used", "Memoria directa usada", "Memoria", Bytes, sum(&memory, "direct_current")).max(sum(&memory, "direct_max")),
                    Metric::new("active_sessions", "Trabajos en curso", "Conexiones", Count, jobs.as_ref().map(|j| j.len() as f64)),
                ];
                let mut t = MonitorTable::new("queries", "Trabajos en curso", &["id", "usuario", "estado", "desde", "consulta"]);
                for r in jobs.iter().flatten() {
                    t.rows.push(vec![
                        r.get("job_id").cloned().unwrap_or(Value::Null),
                        r.get("user_name").cloned().unwrap_or(Value::Null),
                        r.get("status").cloned().unwrap_or(Value::Null),
                        r.get("submitted_ts").cloned().unwrap_or(Value::Null),
                        Value::String(r.get("query").map(text).unwrap_or_default().chars().take(2000).collect()),
                    ]);
                }
                snap.tables.push(t);
                snap.notes.push("Para el monitor completo de Dremio (actividad, nodos, perfiles) usá su conexión REST.".into());
            }
            Engine::DataFusion => match self.records("SELECT * FROM system.queries WHERE running = true LIMIT 200").await {
                Ok(rows) => {
                    snap.metrics = vec![Metric::new("active_sessions", "Consultas en curso", "Conexiones", Count, Some(rows.len() as f64))];
                    let mut t = MonitorTable::new("queries", "Consultas en curso", &["id", "fase", "duración", "consulta"]);
                    for r in &rows {
                        t.rows.push(vec![
                            r.get("id").cloned().unwrap_or(Value::Null),
                            r.get("phase").cloned().unwrap_or(Value::Null),
                            r.get("execute_duration").or_else(|| r.get("elapsed")).cloned().unwrap_or(Value::Null),
                            Value::String(r.get("query_text").map(text).unwrap_or_default().chars().take(2000).collect()),
                        ]);
                    }
                    snap.tables.push(t);
                }
                Err(e) => snap.notes.push(format!("No se pudo leer system.queries ({e}).")),
            },
            Engine::Other => {}
        }
        if snap.metrics.is_empty() {
            snap.notes.push("Flight SQL no define métricas de servidor y este motor no expone tablas de sistema conocidas: solo se muestra la información del servidor.".into());
        }
        Ok(snap)
    }
}

/// `1.5 GiB`, `512.0 MiB`, `16 KB`… in bytes.
fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..split].trim().parse().ok()?;
    let unit = s[split..].trim().to_ascii_lowercase();
    let mult = match unit.as_str() {
        "" | "b" | "bytes" => 1.0,
        "kb" => 1e3,
        "kib" => 1024.0,
        "mb" => 1e6,
        "mib" => 1_048_576.0,
        "gb" => 1e9,
        "gib" => 1_073_741_824.0,
        "tb" => 1e12,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_and_sizes() {
        assert!(is_query("select 1") && is_query(" (SELECT 1)") && is_query("FROM t") && !is_query("CREATE TABLE t (a INT)") && !is_query("insert into t values (1)"));
        assert_eq!(parse_size("1.5 GiB"), Some(1.5 * 1_073_741_824.0));
        assert_eq!(parse_size("16KB"), Some(16_000.0));
        assert_eq!(parse_size("x"), None);
        assert_eq!(like_escape("a_b%"), "a\\_b\\%");
        let s = ServerInfo { name: "GizmoSQL".into(), ..Default::default() };
        assert_eq!(s.engine(), Engine::DuckDb);
        assert_eq!(ServerInfo { name: "Dremio".into(), ..Default::default() }.engine(), Engine::Dremio);
    }

    #[test]
    fn errors() {
        assert!(matches!(flight_error("status: Unauthenticated, message: \"bad\""), Error::AuthFailed(_)));
        let e = flight_error("Tonic error: status: Internal, message: \"Catalog Error: Table with name nope does not exist!\\n\\nLINE 2: select\", details: [], metadata: {}");
        assert!(e.is_query(), "{e:?}");
        let se = e.to_script_error();
        assert_eq!(se.message, "Catalog Error: Table with name nope does not exist!\n\nLINE 2: select");
        assert_eq!((se.code.as_deref(), se.line), (Some("Catalog"), Some(2)));
        let se = flight_error("status: Internal, message: \"Can't prepare statement: 'x' - Error: Parser Error: syntax error\", details: []").to_script_error();
        assert_eq!((se.code.as_deref(), se.message.as_str()), (Some("Parser"), "Parser Error: syntax error"));
        let se = flight_error("status: InvalidArgument, message: \"bad\", details: [], metadata: {}").to_script_error();
        assert_eq!(se.code.as_deref(), Some("InvalidArgument"));
        let units = statements("select $$a;b$$;\n-- c\nselect 2");
        assert_eq!(units, vec![("select $$a;b$$".to_string(), 0), ("select 2".to_string(), 2)]);
    }

    #[test]
    fn one_driver() {
        let d = drivers();
        assert_eq!(d[0].info().id, "flightsql");
        assert!(d[0].supports_explain() && d[0].capabilities().monitor);
    }

    #[test]
    fn schema_scripts() {
        let d = drivers().remove(0);
        let spec = d.schema_spec().unwrap();
        assert!(!spec.owner && spec.cascade && spec.privileges.is_empty());
        assert_eq!(d.create_schema_script(None, "Ven\"tas", None).unwrap(), r#"CREATE SCHEMA "Ven""tas""#);
        assert!(matches!(d.create_schema_script(None, "v", Some("ana")), Err(Error::Unsupported(_))));
        assert_eq!(d.drop_schema_script(None, "v", false).unwrap(), r#"DROP SCHEMA "v""#);
        assert_eq!(d.drop_schema_script(None, "v", true).unwrap(), r#"DROP SCHEMA "v" CASCADE"#);
        assert_eq!(d.create_schema_script(Some("mi\"cat"), "v", None).unwrap(), r#"CREATE SCHEMA "mi""cat"."v""#);
        assert_eq!(d.drop_schema_script(Some("memory"), "v", false).unwrap(), r#"DROP SCHEMA "memory"."v""#);
    }
}

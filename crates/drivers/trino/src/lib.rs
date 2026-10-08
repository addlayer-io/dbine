//! Trino (and Presto, Starburst) through the client REST protocol: `POST
//! /v1/statement`, then follow `nextUri` until the query ends. The session
//! (catalog, schema, `SET SESSION`, prepared statements, transaction) lives
//! in request headers; the server tells us how to update it in response
//! headers. Cancel is a `DELETE` of the current `nextUri`.

mod ddl;
mod literal;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod rename;
mod script;
mod security;
mod stats;
mod sync;
mod transfer;

use base64::Engine as _;
use dbine_driver::sql::{quote_ident, select_top, Limit, Quote};
use dbine_driver::{
    json_bytes, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, Language, ObjectKindInfo, ObjectRef, QueryOutcome,
    ResultColumn, Result, RowChange, SchemaInfo, Session, TableSchema,
};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flavor {
    Trino,
    Presto,
    Starburst,
}

impl Flavor {
    /// `X-Trino-…` or `X-Presto-…`.
    fn header(self, name: &str) -> String {
        match self {
            Flavor::Presto => format!("x-presto-{name}"),
            _ => format!("x-trino-{name}"),
        }
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    [Flavor::Trino, Flavor::Presto, Flavor::Starburst]
        .into_iter()
        .map(|f| Arc::new(TrinoDriver { info: info(f), flavor: f }) as Arc<dyn Driver>)
        .collect()
}

fn info(flavor: Flavor) -> DriverInfo {
    let (id, name) = match flavor {
        Flavor::Trino => ("trino", "Trino"),
        Flavor::Presto => ("presto", "Presto"),
        Flavor::Starburst => ("starburst", "Starburst"),
    };
    DriverInfo {
        id,
        name,
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "trino",
        default_port: 8080,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8080").help("HTTP: 8080; con TLS, 8443 o 443."),
            Field { label: "Catálogo", placeholder: "(ninguno)", ..Field::database() },
            Field::new("schema", "Esquema", dbine_driver::FieldKind::Text).placeholder("(ninguno)"),
            Field::username().required(),
            Field::password().help("Solo con TLS: el servidor rechaza contraseñas por HTTP."),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Catálogos",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::materialized_views()],
    }
}

pub struct TrinoDriver {
    info: DriverInfo,
    flavor: Flavor,
}

#[async_trait]
impl Driver for TrinoDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        script_dialect()
    }

    /// One statement per request; the session (catalog, schema, SET
    /// SESSION, prepared statements, transaction) rides in headers.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    /// START TRANSACTION / COMMIT / ROLLBACK through the transaction header.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// Databases are catalogs, which the server configures (`CREATE
    /// CATALOG` needs a connector and its properties): no create / drop.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, processes: true, cancel_query: true, ..Capabilities::default() }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Large multi-row INSERTs with typed literals, several at once where
    /// the connector allows it (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer(self.flavor))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(ddl::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(self.flavor, changes)
    }

    /// Tables, views, materialized views (not Presto), columns and schemas;
    /// the connector decides what it takes (see [`rename`]).
    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        rename::spec(self.flavor)
    }

    fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<dbine_driver::SyncScript> {
        rename::script(self.flavor, req)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(literal::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(literal::update_script(target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        Ok(literal::delete_script(target.schema(), &target.name, keys))
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Schemas of the session's catalog; the connector decides whether it
    /// takes them (and the owner, `CASCADE`, grants).
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec(self.flavor == Flavor::Presto))
    }

    /// Never with an owner: it's handed over after the grants
    /// (`schema_owner_script`).
    fn create_schema_script(&self, _database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(name)
    }

    /// `ALTER SCHEMA … SET AUTHORIZATION`, after the grants: with
    /// `AUTHORIZATION` in the create, the creator would no longer own the
    /// schema it grants on.
    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        if self.flavor == Flavor::Presto {
            return Err(Error::Query("Presto no asigna dueño a un esquema".into()));
        }
        security::schema_owner(name, owner).map(Some)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        if cascade && self.flavor == Flavor::Presto {
            return Err(Error::Unsupported("Presto solo borra esquemas vacíos (no tiene CASCADE)".into()));
        }
        security::drop_schema(name, cascade, self.flavor == Flavor::Presto)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let port = cfg.port_or(if cfg.encrypt { 8443 } else { 8080 });
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let user = cfg.username.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| "dbine".into());
        let catalog = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let mut s = TrinoSession {
            conn: Arc::new(Conn {
                http,
                base: format!("{scheme}://{host}:{port}"),
                user,
                password: cfg.password.clone().filter(|p| !p.is_empty()),
            }),
            flavor: self.flavor,
            state: SessionState {
                catalog: catalog.map(str::to_string),
                schema: cfg.option("schema").map(str::to_string),
                ..Default::default()
            },
            in_flight: Arc::new(InFlight::default()),
            rt: tokio::runtime::Handle::current(),
            jmx: None,
            profiler: None,
            manual: false,
            last_error: None,
        };
        // Checks the address, the credentials and the catalog.
        let check = async {
            let mut out = QueryOutcome::default();
            let sql = if s.state.catalog.is_some() { "SHOW SCHEMAS" } else { "SELECT 1" };
            s.run(sql, 1, &mut out).await
        };
        tokio::time::timeout(Duration::from_secs(20), check)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    http: reqwest::Client,
    base: String,
    user: String,
    password: Option<String>,
}

impl Conn {
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.password {
            Some(p) => rb.basic_auth(&self.user, Some(p)),
            None => rb,
        }
    }
}

/// What the server asked us to remember between statements.
#[derive(Default, Debug, Clone)]
struct SessionState {
    catalog: Option<String>,
    schema: Option<String>,
    properties: BTreeMap<String, String>,
    prepared: BTreeMap<String, String>,
    transaction: Option<String>,
    /// A statement failed inside the open transaction: the server aborted
    /// it and only a ROLLBACK ends it.
    tx_failed: bool,
}

#[derive(Default)]
struct InFlight {
    next_uri: Mutex<Option<String>>,
    cancelled: AtomicBool,
}

pub struct TrinoSession {
    conn: Arc<Conn>,
    flavor: Flavor,
    state: SessionState,
    in_flight: Arc<InFlight>,
    rt: tokio::runtime::Handle,
    /// The JMX tables the monitor reads, found on its first run.
    jmx: Option<monitor::JmxTables>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// Manual mode: the next statement opens a transaction (`START
    /// TRANSACTION`) that stays open until COMMIT / ROLLBACK.
    manual: bool,
    /// Details of the last failed statement (`run` returns its text only).
    last_error: Option<QueryError>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct QueryResults {
    next_uri: Option<String>,
    columns: Option<Vec<Column>>,
    data: Option<Vec<Vec<Value>>>,
    error: Option<QueryError>,
    update_type: Option<String>,
    update_count: Option<u64>,
    #[serde(default)]
    warnings: Vec<Warning>,
}

#[derive(Deserialize)]
struct Column {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct QueryError {
    message: String,
    #[serde(default)]
    error_name: String,
    #[serde(default)]
    error_location: Option<ErrorLocation>,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
struct ErrorLocation {
    line_number: u32,
    column_number: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Warning {
    message: String,
    #[serde(default)]
    warning_code: Option<WarningCode>,
}

#[derive(Deserialize)]
struct WarningCode {
    #[serde(default)]
    name: String,
}

/// A failed statement with the server's details: its error name as the
/// code and its position in `sql` (the text sent).
fn statement_error(e: &QueryError, sql: &str) -> dbine_driver::ScriptError {
    let (line, col) = match e.error_location {
        Some(l) => (Some(l.line_number), Some(l.column_number)),
        None => script::line_col(&e.message).map_or((None, None), |(l, c)| (Some(l), Some(c))),
    };
    let mut se = dbine_driver::ScriptError::new(e.message.clone());
    if !e.error_name.is_empty() {
        se = se.with_code(e.error_name.clone());
    }
    script::placed(se, sql, line, col)
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

fn query_error(e: QueryError) -> Error {
    match e.error_name.as_str() {
        "USER_CANCELED" => Error::Cancelled,
        "PERMISSION_DENIED" if e.message.contains("uthenticat") => Error::AuthFailed(e.message),
        _ => Error::Query(e.message),
    }
}

/// Header values must be visible ASCII; the protocol URL-encodes the rest.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `SET SESSION`-style `name=value` header values.
fn kv(v: &str) -> Option<(String, String)> {
    let (k, v) = v.split_once('=')?;
    Some((decode(k.trim()), decode(v.trim())))
}

impl SessionState {
    fn headers(&self, flavor: Flavor, user: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let mut put = |name: &str, value: String| {
            if let (Ok(n), Ok(v)) = (reqwest::header::HeaderName::try_from(flavor.header(name)), HeaderValue::from_str(&value)) {
                h.append(n, v);
            }
        };
        put("user", user.to_string());
        put("source", "DBine".into());
        if let Some(c) = &self.catalog {
            put("catalog", c.clone());
        }
        if let Some(s) = &self.schema {
            put("schema", s.clone());
        }
        for (k, v) in &self.properties {
            put("session", format!("{k}={}", encode(v)));
        }
        for (k, v) in &self.prepared {
            put("prepared-statement", format!("{}={}", encode(k), encode(v)));
        }
        put("transaction-id", self.transaction.clone().unwrap_or_else(|| "NONE".into()));
        h
    }

    /// Apply the `X-Trino-Set-*` / `Clear-*` response headers.
    fn update(&mut self, flavor: Flavor, h: &HeaderMap) {
        let all = |name: &str| -> Vec<String> {
            h.get_all(flavor.header(name)).iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect()
        };
        if let Some(c) = all("set-catalog").pop() {
            self.catalog = Some(c);
        }
        if let Some(s) = all("set-schema").pop() {
            self.schema = Some(s);
        }
        for v in all("set-session") {
            if let Some((k, v)) = kv(&v) {
                self.properties.insert(k, v);
            }
        }
        for k in all("clear-session") {
            self.properties.remove(k.trim());
        }
        for v in all("added-prepare") {
            if let Some((k, v)) = kv(&v) {
                self.prepared.insert(k, v);
            }
        }
        for k in all("deallocated-prepare") {
            self.prepared.remove(&decode(k.trim()));
        }
        if let Some(t) = all("started-transaction-id").pop() {
            self.transaction = Some(t);
            self.tx_failed = false;
        }
        if !all("clear-transaction-id").is_empty() {
            self.transaction = None;
            self.tx_failed = false;
        }
    }
}

impl TrinoSession {
    /// Run one statement to the end, appending its result to `out`.
    async fn run(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.in_flight.cancelled.store(false, Ordering::SeqCst);
        self.last_error = None;
        let catalog = self.state.catalog.clone();
        let rb = self
            .conn
            .http
            .post(format!("{}/v1/statement", self.conn.base))
            .headers(self.state.headers(self.flavor, &self.conn.user))
            .body(sql.to_string());
        let mut resp = self.conn.auth(rb).send().await.map_err(http_error)?;
        let mut started = false;
        let res = loop {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
                let text = resp.text().await.unwrap_or_default();
                break Err(Error::AuthFailed(if text.trim().is_empty() { format!("HTTP {status}") } else { text.trim().to_string() }));
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                break Err(if self.in_flight.cancelled.load(Ordering::SeqCst) {
                    Error::Cancelled
                } else {
                    Error::Query(format!("HTTP {status}: {}", text.trim()))
                });
            }
            self.state.update(self.flavor, resp.headers());
            let page: QueryResults = match resp.json().await {
                Ok(p) => p,
                Err(e) => break Err(Error::Query(e.to_string())),
            };
            if let Some(e) = page.error {
                // Any failure inside a transaction aborts it on the server.
                self.state.tx_failed |= self.state.transaction.is_some();
                self.last_error = Some(e.clone());
                break Err(query_error(e));
            }
            if !started {
                if let Some(cols) = &page.columns {
                    if page.update_type.is_none() {
                        out.begin_result(cols.iter().map(|c| ResultColumn { name: c.name.clone(), type_name: c.ty.clone() }).collect());
                        started = true;
                    }
                }
            }
            if started {
                let types: Vec<String> = out.results.last().map(|r| r.columns.iter().map(|c| c.type_name.clone()).collect()).unwrap_or_default();
                for row in page.data.unwrap_or_default() {
                    out.push_row(row.into_iter().enumerate().map(|(i, v)| cell(v, types.get(i).map_or("", String::as_str))).collect(), max_rows);
                }
            }
            for w in page.warnings {
                let code = w.warning_code.map(|c| c.name).filter(|n| !n.is_empty());
                out.message(dbine_driver::Message { level: dbine_driver::MessageLevel::Warning, text: w.message, code, ..Default::default() });
            }
            let Some(next) = page.next_uri else {
                if !started {
                    if page.update_type.is_some() {
                        out.push_affected(page.update_count.unwrap_or(0));
                    } else {
                        out.begin_result(Vec::new());
                    }
                }
                if let (Some(tag), Some(last)) = (&page.update_type, out.results.last_mut()) {
                    last.tag = Some(tag.clone());
                }
                if self.state.catalog != catalog {
                    // USE moved the session to another catalog: the tab follows.
                    out.database = self.state.catalog.clone();
                }
                break Ok(());
            };
            *self.in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = Some(next.clone());
            if self.in_flight.cancelled.load(Ordering::SeqCst) {
                break Err(Error::Cancelled);
            }
            resp = match self.conn.auth(self.conn.http.get(&next)).send().await {
                Ok(r) => r,
                Err(e) => break Err(http_error(e)),
            };
        };
        *self.in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A statement stopped before its first row (cancelled, failed) has
        // no result set to show: only the error.
        if res.is_err() && started && out.results.last().is_some_and(|r| r.rows.is_empty()) {
            out.results.pop();
        }
        res
    }

    /// A catalog query's rows as strings.
    async fn strings(&mut self, sql: &str) -> Result<Vec<Vec<String>>> {
        let mut out = QueryOutcome::default();
        self.run(sql, 1_000_000, &mut out).await?;
        Ok(out
            .results
            .pop()
            .map(|r| r.rows.into_iter().map(|row| row.iter().map(text).collect()).collect())
            .unwrap_or_default())
    }

    /// The single text cell EXPLAIN returns.
    async fn plan_text(&mut self, sql: &str) -> Result<String> {
        let rows = self.strings(sql).await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).collect::<Vec<_>>().join("\n"))
    }

    async fn estimated_plan(&mut self, stmt: &str) -> Result<dbine_driver::Plan> {
        match self.plan_text(&format!("EXPLAIN (TYPE DISTRIBUTED, FORMAT JSON) {stmt}")).await {
            Ok(raw) => plan::plan_json(stmt, &raw).map_err(Error::Query),
            Err(Error::Query(e)) => {
                tracing::debug!("{:?}: EXPLAIN FORMAT JSON refused, trying text: {e}", self.flavor);
                let raw = self.plan_text(&format!("EXPLAIN (TYPE DISTRIBUTED) {stmt}")).await?;
                let mut p = plan::analyze_text(stmt, &raw);
                p.actual = false;
                Ok(p)
            }
            Err(e) => Err(e),
        }
    }

    fn catalog(&self) -> Result<String> {
        self.state.catalog.clone().ok_or_else(|| Error::Query("Elegí un catálogo para ver sus objetos.".into()))
    }

    /// An editor statement: in manual mode, opens the transaction first;
    /// a failure comes back with the server's error name and position.
    async fn run_statement(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let (w1, w2) = script::head(stmt);
        let tx_control = matches!(w1.as_str(), "COMMIT" | "ROLLBACK") || (w1 == "START" && w2 == "TRANSACTION");
        if self.manual && self.state.transaction.is_none() && !tx_control {
            self.run("START TRANSACTION", 1, &mut QueryOutcome::default()).await?;
        }
        let r = self.run(stmt, max_rows, out).await;
        if r.is_err() && matches!(w1.as_str(), "COMMIT" | "ROLLBACK") {
            // A failed COMMIT / ROLLBACK ends the transaction on the server.
            self.state.transaction = None;
            self.state.tx_failed = false;
        }
        match r {
            Err(Error::Query(m)) => match self.last_error.take() {
                Some(e) => Err(statement_error(&e, stmt).into()),
                None => Err(Error::Query(m)),
            },
            other => other,
        }
    }

    /// COMMIT or ROLLBACK of the open transaction, if any.
    async fn end_transaction(&mut self, sql: &str) -> Result<()> {
        if self.state.transaction.is_none() {
            return Ok(());
        }
        let r = self.run(sql, 1, &mut QueryOutcome::default()).await;
        if r.is_err() {
            // The server forgot it (it failed or expired): a failed COMMIT
            // ends it too ("Current transaction has already been aborted").
            self.state.transaction = None;
            self.state.tx_failed = false;
        }
        r
    }
}

/// The trino CLI's reading of a script: `;` outside '…', "…" and
/// comments; no backtick identifiers.
fn script_dialect() -> dbine_driver::ScriptDialect {
    dbine_driver::ScriptDialect { backtick_idents: false, ..dbine_driver::ScriptDialect::generic() }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A string literal: quotes doubled.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A cell as the UI wants it, given its Trino type.
fn cell(v: Value, ty: &str) -> Value {
    match v {
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => Value::Number(n),
        },
        Value::String(s) if ty == "varbinary" => match base64::engine::general_purpose::STANDARD.decode(&s) {
            Ok(b) => json_bytes(&b),
            Err(_) => Value::String(s),
        },
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v,
    }
}

#[async_trait]
impl Session for TrinoSession {
    async fn server_version(&mut self) -> Result<String> {
        let rb = self.conn.http.get(format!("{}/v1/info", self.conn.base));
        let info: Value = self.conn.auth(rb).send().await.map_err(http_error)?.json().await.map_err(Error::query)?;
        let v = info.pointer("/nodeVersion/version").map(text).unwrap_or_default();
        Ok(format!("{} {v}", info_name(self.flavor)))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.strings("SHOW CATALOGS").await?;
        Ok(rows.into_iter().filter_map(|mut r| (!r.is_empty()).then(|| r.remove(0))).filter(|c| c != "system").collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let cat = self.catalog()?;
        let rows = self
            .strings(&format!(
                "SELECT table_schema, table_name, table_type FROM {}.information_schema.tables
                 WHERE table_schema <> 'information_schema' ORDER BY 1, 2",
                quote_ident(Quote::Double, &cat)
            ))
            .await?;
        // Materialized views also show up as tables in information_schema.
        let mvs: Vec<(String, String)> = self
            .strings(&format!(
                "SELECT schema_name, name FROM system.metadata.materialized_views WHERE catalog_name = {}",
                lit(&cat)
            ))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.len() == 2)
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect();
        Ok(rows
            .into_iter()
            .filter(|r| r.len() == 3)
            .map(|r| {
                let is_mv = mvs.iter().any(|(s, n)| *s == r[0] && *n == r[1]);
                let kind = if is_mv {
                    kinds::MATERIALIZED_VIEW
                } else if r[2] == "VIEW" {
                    kinds::VIEW
                } else {
                    kinds::TABLE
                };
                DbObject { kind: kind.into(), schema: Some(r[0].clone()), name: r[1].clone(), parent: None }
            })
            .collect())
    }

    /// The catalog's `information_schema.schemata`; information_schema is
    /// the system one.
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let Ok(cat) = self.catalog() else { return Ok(None) };
        let rows = self.strings(&format!("SELECT schema_name FROM {}.information_schema.schemata ORDER BY 1", quote_ident(Quote::Double, &cat))).await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|r| r.into_iter().next())
                .map(|name| SchemaInfo { system: name == "information_schema", name })
                .collect(),
        ))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let cat = self.catalog()?;
        let schema = obj.schema().map(str::to_string).or(self.state.schema.clone()).unwrap_or_default();
        let rows = self
            .strings(&format!(
                "SELECT column_name, data_type, is_nullable, column_default FROM {}.information_schema.columns
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                quote_ident(Quote::Double, &cat),
                lit(&schema),
                lit(&obj.name)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.len() == 4)
            .map(|r| ColumnInfo {
                name: r[0].clone(),
                data_type: r[1].clone(),
                nullable: r[2] == "YES",
                primary_key: false,
                auto_increment: false,
                default_value: (!r[3].is_empty()).then(|| r[3].clone()),
            })
            .collect())
    }

    /// Tables of every schema of the catalog: columns (with comments) from
    /// `system.jdbc.columns`, defaults from `information_schema.columns`,
    /// table comments from `system.metadata.table_comments`.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let cat = self.catalog()?;
        let mut tables: Vec<TableSchema> = self
            .list_objects()
            .await?
            .into_iter()
            .filter(|o| o.kind == kinds::TABLE)
            .map(|o| TableSchema { kind: o.kind, schema: o.schema, name: o.name, ..Default::default() })
            .collect();
        let rows = self
            .strings(&format!(
                "SELECT c.table_schem, c.table_name, c.column_name, c.type_name, c.is_nullable, c.remarks, i.column_default
                 FROM system.jdbc.columns c
                 LEFT JOIN {}.information_schema.columns i
                   ON i.table_schema = c.table_schem AND i.table_name = c.table_name AND i.column_name = c.column_name
                 WHERE c.table_cat = {} AND c.table_schem <> 'information_schema'
                 ORDER BY c.table_schem, c.table_name, c.ordinal_position",
                quote_ident(Quote::Double, &cat),
                lit(&cat)
            ))
            .await?;
        let comments = self
            .strings(&format!(
                "SELECT schema_name, table_name, comment FROM system.metadata.table_comments
                 WHERE catalog_name = {} AND schema_name <> 'information_schema' AND comment IS NOT NULL",
                lit(&cat)
            ))
            .await
            .unwrap_or_default();
        let index: std::collections::HashMap<(String, String), usize> =
            tables.iter().enumerate().map(|(i, t)| ((t.schema.clone().unwrap_or_default(), t.name.clone()), i)).collect();
        let find = |s: &str, n: &str| index.get(&(s.to_string(), n.to_string())).copied();
        for r in rows.into_iter().filter(|r| r.len() == 7) {
            if let Some(i) = find(&r[0], &r[1]) {
                tables[i].columns.push(ColumnDef {
                    name: r[2].clone(),
                    data_type: r[3].clone(),
                    nullable: r[4] != "NO",
                    default_value: (!r[6].is_empty()).then(|| r[6].clone()),
                    comment: (!r[5].is_empty()).then(|| r[5].clone()),
                    ..Default::default()
                });
            }
        }
        for r in comments.into_iter().filter(|r| r.len() == 3 && !r[2].is_empty()) {
            if let Some(i) = find(&r[0], &r[1]) {
                tables[i].comment = Some(r[2].clone());
            }
        }
        Ok(tables)
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let what = match obj.kind.as_str() {
            kinds::TABLE => "TABLE",
            kinds::VIEW => "VIEW",
            kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
            _ => return Ok(None),
        };
        let cat = self.catalog()?;
        let schema = obj.schema().map(str::to_string).or(self.state.schema.clone()).unwrap_or_default();
        let name = [cat.as_str(), &schema, &obj.name].iter().map(|p| quote_ident(Quote::Double, p)).collect::<Vec<_>>().join(".");
        let rows = self.strings(&format!("SHOW CREATE {what} {name}")).await?;
        Ok(rows.into_iter().next().and_then(|mut r| (!r.is_empty()).then(|| r.remove(0))))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    /// One statement per request (the app sends them one by one). A
    /// script handed whole runs statement by statement, stopping at the
    /// first error, as `trino -f` does.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let dialect = script_dialect();
        for unit in script::units(text, &dialect) {
            self.run_statement(&unit.text, max_rows, out).await.map_err(|e| script::shift(e, &unit))?;
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<Option<dbine_driver::TxState>> {
        Ok(Some(match (&self.state.transaction, self.state.tx_failed) {
            (None, _) => dbine_driver::TxState::Idle,
            (Some(_), false) => dbine_driver::TxState::Open,
            (Some(_), true) => dbine_driver::TxState::Failed,
        }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.manual = !on;
        if on && self.state.transaction.is_some() {
            self.end_transaction("COMMIT").await?;
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.end_transaction("COMMIT").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.end_transaction("ROLLBACK").await
    }

    /// Plans per statement. Estimated: the distributed `EXPLAIN (FORMAT
    /// JSON)` (text `EXPLAIN` on servers without it), nothing runs.
    /// Actual: each statement runs as with `execute`; a read then runs a
    /// second time under `EXPLAIN ANALYZE` (text only) for its figures,
    /// while a write gets its estimated plan before running.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        use plan::StmtKind;
        for unit in script::units(sql, &script_dialect()) {
            let stmt = unit.text;
            match (analyze, plan::classify(&stmt)) {
                (false, StmtKind::Other) => out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt))),
                (false, _) => {
                    let p = self.estimated_plan(&stmt).await?;
                    out.plans.push(p);
                }
                (true, StmtKind::Read) => {
                    self.run(&stmt, max_rows, out).await?;
                    let raw = self.plan_text(&format!("EXPLAIN ANALYZE {stmt}")).await?;
                    out.plans.push(plan::analyze_text(&stmt, &raw));
                }
                (true, StmtKind::Other) => self.run(&stmt, max_rows, out).await?,
                (true, StmtKind::Write) => {
                    let p = self.estimated_plan(&stmt).await?;
                    out.plans.push(p);
                    self.run(&stmt, max_rows, out).await?;
                }
            }
        }
        Ok(())
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn row_estimates(&mut self) -> Result<Vec<dbine_driver::stats::RowEstimate>> {
        stats::row_estimates(self).await
    }

    async fn object_comments(&mut self) -> Result<Vec<dbine_driver::stats::ObjectComment>> {
        stats::object_comments(self).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    /// The queries in flight (`system.runtime.queries`): the coordinator
    /// has no sessions to list.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.processes_list().await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_running(id).await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = self.profiler_begin(opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = self.profiler_next(&mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        // It changes no server setting.
        self.profiler = None;
        Ok(())
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, columns, source, progress).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let conn = self.conn.clone();
        let in_flight = self.in_flight.clone();
        let rt = self.rt.clone();
        Some(Arc::new(move || {
            in_flight.cancelled.store(true, Ordering::SeqCst);
            let Some(uri) = in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                if let Err(e) = conn.auth(conn.http.delete(&uri)).send().await {
                    tracing::debug!("trino cancel failed: {e}");
                }
            });
        }))
    }

    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self).await
    }
}

fn info_name(f: Flavor) -> &'static str {
    match f {
        Flavor::Trino => "Trino",
        Flavor::Presto => "Presto",
        Flavor::Starburst => "Starburst",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Con opción de otorgar" is offered on the new schema's grants
    /// exactly where the engine writes them (`SchemaSpec::grant_option`).
    #[test]
    fn schema_grant_option_matches_the_script() {
        for d in crate::drivers() {
            let Some(spec) = d.schema_spec() else { continue };
            let Some(p) = spec.privileges.first() else { continue };
            let grant = |grantable| d.schema_grant_script(Some("memory"), "ventas", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }
    use serde_json::json;

    /// "Nuevo esquema…" with an owner: created by the user, the grants,
    /// then `SET AUTHORIZATION`; Presto has no owner.
    #[test]
    fn schema_owner_goes_after_the_grants() {
        let ds = drivers();
        let trino = ds.iter().find(|d| d.info().id == "trino").unwrap();
        assert_eq!(trino.create_schema_script(Some("hive"), "ventas", Some("ana")).unwrap(), "CREATE SCHEMA \"ventas\";");
        assert_eq!(
            trino.schema_owner_script(Some("hive"), "ventas", "ana").unwrap().as_deref(),
            Some("ALTER SCHEMA \"ventas\" SET AUTHORIZATION USER \"ana\";")
        );
        assert_eq!(
            trino.schema_owner_script(None, "ventas", "lect IN hive").unwrap().as_deref(),
            Some("ALTER SCHEMA \"ventas\" SET AUTHORIZATION ROLE \"lect\";")
        );
        assert!(trino.schema_owner_script(None, "ventas", " ").is_err());
        let presto = ds.iter().find(|d| d.info().id == "presto").unwrap();
        assert!(matches!(presto.schema_owner_script(None, "ventas", "ana"), Err(Error::Query(_))));
    }

    #[test]
    fn cells_follow_their_types() {
        assert_eq!(cell(json!(9007199254740993i64), "bigint"), json!("9007199254740993"));
        assert_eq!(cell(json!(1), "integer"), json!(1));
        assert_eq!(cell(json!("1.50"), "decimal(10,2)"), json!("1.50"));
        assert_eq!(cell(json!("yv4="), "varbinary"), json!("0xCAFE"));
        assert_eq!(cell(json!([1, 2]), "array(integer)"), json!("[1,2]"));
        assert_eq!(cell(json!({"a": 1}), "map(varchar, integer)"), json!("{\"a\":1}"));
    }

    #[test]
    fn session_headers_round_trip() {
        let mut st = SessionState { catalog: Some("memory".into()), ..Default::default() };
        let mut h = HeaderMap::new();
        h.append("x-trino-set-schema", HeaderValue::from_static("s1"));
        h.append("x-trino-set-session", HeaderValue::from_static("query_max_run_time=1h"));
        h.append("x-trino-added-prepare", HeaderValue::from_static("q1=SELECT+%3F"));
        h.append("x-trino-started-transaction-id", HeaderValue::from_static("tx1"));
        st.update(Flavor::Trino, &h);
        assert_eq!(st.schema.as_deref(), Some("s1"));
        assert_eq!(st.properties.get("query_max_run_time").map(String::as_str), Some("1h"));
        assert_eq!(st.prepared.get("q1").map(String::as_str), Some("SELECT ?"));
        let out = st.headers(Flavor::Presto, "me");
        assert_eq!(out.get("x-presto-user").unwrap(), "me");
        assert_eq!(out.get("x-presto-transaction-id").unwrap(), "tx1");
        assert_eq!(out.get("x-presto-prepared-statement").unwrap(), "q1=SELECT%20%3F");
        let mut h = HeaderMap::new();
        h.append("x-trino-clear-transaction-id", HeaderValue::from_static("true"));
        h.append("x-trino-deallocated-prepare", HeaderValue::from_static("q1"));
        st.update(Flavor::Trino, &h);
        assert!(st.transaction.is_none() && st.prepared.is_empty());
    }

    #[test]
    fn errors_are_classified() {
        let e = query_error(QueryError { message: "Query was canceled".into(), error_name: "USER_CANCELED".into(), ..Default::default() });
        assert!(matches!(e, Error::Cancelled));
        // The position comes from the message when there's no errorLocation.
        let qe = QueryError { message: "line 2:6: Table 'x' does not exist".into(), error_name: "TABLE_NOT_FOUND".into(), ..Default::default() };
        assert!(matches!(query_error(qe.clone()), Error::Query(_)));
        let se = statement_error(&qe, "select *\nfrom x");
        assert_eq!((se.code.as_deref(), se.line, se.offset), (Some("TABLE_NOT_FOUND"), Some(2), Some(14)));
        // errorLocation wins over the message.
        let qe = QueryError { error_location: Some(ErrorLocation { line_number: 1, column_number: 8 }), ..qe };
        let se = statement_error(&qe, "select *\nfrom x");
        assert_eq!((se.line, se.offset), (Some(1), Some(7)));
        assert_eq!(lit("o'k"), "'o''k'");
    }
}

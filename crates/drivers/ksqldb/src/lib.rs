//! ksqlDB through its REST API (8088). Statements go to `/ksql`, one per
//! request; `SELECT`s go to `/query-stream` (newline-delimited rows) and
//! `PRINT` to `/query`. Push queries (`EMIT CHANGES`) never end on their own:
//! they stop at `max_rows` or after a time cap, and the connection is closed.
//!
//! The REST API is stateless: `SET` / `UNSET` of properties are kept here
//! and sent with every request, as the ksqlDB CLI does.

use dbine_driver::sql::{quote_ident, split_statements, Quote};
use dbine_driver::{
    json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver,
    DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, QueryOutcome, ResultColumn, Result,
    RowChange, Session, TableSchema,
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

mod ddl;
mod sync;
mod monitor;
mod plan;
mod transfer;

const KSQL_JSON: &str = "application/vnd.ksql.v1+json";
const DELIMITED: &str = "application/vnd.ksqlapi.delimited.v1";
/// The processing-log stream ksqlDB creates for itself.
const PROCESSING_LOG: &str = "KSQL_PROCESSING_LOG";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(KsqlDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "ksqldb",
        name: "ksqlDB",
        family: Family::Streaming,
        language: Language::Sql,
        dialect: "ksql",
        default_port: 8088,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8088"),
            Field::username(),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::new("push_timeout", "Límite de las consultas push (s)", FieldKind::Number)
                .default_value("10")
                .help("Una consulta EMIT CHANGES se corta al llegar al máximo de filas o a este tiempo.")
                .advanced(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(kinds::STREAM, "Streams", true, true, true),
            ObjectKindInfo::tables(),
            ObjectKindInfo::new(kinds::TOPIC, "Topics", false, true, false),
        ],
    }
}

pub struct KsqlDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for KsqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// `/inserts-stream` (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// ksqlDB is a single namespace (no databases to create or drop) and
    /// has no foreign keys.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Capabilities::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        if target.kind == kinds::TOPIC {
            return Err(Error::Unsupported("ksqlDB no inserta en topics: hay que hacerlo en un stream".into()));
        }
        Ok(ddl::insert_script(&target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        ddl::update_script(&target.kind, &target.name, changes)
    }

    /// ksqlDB has no DELETE: a table row goes away with a tombstone (a
    /// null-valued record for its key) written to the Kafka topic, which no
    /// statement can produce, and streams are append-only.
    fn delete_script(&self, _target: &ObjectRef, _keys: &[Vec<(String, Value)>]) -> Result<String> {
        Err(Error::Unsupported(
            "ksqlDB no tiene DELETE: una fila de una tabla se borra con un tombstone (un registro con valor nulo para su clave) escrito directamente en el topic de Kafka, y los streams solo admiten agregar eventos".into(),
        ))
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let mut s = KsqlSession {
            conn: Arc::new(Conn {
                http,
                base: format!("{scheme}://{host}:{}", cfg.port_or(8088)),
                user: cfg.username.clone().filter(|u| !u.is_empty()),
                password: cfg.password.clone(),
            }),
            properties: BTreeMap::new(),
            push_timeout: Duration::from_secs(cfg.option("push_timeout").and_then(|v| v.parse().ok()).unwrap_or(10)),
            in_flight: Arc::new(InFlight::default()),
            rt: tokio::runtime::Handle::current(),
        };
        // /info doesn't check credentials; a harmless statement does.
        tokio::time::timeout(Duration::from_secs(20), s.ksql("SHOW PROPERTIES;"))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    http: reqwest::Client,
    base: String,
    user: Option<String>,
    password: Option<String>,
}

impl Conn {
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.post(format!("{}{path}", self.base)))
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.user {
            Some(u) => rb.basic_auth(u, self.password.as_deref()),
            None => rb,
        }
    }
}

#[derive(Default)]
struct InFlight {
    query_id: Mutex<Option<String>>,
    cancelled: AtomicBool,
    notify: Notify,
}

pub struct KsqlSession {
    conn: Arc<Conn>,
    /// Streams properties set with `SET` (e.g. `auto.offset.reset`).
    properties: BTreeMap<String, String>,
    push_timeout: Duration,
    in_flight: Arc<InFlight>,
    rt: tokio::runtime::Handle,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

/// A non-2xx answer: `{"@type": "statement_error", "message": …}`.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| if text.trim().is_empty() { format!("HTTP {status}") } else { text.trim().to_string() });
    Err(match status.as_u16() {
        401 | 403 => Error::AuthFailed(msg),
        _ => Error::Query(msg),
    })
}

/// What kind of request a statement needs.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Query { push: bool },
    Print,
    Set(String, String),
    Unset(String),
    Ksql,
}

fn words(stmt: &str) -> Vec<String> {
    stmt.split_whitespace().map(|w| w.to_ascii_uppercase()).collect()
}

/// `'text'` → `text`.
fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).map_or_else(|| s.to_string(), |s| s.replace("''", "'"))
}

/// Statements `EXPLAIN` takes: queries and the ones that start a
/// persistent query (`CREATE … AS SELECT`, `INSERT INTO … SELECT`).
fn explainable(stmt: &str) -> bool {
    let w = words(stmt);
    match w.first().map(String::as_str) {
        Some("SELECT") => true,
        Some("CREATE" | "INSERT") => w.iter().any(|x| x == "SELECT"),
        _ => false,
    }
}

/// A statement cut for a message.
fn short(stmt: &str) -> String {
    let one = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(60).collect::<String>())
    } else {
        one
    }
}

fn route(stmt: &str) -> Route {
    let w = words(stmt);
    match w.first().map(String::as_str) {
        Some("SELECT") => Route::Query { push: w.windows(2).any(|p| p[0] == "EMIT" && p[1] == "CHANGES") },
        Some("PRINT") => Route::Print,
        Some("SET") => {
            let rest = stmt.trim()[3..].trim();
            match rest.split_once('=') {
                Some((k, v)) => Route::Set(unquote(k), unquote(v)),
                None => Route::Ksql,
            }
        }
        Some("UNSET") => Route::Unset(unquote(stmt.trim()[5..].trim())),
        _ => Route::Ksql,
    }
}

impl KsqlSession {
    fn props(&self) -> Value {
        let mut p: Map<String, Value> = self.properties.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        // Show what the stream already holds unless told otherwise.
        p.entry("auto.offset.reset").or_insert_with(|| "earliest".into());
        Value::Object(p)
    }

    /// One statement through `/ksql`: its entities.
    async fn ksql(&mut self, stmt: &str) -> Result<Vec<Value>> {
        let body = json!({ "ksql": format!("{};", stmt.trim().trim_end_matches(';')), "streamsProperties": self.props() });
        let resp = self
            .conn
            .post("/ksql")
            .header("Content-Type", KSQL_JSON)
            .header("Accept", KSQL_JSON)
            .body(body.to_string())
            .send()
            .await
            .map_err(http_error)?;
        let v: Value = check(resp).await?.json().await.map_err(Error::query)?;
        Ok(match v {
            Value::Array(a) => a,
            v => vec![v],
        })
    }

    /// One `DESCRIBE`'s source description.
    async fn describe(&mut self, name: &str) -> Result<Value> {
        let ents = self.ksql(&format!("DESCRIBE {}", quote_ident(Quote::Backtick, name))).await?;
        ents.into_iter()
            .find_map(|e| e.get("sourceDescription").cloned())
            .ok_or_else(|| Error::Query(format!("no se pudo describir {name}")))
    }

    fn begin(&self) {
        self.in_flight.cancelled.store(false, Ordering::SeqCst);
    }

    /// A `SELECT` through `/query-stream`.
    async fn query(&mut self, stmt: &str, push: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.begin();
        let body = json!({ "sql": format!("{};", stmt.trim().trim_end_matches(';')), "properties": self.props() });
        let resp = self
            .conn
            .post("/query-stream")
            .header("Content-Type", "application/json")
            .header("Accept", DELIMITED)
            .body(body.to_string())
            .send()
            .await
            .map_err(http_error)?;
        let mut lines = Lines::new(check(resp).await?);
        let deadline = push.then(|| Instant::now() + self.push_timeout);
        let mut header = true;
        let res = loop {
            let line = match self.next_line(&mut lines, deadline).await {
                Ok(Some(l)) => l,
                Ok(None) => break Ok(()),
                Err(Stop::Timeout) => {
                    out.messages.push(format!("La consulta push se detuvo a los {} s.", self.push_timeout.as_secs()));
                    break Ok(());
                }
                Err(Stop::Cancelled) => break Err(Error::Cancelled),
                Err(Stop::Failed(e)) => break Err(e),
            };
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => break Err(Error::Query(line)),
            };
            if header {
                header = false;
                if let Some(id) = v.get("queryId").and_then(Value::as_str) {
                    *self.in_flight.query_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
                }
                let names = v.get("columnNames").and_then(Value::as_array).cloned().unwrap_or_default();
                let types = v.get("columnTypes").and_then(Value::as_array).cloned().unwrap_or_default();
                out.begin_result(
                    names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| ResultColumn {
                            name: n.as_str().unwrap_or_default().to_string(),
                            type_name: types.get(i).and_then(Value::as_str).unwrap_or_default().to_string(),
                        })
                        .collect(),
                );
                continue;
            }
            match v {
                Value::Array(cells) => {
                    out.push_row(cells.into_iter().map(cell).collect(), max_rows);
                    let r = out.results.last_mut().expect("a result set");
                    if push && r.rows.len() >= max_rows {
                        // More would come: stop here.
                        r.truncated = true;
                        break Ok(());
                    }
                }
                Value::Object(o) if o.contains_key("message") || o.contains_key("errorMessage") => {
                    let msg = o.get("message").or(o.get("errorMessage")).map(text).unwrap_or_default();
                    break Err(Error::Query(msg));
                }
                // Final messages ("Limit Reached", "Query Completed"…).
                Value::Object(o) => {
                    if let Some(m) = o.get("finalMessage").and_then(Value::as_str) {
                        out.messages.push(m.to_string());
                    }
                }
                _ => {}
            }
        };
        drop(lines);
        self.close_query();
        res
    }

    /// `PRINT 'topic'` through `/query`: one text row per record.
    async fn print(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.begin();
        let body = json!({ "ksql": format!("{};", stmt.trim().trim_end_matches(';')), "streamsProperties": self.props() });
        let resp = self
            .conn
            .post("/query")
            .header("Content-Type", KSQL_JSON)
            .body(body.to_string())
            .send()
            .await
            .map_err(http_error)?;
        let mut lines = Lines::new(check(resp).await?);
        out.begin_result(vec![ResultColumn { name: "record".into(), type_name: String::new() }]);
        let deadline = Some(Instant::now() + self.push_timeout);
        loop {
            match self.next_line(&mut lines, deadline).await {
                Ok(Some(l)) if l.trim().is_empty() => continue,
                Ok(Some(l)) => {
                    out.push_row(vec![l.into()], max_rows);
                    let r = out.results.last_mut().expect("a result set");
                    if r.rows.len() >= max_rows {
                        r.truncated = true;
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(Stop::Timeout) => {
                    out.messages.push(format!("PRINT se detuvo a los {} s.", self.push_timeout.as_secs()));
                    return Ok(());
                }
                Err(Stop::Cancelled) => return Err(Error::Cancelled),
                Err(Stop::Failed(e)) => return Err(e),
            }
        }
    }

    /// The next line, unless the deadline passes or the user cancels first.
    async fn next_line(&self, lines: &mut Lines, deadline: Option<Instant>) -> std::result::Result<Option<String>, Stop> {
        let notified = self.in_flight.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.in_flight.cancelled.load(Ordering::SeqCst) {
            return Err(Stop::Cancelled);
        }
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            l = lines.next() => l.map_err(Stop::Failed),
            _ = sleep => Err(Stop::Timeout),
            _ = notified => Err(Stop::Cancelled),
        }
    }

    /// Ask the server to end the transient query (dropping the connection
    /// also ends it; this makes it prompt).
    fn close_query(&self) {
        let Some(id) = self.in_flight.query_id.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        let conn = self.conn.clone();
        self.rt.spawn(async move {
            let _ = conn.post("/close-query").header("Content-Type", "application/json").body(json!({ "queryId": id }).to_string()).send().await;
        });
    }

    async fn names(&mut self, stmt: &str, field: &str) -> Result<Vec<String>> {
        let ents = self.ksql(stmt).await?;
        Ok(ents
            .iter()
            .filter_map(|e| e.get(field).and_then(Value::as_array))
            .flatten()
            .filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_string))
            .collect())
    }
}

enum Stop {
    Timeout,
    Cancelled,
    Failed(Error),
}

/// Lines of a streamed body.
struct Lines {
    resp: reqwest::Response,
    buf: Vec<u8>,
    eof: bool,
}

impl Lines {
    fn new(resp: reqwest::Response) -> Self {
        Self { resp, buf: Vec::new(), eof: false }
    }

    async fn next(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                return Ok(Some(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_string()));
            }
            if self.eof {
                return Ok((!self.buf.is_empty()).then(|| String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned()));
            }
            match self.resp.chunk().await.map_err(http_error)? {
                Some(c) => self.buf.extend_from_slice(&c),
                None => self.eof = true,
            }
        }
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn cell(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => json_i64(i),
            None => n.as_f64().map_or(Value::Number(n), json_f64),
        },
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v,
    }
}

/// `{"type": "ARRAY", "memberSchema": {"type": "STRING"}}` → `ARRAY<STRING>`.
fn schema_type(s: &Value) -> String {
    let ty = s.get("type").and_then(Value::as_str).unwrap_or_default();
    match ty {
        "ARRAY" => format!("ARRAY<{}>", s.get("memberSchema").map(schema_type).unwrap_or_default()),
        "MAP" => format!("MAP<STRING, {}>", s.get("memberSchema").map(schema_type).unwrap_or_default()),
        "STRUCT" => {
            let fields = s.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
            let inner: Vec<String> = fields
                .iter()
                .map(|f| format!("{} {}", text(&f["name"]), f.get("schema").map(schema_type).unwrap_or_default()))
                .collect();
            format!("STRUCT<{}>", inner.join(", "))
        }
        "DECIMAL" => {
            let p = s.pointer("/parameters/precision").map(text).unwrap_or_default();
            let sc = s.pointer("/parameters/scale").map(text).unwrap_or_default();
            format!("DECIMAL({p}, {sc})")
        }
        t => t.to_string(),
    }
}

/// A `/ksql` entity as rows: the first list of objects it carries (streams,
/// topics, fields…), or its scalar fields as one row.
fn entity_table(e: &Value, max_rows: usize, out: &mut QueryOutcome) {
    let Some(obj) = e.as_object() else { return };
    let list = obj
        .iter()
        .filter(|(k, _)| k.as_str() != "warnings")
        .find_map(|(_, v)| v.as_array().filter(|a| a.iter().all(Value::is_object) && !a.is_empty()))
        .or_else(|| obj.get("sourceDescription").and_then(|d| d.get("fields")).and_then(Value::as_array));
    let rows: Vec<Map<String, Value>> = match list {
        Some(a) => a.iter().filter_map(|x| x.as_object().cloned()).collect(),
        None => {
            let m: Map<String, Value> = obj
                .iter()
                .filter(|(k, v)| !k.starts_with('@') && k.as_str() != "statementText" && !v.is_array())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            vec![m]
        }
    };
    let mut cols: Vec<String> = Vec::new();
    for r in &rows {
        for k in r.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    // serde_json maps are sorted: at least put the name first.
    if let Some(i) = cols.iter().position(|c| c == "name") {
        let n = cols.remove(i);
        cols.insert(0, n);
    }
    out.begin_result(cols.iter().map(|c| ResultColumn { name: c.clone(), type_name: String::new() }).collect());
    for r in rows {
        let row = cols
            .iter()
            .map(|c| match r.get(c) {
                // DESCRIBE's field schemas read better as type names.
                Some(s @ Value::Object(o)) if c == "schema" && o.contains_key("type") => Value::String(schema_type(s)),
                Some(v) => cell(v.clone()),
                None => Value::Null,
            })
            .collect();
        out.push_row(row, max_rows);
    }
}

fn apply_entities(ents: Vec<Value>, stmt: &str, max_rows: usize, out: &mut QueryOutcome) {
    if ents.is_empty() {
        // INSERT … VALUES answers with nothing.
        let one = words(stmt).first().map(String::as_str) == Some("INSERT");
        out.push_affected(u64::from(one));
    }
    for e in ents {
        for w in e.get("warnings").and_then(Value::as_array).into_iter().flatten() {
            out.messages.push(text(w.get("message").unwrap_or(w)));
        }
        match e.get("@type").and_then(Value::as_str) {
            Some("currentStatus") => {
                if let Some(m) = e.pointer("/commandStatus/message").and_then(Value::as_str) {
                    out.messages.push(m.to_string());
                }
                out.push_affected(0);
            }
            _ => entity_table(&e, max_rows, out),
        }
    }
}

#[async_trait]
impl Session for KsqlSession {
    async fn server_version(&mut self) -> Result<String> {
        let rb = self.conn.auth(self.conn.http.get(format!("{}/info", self.conn.base)));
        let resp = check(rb.send().await.map_err(http_error)?).await?;
        let v: Value = resp.json().await.map_err(Error::query)?;
        Ok(format!("ksqlDB {}", v.pointer("/KsqlServerInfo/version").map(text).unwrap_or_default()))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut out = Vec::new();
        for (stmt, field, kind) in [("SHOW STREAMS", "streams", kinds::STREAM), ("SHOW TABLES", "tables", kinds::TABLE), ("SHOW TOPICS", "topics", kinds::TOPIC)] {
            let mut names = self.names(stmt, field).await?;
            names.retain(|n| n != PROCESSING_LOG && !n.starts_with('_') && !n.ends_with("ksql_processing_log"));
            names.sort();
            out.extend(names.into_iter().map(|name| DbObject { kind: kind.into(), schema: None, name, parent: None }));
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if obj.kind == kinds::TOPIC {
            return Ok(Vec::new());
        }
        let d = self.describe(&obj.name).await?;
        Ok(d.get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|f| ColumnInfo {
                name: text(&f["name"]),
                data_type: f.get("schema").map(schema_type).unwrap_or_default(),
                nullable: true,
                primary_key: matches!(f.get("type").and_then(Value::as_str), Some("KEY" | "PRIMARY_KEY")),
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if obj.kind == kinds::TOPIC {
            return Ok(None);
        }
        let d = self.describe(&obj.name).await?;
        Ok(d.get("statement").and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty()))
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    /// A pull query read to the end, typed by its column types.
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, source, progress).await
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let ents = self.ksql("SHOW STREAMS EXTENDED; SHOW TABLES EXTENDED").await?;
        let mut out = ddl::from_descriptions(
            ents.iter().filter_map(|e| e.get("sourceDescriptions").and_then(Value::as_array)).flatten(),
        );
        out.retain(|t| t.name != PROCESSING_LOG && !t.name.starts_with('_'));
        Ok(out)
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        if obj.kind == kinds::TOPIC {
            return format!("PRINT '{}' FROM BEGINNING LIMIT {limit};", obj.name.replace('\'', "''"));
        }
        format!("SELECT * FROM {} EMIT CHANGES LIMIT {limit};", quote_ident(Quote::Backtick, &obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for stmt in split_statements(text) {
            match route(&stmt) {
                Route::Query { push } => self.query(&stmt, push, max_rows, out).await?,
                Route::Print => self.print(&stmt, max_rows, out).await?,
                Route::Set(k, v) => {
                    out.messages.push(format!("Propiedad '{k}' = '{v}'"));
                    self.properties.insert(k, v);
                    out.push_affected(0);
                }
                Route::Unset(k) => {
                    self.properties.remove(&k);
                    out.push_affected(0);
                }
                Route::Ksql => {
                    let ents = self.ksql(&stmt).await?;
                    apply_entities(ents, &stmt, max_rows, out);
                }
            }
        }
        Ok(())
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mut noted = false;
        for stmt in split_statements(text) {
            if explainable(&stmt) {
                let ents = self.ksql(&format!("EXPLAIN {}", stmt.trim().trim_end_matches(';'))).await?;
                match ents.iter().find_map(|e| e.get("queryDescription")) {
                    Some(d) => out.plans.push(plan::from_description(&stmt, d)),
                    None => out.messages.push(format!("ksqlDB no devolvió un plan para «{}».", short(&stmt))),
                }
                if analyze && !noted {
                    out.messages.push(
                        "ksqlDB no da cifras reales por operador: se muestra el plan estimado junto al resultado.".into(),
                    );
                    noted = true;
                }
            } else if !analyze && !matches!(route(&stmt), Route::Set(..) | Route::Unset(_)) {
                out.messages.push(format!("Sin plan para «{}»: ksqlDB solo explica consultas.", short(&stmt)));
            }
            // SET / UNSET shape the plans after them; the rest only runs
            // when asked to.
            if analyze || matches!(route(&stmt), Route::Set(..) | Route::Unset(_)) {
                self.execute(&stmt, max_rows, out).await?;
            }
        }
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let in_flight = self.in_flight.clone();
        Some(Arc::new(move || {
            in_flight.cancelled.store(true, Ordering::SeqCst);
            in_flight.notify.notify_waiters();
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_are_unsupported() {
        let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() };
        let d = &drivers()[0];
        assert!(matches!(d.delete_script(&t, &[vec![("ID".into(), Value::from(1))]]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn what_explain_takes() {
        assert!(explainable("select * from s emit changes"));
        assert!(explainable("CREATE STREAM s2 AS SELECT * FROM s"));
        assert!(explainable("INSERT INTO s2 SELECT * FROM s"));
        assert!(!explainable("CREATE STREAM s (id INT) WITH (kafka_topic='t', value_format='JSON')"));
        assert!(!explainable("SHOW STREAMS"));
    }

    #[test]
    fn statements_are_routed() {
        assert_eq!(route("select * from s emit changes limit 3"), Route::Query { push: true });
        assert_eq!(route("SELECT * FROM t WHERE id = 1"), Route::Query { push: false });
        assert_eq!(route("PRINT 'x' FROM BEGINNING"), Route::Print);
        assert_eq!(route("SET 'auto.offset.reset' = 'latest'"), Route::Set("auto.offset.reset".into(), "latest".into()));
        assert_eq!(route("UNSET 'auto.offset.reset'"), Route::Unset("auto.offset.reset".into()));
        assert_eq!(route("SHOW STREAMS"), Route::Ksql);
    }

    #[test]
    fn entities_become_tables() {
        let ents: Vec<Value> = serde_json::from_str(
            r#"[{"@type":"streams","statementText":"SHOW STREAMS;","streams":[
                  {"type":"STREAM","name":"S1","topic":"s1","isWindowed":false}],"warnings":[]},
                {"@type":"currentStatus","commandStatus":{"status":"SUCCESS","message":"Stream created"},"warnings":[{"message":"w"}]}]"#,
        )
        .unwrap();
        let mut out = QueryOutcome::default();
        apply_entities(ents, "SHOW STREAMS", 10, &mut out);
        // "name" first; the rest keep the server's order only when serde_json's
        // preserve_order is on (it is in the app, through bson), so compare as a set.
        let cols: Vec<&str> = out.results[0].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols[0], "name");
        let mut rest = cols[1..].to_vec();
        rest.sort_unstable();
        assert_eq!(rest, ["isWindowed", "topic", "type"]);
        assert_eq!(out.results[0].rows[0][0], json!("S1"));
        assert_eq!(out.results[1].rows_affected, Some(0));
        assert_eq!(out.messages, ["w", "Stream created"]);
    }

    #[test]
    fn schema_types_read_like_ddl() {
        let s = json!({"type":"STRUCT","fields":[{"name":"A","schema":{"type":"ARRAY","memberSchema":{"type":"INTEGER"}}}]});
        assert_eq!(schema_type(&s), "STRUCT<A ARRAY<INTEGER>>");
        assert_eq!(cell(json!([1, "x"])), json!("[1,\"x\"]"));
    }
}

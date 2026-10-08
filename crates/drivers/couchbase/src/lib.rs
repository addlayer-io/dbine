//! Couchbase Server through its REST services: SQL++ (N1QL) statements go
//! to the Query service (`POST /query/service`, port 8093) and buckets,
//! scopes, collections and cluster figures come from the cluster manager
//! (port 8091). Both take basic auth.
//!
//! A session is one bucket; its objects are the collections of every scope
//! (schema `bucket.scope`, so statements always use the full keyspace
//! path) and their GSI indexes. Each request carries a
//! `client_context_id`: cancel drops the request and deletes it from
//! `system:active_requests`. Read-only sessions also send `readonly`, so
//! the server refuses writes.

mod create_db;
mod ddl;
mod index_usage;
mod sync;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod properties;
mod security;
mod stats;
mod steps;
mod transfer;

use ddl::{path, q, split_schema};
use dbine_driver::sql::{split_script, split_statements, ScriptDefaults, ScriptDialect, ScriptMode, StatementKind};
use dbine_driver::{
    async_trait, json_i64, json_u64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, Message, MessageLevel, Metric, MetricUnit,
    MonitorSnapshot, MonitorTable, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, ScriptError, Session,
    TableSchema, TxState,
};
use steps::Step;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Documents sampled to infer a collection's fields.
const SAMPLE: usize = 100;

/// How long a transaction opened from the editor may stay open (the
/// server's default, 15 s, is too short for someone typing).
const TX_TIMEOUT: &str = "1h";

/// cbq's reading of a script: SQL++ strings take backslash escapes, and
/// there are no `BEGIN … END` bodies (`BEGIN WORK` is a statement).
fn dialect() -> ScriptDialect {
    ScriptDialect { backslash_escapes: true, compound_blocks: false, ..ScriptDialect::generic() }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(CouchbaseDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "couchbase",
        name: "Couchbase",
        family: Family::Document,
        language: Language::Sql,
        dialect: "n1ql",
        default_port: 8093,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8093").help("Servicio de consultas: 8093; con TLS, 18093."),
            Field::new("mgmt_port", "Puerto de administración", FieldKind::Number)
                .placeholder("8091")
                .help("Cluster manager (buckets, scopes, métricas): 8091; con TLS, 18091.")
                .advanced(),
            Field { label: "Bucket", placeholder: "(ninguno)", ..Field::database() },
            Field::username().required(),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Buckets",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::new(kinds::COLLECTION, "Colecciones", true, true, true),
            ObjectKindInfo::new(kinds::INDEX, "Índices", false, false, true),
            ObjectKindInfo::new(kinds::FUNCTION, "Funciones", false, false, true),
        ],
    }
}

pub struct CouchbaseDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for CouchbaseDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        "SQL++ (N1QL). Las colecciones se nombran `bucket`.`scope`.`coleccion`; sin prefijo se buscan en el scope _default del bucket de la sesión. META(d).id es la clave del documento."
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// `system:indexes` + the index service's statistics (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One statement per Query service request; the session's state (the
    /// open transaction's txid) lives in the session.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// cbq goes on after a failed statement (unless `-exit-on-error`).
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    /// SQL++ transactions (`BEGIN WORK` … `COMMIT`): the txid the server
    /// hands out goes with every statement until the end.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// Multi-row SQL++ `INSERT`s, several at once (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields().into_iter().map(create_db::grouped).collect()
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(name, options)
    }

    /// "Propiedades" of a bucket (see [`properties`]).
    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(database, changes)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: false,
            monitor: true,
            processes: true,
            cancel_query: true,
            database_properties: true,
            ..Default::default()
        }
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
        ddl::insert_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Scopes are the schemas (`bucket.scope`). They have no owner; roles
    /// that take a scope (Enterprise Edition) can be granted on a new one.
    /// `DROP SCOPE` always takes its collections with it.
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(dbine_driver::SchemaSpec { owner: false, owner_kinds: dbine_driver::SchemaOwnerKinds::Both, cascade: true, privileges: security::scope_roles(), grant_option: false })
    }

    /// `name` is `bucket.scope`, or a bare scope of the menu's bucket.
    fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        ddl::create_scope(&ddl::full_scope(database, name), owner)
    }

    fn schema_grant_script(&self, database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
        let object = ObjectRef { kind: "schema".into(), schema: None, name: ddl::full_scope(database, name) };
        security::script(&dbine_driver::SecurityAction::Grant { privileges: privileges.to_vec(), object: Some(object), to: to.to_string(), grantable })
    }

    fn drop_schema_script(&self, database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        ddl::drop_scope(&ddl::full_scope(database, name), cascade)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let mgmt_port: u16 = cfg.option("mgmt_port").and_then(|p| p.trim().parse().ok()).unwrap_or(if cfg.encrypt { 18091 } else { 8091 });
        let bucket = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let s = CbSession {
            conn: Arc::new(Conn {
                http,
                query: format!("{scheme}://{host}:{}", cfg.port_or(if cfg.encrypt { 18093 } else { 8093 })),
                mgmt: format!("{scheme}://{host}:{mgmt_port}"),
                user: cfg.username.clone().unwrap_or_default(),
                password: cfg.password.clone().unwrap_or_default(),
                read_only: cfg.read_only,
            }),
            bucket: bucket.map(str::to_string),
            cancel: Arc::new(Cancel::default()),
            rt: tokio::runtime::Handle::current(),
            profiler: None,
            txid: None,
            manual: false,
        };
        let check = async {
            s.query("SELECT RAW 1", None).await?;
            if let Some(b) = &s.bucket {
                s.conn.mgmt_get(&format!("/pools/default/buckets/{}", encode(b))).await?;
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(Duration::from_secs(20), check)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    http: reqwest::Client,
    query: String,
    mgmt: String,
    user: String,
    password: String,
    read_only: bool,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Query service error codes: 10000 authentication, 12008/13014 (and
/// 2120 on authorization) credentials; 1010 / 1260 request stopped. Others
/// keep their code and, when the server gives one, their place in
/// `statement` (`line` / `column`); further errors of the same request
/// follow the first one's message.
fn query_error(errors: &[Value], statement: Option<&str>) -> Error {
    let first = errors.first().cloned().unwrap_or(Value::Null);
    let code = first.get("code").and_then(Value::as_i64).unwrap_or(0);
    let msg = first.get("msg").and_then(Value::as_str).unwrap_or("error de Couchbase").to_string();
    match code {
        10000 | 13014 => Error::AuthFailed(msg),
        2120 if msg.contains("authenticate") => Error::AuthFailed(msg),
        1010 | 1260 | 5010 if msg.to_ascii_lowercase().contains("stop") || msg.contains("cancel") => Error::Cancelled,
        _ => {
            let mut text = msg;
            // Transaction errors carry the data service's reason inside.
            let mut cause = first.get("cause");
            while let Some(c) = cause {
                if let Some(d) = c.get("error_description").and_then(Value::as_str) {
                    text.push_str(&format!(" ({d})"));
                    break;
                }
                cause = c.get("cause");
            }
            for e in &errors[1..] {
                text.push('\n');
                text.push_str(e.get("msg").and_then(Value::as_str).unwrap_or_default());
            }
            let mut se = ScriptError::new(text);
            if code != 0 {
                se = se.with_code(code.to_string());
            }
            let at = |k: &str| first.get(k).and_then(Value::as_u64).map(|n| n as u32);
            if let (Some(line), Some(stmt)) = (at("line"), statement) {
                se = se.at_line(line).at_offset(steps::offset_of(stmt, line, at("column").unwrap_or(1)));
            }
            Error::Statement(Box::new(se))
        }
    }
}

/// The transaction no longer exists on the server (it expired, or ended).
fn tx_gone(e: &Error) -> bool {
    match e {
        Error::Statement(se) => {
            se.code.as_deref() == Some("17004") || se.message.contains("is not present") || se.message.to_ascii_lowercase().contains("expired")
        }
        _ => false,
    }
}

/// What a statement does to the transaction.
#[derive(Debug, PartialEq, Eq)]
enum TxEffect {
    /// `BEGIN WORK`, `START TRANSACTION`.
    Begin,
    /// `COMMIT [WORK|TRANSACTION]`.
    Commit,
    /// `ROLLBACK [WORK|TRANSACTION]` (not `ROLLBACK TO SAVEPOINT`).
    Rollback,
    /// A statement a manual transaction takes (reads and DML; the server
    /// refuses DDL inside a transaction).
    Data,
    Other,
}

/// The txid of a `BEGIN WORK` reply.
fn txid_of(v: &Value) -> Option<String> {
    v.pointer("/results/0/txid").and_then(Value::as_str).map(str::to_string)
}

fn tx_effect(stmt: &str) -> TxEffect {
    let words: Vec<String> = stmt.split_whitespace().take(3).map(|w| w.trim_end_matches(';').to_ascii_uppercase()).collect();
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    match w(0) {
        "BEGIN" if matches!(w(1), "" | "WORK" | "TRANSACTION" | "TRAN") => TxEffect::Begin,
        "START" if matches!(w(1), "WORK" | "TRANSACTION" | "TRAN") => TxEffect::Begin,
        "COMMIT" => TxEffect::Commit,
        "ROLLBACK" if !(w(1) == "TO" || w(2) == "TO") => TxEffect::Rollback,
        "SELECT" | "WITH" | "INSERT" | "UPSERT" | "UPDATE" | "DELETE" | "MERGE" | "EXECUTE" | "INFER" => TxEffect::Data,
        _ => TxEffect::Other,
    }
}

impl Conn {
    async fn post_query(&self, body: &Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}/query/service", self.query))
            .basic_auth(&self.user, Some(&self.password))
            .json(body)
            .send()
            .await
            .map_err(http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(http_error)?;
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                Error::AuthFailed(text.trim().to_string())
            } else {
                Error::Query(format!("HTTP {status}: {}", text.trim()))
            }
        })?;
        if let Some(errs) = v.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            return Err(query_error(errs, body.get("statement").and_then(Value::as_str)));
        }
        if v.get("status").and_then(Value::as_str) == Some("stopped") {
            return Err(Error::Cancelled);
        }
        Ok(v)
    }

    async fn mgmt_send(&self, rb: reqwest::RequestBuilder) -> Result<String> {
        let resp = rb.basic_auth(&self.user, Some(&self.password)).send().await.map_err(http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(http_error)?;
        match status.as_u16() {
            200..=299 => Ok(text),
            401 => Err(Error::AuthFailed("Couchbase rechazó el usuario o la contraseña.".into())),
            403 => Err(Error::Query(format!("Sin permiso en el cluster manager: {}", text.trim()))),
            _ => Err(Error::Query(format!("HTTP {status}: {}", text.trim()))),
        }
    }

    async fn mgmt_get(&self, path: &str) -> Result<Value> {
        let text = self.mgmt_send(self.http.get(format!("{}{path}", self.mgmt))).await?;
        serde_json::from_str(&text).map_err(Error::query)
    }
}

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
    current: Mutex<Option<String>>,
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

pub struct CbSession {
    conn: Arc<Conn>,
    bucket: Option<String>,
    cancel: Arc<Cancel>,
    rt: tokio::runtime::Handle,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// The open transaction (`BEGIN WORK`): sent with every editor
    /// statement until `COMMIT` / `ROLLBACK`.
    txid: Option<String>,
    /// Manual transactions: the first read or DML opens one.
    manual: bool,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn context_id() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("dbine-{t:x}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A value as a cell: numbers checked for JS precision, nested as JSON text.
fn cell(v: &Value) -> Value {
    match v {
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => v.clone(),
        },
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v.clone(),
    }
}

/// Results as a table: objects become one row each with the union of
/// their keys as columns (in order of appearance); other values (`SELECT
/// RAW`) go in one `value` column.
fn tabulate(results: &[Value], signature: Option<&Value>) -> (Vec<String>, Vec<Vec<Value>>) {
    let objects = !results.is_empty() && results.iter().all(Value::is_object);
    if !objects {
        if results.is_empty() {
            let cols: Vec<String> = signature
                .and_then(Value::as_object)
                .map(|o| o.keys().filter(|k| *k != "*").cloned().collect())
                .unwrap_or_default();
            if !cols.is_empty() {
                return (cols, Vec::new());
            }
        }
        return (vec!["value".into()], results.iter().map(|v| vec![cell(v)]).collect());
    }
    // Named projections (no `*`) keep their order and their MISSING fields.
    let mut cols: Vec<String> = signature
        .and_then(Value::as_object)
        .filter(|o| !o.contains_key("*"))
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for r in results {
        for k in r.as_object().into_iter().flat_map(|o| o.keys()) {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let rows = results.iter().map(|r| cols.iter().map(|c| r.get(c).map(cell).unwrap_or(Value::Null)).collect()).collect();
    (cols, rows)
}

fn first_word(stmt: &str) -> String {
    stmt.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

/// Statements `EXPLAIN` takes (it never runs them).
fn explainable(stmt: &str) -> bool {
    matches!(first_word(stmt).as_str(), "SELECT" | "WITH" | "INSERT" | "UPSERT" | "UPDATE" | "DELETE" | "MERGE")
}

/// The keyspace of a `CREATE COLLECTION` statement.
fn created_collection(stmt: &str) -> Option<String> {
    let words: Vec<&str> = stmt.split_whitespace().collect();
    if words.len() < 3 || !words[0].eq_ignore_ascii_case("CREATE") || !words[1].eq_ignore_ascii_case("COLLECTION") {
        return None;
    }
    let ks = if words.len() >= 6 && words[2].eq_ignore_ascii_case("IF") { words[5] } else { words[2] };
    Some(ks.trim_end_matches(';').to_string()).filter(|k| !k.is_empty() && !k.eq_ignore_ascii_case("IF"))
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

impl CbSession {
    fn context(&self) -> Option<String> {
        self.bucket.as_ref().map(|b| format!("default:{}.`_default`", q(b)))
    }

    fn body(&self, stmt: &str, id: Option<&str>) -> Value {
        let mut b = json!({"statement": stmt, "scan_consistency": "request_plus"});
        if let Some(c) = self.context() {
            b["query_context"] = Value::String(c);
        }
        if let Some(id) = id {
            b["client_context_id"] = Value::String(id.to_string());
        }
        if self.conn.read_only {
            b["readonly"] = Value::Bool(true);
        }
        b
    }

    async fn query(&self, stmt: &str, id: Option<&str>) -> Result<Value> {
        self.cancel.run(self.conn.post_query(&self.body(stmt, id))).await
    }

    async fn results(&self, stmt: &str) -> Result<Vec<Value>> {
        let v = self.query(stmt, None).await?;
        Ok(v.get("results").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Run one statement (with `extra` request fields) into `out`.
    async fn run(&mut self, stmt: &str, extra: Option<Value>, max_rows: usize, out: &mut QueryOutcome) -> Result<Value> {
        let id = context_id();
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        let mut body = self.body(stmt, Some(&id));
        if let Some(t) = &self.txid {
            body["txid"] = Value::String(t.clone());
        }
        if let Some(Value::Object(e)) = extra {
            for (k, v) in e {
                body[k] = v;
            }
        }
        let r = self.cancel.run(self.conn.post_query(&body)).await;
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let v = r?;
        let results = v.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
        let signature = v.get("signature").filter(|s| !s.is_null());
        let mutations = v.pointer("/metrics/mutationCount").and_then(Value::as_u64);
        if results.is_empty() && matches!(tx_effect(stmt), TxEffect::Commit | TxEffect::Rollback) {
            // Its own message says what happened: no grid, no count.
            out.results.push(dbine_driver::StatementResult::default());
        } else if results.is_empty() && (signature.is_none() || mutations.is_some()) {
            out.push_affected(mutations.unwrap_or(0));
        } else {
            let (cols, rows) = tabulate(&results, signature);
            out.begin_result(cols.into_iter().map(|name| ResultColumn { name, type_name: String::new() }).collect());
            for r in rows {
                out.push_row(r, max_rows);
            }
            if let Some(m) = mutations {
                out.info(format!("{m} documentos modificados"));
            }
        }
        for w in v.get("warnings").and_then(Value::as_array).into_iter().flatten() {
            out.message(Message {
                level: MessageLevel::Warning,
                text: w.get("msg").map(text).unwrap_or_else(|| w.to_string()),
                code: w.get("code").filter(|c| !c.is_null()).map(text),
                ..Default::default()
            });
        }
        Ok(v)
    }

    /// An editor statement: in the open transaction, opening one first in
    /// manual mode, and keeping the txid `BEGIN WORK` returns.
    async fn run_stmt(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let effect = tx_effect(stmt);
        if self.manual && self.txid.is_none() && effect == TxEffect::Data {
            self.begin(out).await?;
        }
        if effect == TxEffect::Begin && self.txid.is_none() {
            let v = self.run(stmt, Some(json!({"txtimeout": TX_TIMEOUT})), max_rows, out).await?;
            self.txid = txid_of(&v);
            out.info("Transacción iniciada.");
            return Ok(());
        }
        match self.run(stmt, None, max_rows, out).await {
            Ok(_) => {
                match effect {
                    TxEffect::Commit if self.txid.take().is_some() => out.info("Transacción confirmada."),
                    TxEffect::Rollback if self.txid.take().is_some() => out.info("Transacción deshecha."),
                    _ => {}
                }
                Ok(())
            }
            Err(e) => {
                // A failed COMMIT ends the transaction too (the server
                // rolls it back).
                if self.txid.is_some() && (tx_gone(&e) || matches!(effect, TxEffect::Commit | TxEffect::Rollback)) {
                    self.txid = None;
                    out.warning("La transacción ya no está abierta en el servidor: sus cambios no se guardaron.");
                }
                Err(e)
            }
        }
    }

    /// `BEGIN WORK` for manual mode.
    async fn begin(&mut self, out: &mut QueryOutcome) -> Result<()> {
        let mut b = self.body("BEGIN WORK", None);
        b["txtimeout"] = Value::String(TX_TIMEOUT.into());
        let v = self.cancel.run(self.conn.post_query(&b)).await?;
        self.txid = txid_of(&v);
        out.info("Transacción iniciada.");
        Ok(())
    }

    /// `COMMIT` / `ROLLBACK` of the open transaction, if any.
    async fn end_tx(&mut self, stmt: &str) -> Result<()> {
        let Some(t) = self.txid.take() else { return Ok(()) };
        let mut b = self.body(stmt, None);
        b["txid"] = Value::String(t);
        match self.cancel.run(self.conn.post_query(&b)).await {
            Err(e) if tx_gone(&e) => Err(Error::Query("La transacción ya no estaba abierta en el servidor (se venció): sus cambios no se guardaron.".into())),
            r => r.map(|_| ()),
        }
    }

    /// A new collection takes a moment to reach the query service; the
    /// next statement of the script (an index, an insert) needs it.
    async fn wait_for(&self, keyspace: &str) {
        for _ in 0..40 {
            if self.query(&format!("SELECT RAW 1 FROM {keyspace} LIMIT 1"), None).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn bucket(&self) -> Result<String> {
        self.bucket.clone().ok_or_else(|| Error::Query("Elegí un bucket para ver sus colecciones.".into()))
    }

    async fn explain_plan(&self, stmt: &str) -> Result<dbine_driver::Plan> {
        let r = self.results(&format!("EXPLAIN {stmt}")).await?;
        let p = r.first().and_then(|x| x.get("plan")).cloned().unwrap_or(Value::Null);
        Ok(plan::from_json(stmt, &p, false))
    }

    /// Indexes of the session's bucket: (scope, collection, name, is_primary, definition).
    async fn indexes(&self) -> Result<Vec<(String, String, String, bool, String)>> {
        let b = self.bucket()?;
        let rows = self
            .results(&format!(
                "SELECT i.name, i.scope_id, i.keyspace_id, i.bucket_id, i.is_primary, i.metadata.definition AS def
                 FROM system:indexes AS i
                 WHERE (i.bucket_id = {0} OR (i.bucket_id IS MISSING AND i.keyspace_id = {0})) AND i.`using` = 'gsi'
                 ORDER BY i.keyspace_id, i.name",
                serde_json::to_string(&b).unwrap_or_default()
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let has_bucket = r.get("bucket_id").is_some();
                let scope = if has_bucket { r.get("scope_id").map(text).unwrap_or_default() } else { "_default".into() };
                let coll = if has_bucket { r.get("keyspace_id").map(text).unwrap_or_default() } else { "_default".into() };
                (scope, coll, r.get("name").map(text).unwrap_or_default(), r.get("is_primary").and_then(Value::as_bool).unwrap_or(false), r.get("def").map(text).unwrap_or_default())
            })
            .collect())
    }
}

#[async_trait]
impl Session for CbSession {
    async fn server_version(&mut self) -> Result<String> {
        let v = self.conn.mgmt_get("/pools").await.ok();
        let impl_version = v.as_ref().and_then(|v| v.get("implementationVersion")).map(text);
        match impl_version {
            Some(iv) => Ok(format!("Couchbase Server {iv}")),
            None => {
                let r = self.results("SELECT RAW version()").await?;
                Ok(format!("Couchbase Query {}", r.first().map(text).unwrap_or_default()))
            }
        }
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self.conn.mgmt_get("/pools/default/buckets?skipMap=true").await {
            Ok(v) => Ok(v.as_array().into_iter().flatten().filter_map(|b| b.get("name").map(text)).collect()),
            Err(_) => Ok(self.results("SELECT RAW name FROM system:buckets ORDER BY name").await?.iter().map(text).collect()),
        }
    }

    async fn row_estimates(&mut self) -> Result<Vec<dbine_driver::stats::RowEstimate>> {
        stats::row_estimates(self).await
    }

    /// Couchbase keeps no comments.
    async fn object_comments(&mut self) -> Result<Vec<dbine_driver::stats::ObjectComment>> {
        Ok(Vec::new())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let b = self.bucket()?;
        let mut out = Vec::new();
        let scopes = match self.conn.mgmt_get(&format!("/pools/default/buckets/{}/scopes", encode(&b))).await {
            Ok(v) => v
                .get("scopes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|s| {
                    let scope = s.get("name").map(text).unwrap_or_default();
                    s.get("collections").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(move |c| (scope.clone(), c.get("name").map(text).unwrap_or_default()))
                })
                .collect::<Vec<_>>(),
            Err(_) => self
                .results(&format!("SELECT k.`scope`, k.name FROM system:keyspaces AS k WHERE k.`bucket` = {} ORDER BY k.`scope`, k.name", serde_json::to_string(&b).unwrap_or_default()))
                .await?
                .into_iter()
                .map(|r| (r.get("scope").map(text).unwrap_or_default(), r.get("name").map(text).unwrap_or_default()))
                .collect(),
        };
        for (scope, coll) in scopes.into_iter().filter(|(s, _)| !s.starts_with("_system")) {
            out.push(DbObject { kind: kinds::COLLECTION.into(), schema: Some(format!("{b}.{scope}")), name: coll, parent: None });
        }
        if let Ok(ix) = self.indexes().await {
            for (scope, coll, name, _, _) in ix.into_iter().filter(|(s, ..)| !s.starts_with("_system")) {
                out.push(DbObject { kind: kinds::INDEX.into(), schema: Some(format!("{b}.{scope}")), name, parent: Some(coll) });
            }
        }
        if let Ok(fns) = self
            .results(&format!(
                "SELECT RAW f.identity.name FROM system:functions AS f WHERE f.identity.`bucket` = {} OR f.identity.type = 'global'",
                serde_json::to_string(&b).unwrap_or_default()
            ))
            .await
        {
            for f in fns {
                out.push(DbObject { kind: kinds::FUNCTION.into(), schema: None, name: text(&f), parent: None });
            }
        }
        Ok(out)
    }

    /// The bucket's scopes (`bucket.scope`, as the objects name them), so an
    /// empty scope (just made with "Nuevo esquema…") shows and can be
    /// dropped. `_system` (Couchbase's own, 7.6+) is marked system: its
    /// collections aren't listed, so the tree never shows it.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let Ok(b) = self.bucket() else { return Ok(None) };
        let names: Vec<String> = match self.conn.mgmt_get(&format!("/pools/default/buckets/{}/scopes", encode(&b))).await {
            Ok(v) => v.get("scopes").and_then(Value::as_array).into_iter().flatten().map(|s| s.get("name").map(text).unwrap_or_default()).collect(),
            Err(_) => self
                .results(&format!("SELECT RAW s.name FROM system:scopes AS s WHERE s.`bucket` = {} ORDER BY s.name", serde_json::to_string(&b).unwrap_or_default()))
                .await?
                .iter()
                .map(text)
                .collect(),
        };
        Ok(Some(
            names
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(|s| dbine_driver::SchemaInfo { system: s.starts_with("_system"), name: format!("{b}.{s}") })
                .collect(),
        ))
    }

    /// Fields of a sample of documents: `_id` (the key) first, then the
    /// union of top-level fields with their observed types.
    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if obj.kind != kinds::COLLECTION {
            return Ok(Vec::new());
        }
        let docs = self.results(&format!("SELECT RAW d FROM {} AS d LIMIT {SAMPLE}", path(obj.schema(), &obj.name))).await?;
        let mut cols: Vec<(String, Vec<&'static str>, usize)> = Vec::new();
        for d in docs.iter().filter_map(Value::as_object) {
            for (k, v) in d {
                match cols.iter_mut().find(|(n, ..)| n == k) {
                    Some((_, types, seen)) => {
                        *seen += 1;
                        if !types.contains(&json_type(v)) {
                            types.push(json_type(v));
                        }
                    }
                    None => cols.push((k.clone(), vec![json_type(v)], 1)),
                }
            }
        }
        let n = docs.len();
        let mut out = vec![ColumnInfo { name: "_id".into(), data_type: "string".into(), nullable: false, primary_key: true, auto_increment: false, default_value: None }];
        out.extend(cols.into_iter().map(|(name, types, seen)| ColumnInfo {
            name,
            data_type: types.join(" | "),
            nullable: seen < n || types.contains(&"null"),
            primary_key: false,
            auto_increment: false,
            default_value: None,
        }));
        Ok(out)
    }

    /// The bucket's collections with their sampled fields (`_id` as key)
    /// and their GSI indexes, primary ones included ([`ddl::index_from_row`]).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let b = self.bucket()?;
        let mut out = Vec::new();
        for o in self.list_objects().await?.into_iter().filter(|o| o.kind == kinds::COLLECTION) {
            let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            let cols = self.columns(&obj).await.unwrap_or_default();
            out.push(TableSchema {
                kind: o.kind,
                schema: o.schema,
                name: o.name,
                primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["_id".into()] }),
                columns: cols
                    .into_iter()
                    .map(|c| dbine_driver::ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
                    .collect(),
                ..Default::default()
            });
        }
        let rows = self
            .results(&format!(
                "SELECT i.name, i.scope_id, i.keyspace_id, i.bucket_id, i.is_primary, i.index_key, i.`condition`, i.`partition`, i.`with`
                 FROM system:indexes AS i
                 WHERE (i.bucket_id = {0} OR (i.bucket_id IS MISSING AND i.keyspace_id = {0})) AND i.`using` = 'gsi'
                 ORDER BY i.keyspace_id, i.name",
                serde_json::to_string(&b).unwrap_or_default()
            ))
            .await?;
        for r in rows {
            let has_bucket = r.get("bucket_id").is_some();
            let scope = if has_bucket { r.get("scope_id").map(text).unwrap_or_default() } else { "_default".into() };
            let coll = if has_bucket { r.get("keyspace_id").map(text).unwrap_or_default() } else { "_default".into() };
            let schema = format!("{b}.{scope}");
            if let Some(t) = out.iter_mut().find(|t| t.name == coll && t.schema.as_deref() == Some(schema.as_str())) {
                t.indexes.push(ddl::index_from_row(&r));
            }
        }
        Ok(out)
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let b = self.bucket()?;
        match obj.kind.as_str() {
            kinds::INDEX => {
                let scope = obj.schema().and_then(split_schema).map(|(_, s)| s.to_string());
                Ok(self
                    .indexes()
                    .await?
                    .into_iter()
                    .find(|(s, _, n, ..)| *n == obj.name && scope.as_deref().is_none_or(|x| x == s))
                    .map(|(.., def)| format!("{def};")))
            }
            kinds::COLLECTION => {
                let scope = obj.schema().and_then(split_schema).map(|(_, s)| s.to_string()).unwrap_or_else(|| "_default".into());
                let mut lines = Vec::new();
                let max_ttl = self
                    .conn
                    .mgmt_get(&format!("/pools/default/buckets/{}/scopes", encode(&b)))
                    .await
                    .ok()
                    .and_then(|v| {
                        v.get("scopes")?
                            .as_array()?
                            .iter()
                            .find(|s| s.get("name").map(text).as_deref() == Some(scope.as_str()))?
                            .get("collections")?
                            .as_array()?
                            .iter()
                            .find(|c| c.get("name").map(text).as_deref() == Some(obj.name.as_str()))?
                            .get("maxTTL")?
                            .as_i64()
                    })
                    .filter(|t| *t > 0);
                let p = path(Some(&format!("{b}.{scope}")), &obj.name);
                lines.push(match max_ttl {
                    Some(t) => format!("CREATE COLLECTION {p} WITH {{\"maxTTL\": {t}}};"),
                    None => format!("CREATE COLLECTION {p};"),
                });
                for (s, c, _, _, def) in self.indexes().await.unwrap_or_default() {
                    if s == scope && c == obj.name && !def.is_empty() {
                        lines.push(format!("{def};"));
                    }
                }
                Ok(Some(lines.join("\n")))
            }
            kinds::FUNCTION => {
                let r = self
                    .results(&format!("SELECT RAW f FROM system:functions AS f WHERE f.identity.name = {}", serde_json::to_string(&obj.name).unwrap_or_default()))
                    .await?;
                Ok(r.first().map(|f| {
                    let params = f.pointer("/definition/parameters").and_then(Value::as_array).map(|a| a.iter().map(text).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    match f.pointer("/definition/expression").map(text) {
                        Some(e) => format!("CREATE OR REPLACE FUNCTION {}({params}) {{\n    {e}\n}};", q(&obj.name)),
                        None => serde_json::to_string_pretty(f).unwrap_or_default(),
                    }
                }))
            }
            _ => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT META(d).id AS _id, d.*\nFROM {} AS d\nLIMIT {limit}", path(obj.schema(), &obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let own = out.current_statement.is_none();
        let units = split_script(text, &dialect());
        for (i, u) in units.iter().filter(|u| u.kind != StatementKind::ClientCommand).enumerate() {
            let step = Step::start(out, own, i, u.start, u.line);
            let r = self.run_stmt(&u.text, max_rows, out).await;
            step.end(out, r)?;
            if let Some(ks) = created_collection(&u.text) {
                self.wait_for(&ks).await;
            }
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(Some(if self.txid.is_some() { TxState::Open } else { TxState::Idle }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.end_tx("COMMIT").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.end_tx("ROLLBACK").await
    }

    /// Estimated: `EXPLAIN` of each DML statement (it doesn't run them).
    /// Actual: each statement runs with `profile: timings`; the executed
    /// plan comes back with per-operator figures on Enterprise Edition.
    /// Community Edition doesn't profile: the estimated plan is shown then.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for stmt in split_statements(text) {
            let plannable = explainable(&stmt);
            if !analyze {
                if plannable {
                    let p = self.explain_plan(&stmt).await?;
                    out.plans.push(p);
                } else {
                    out.info(format!("Sin plan (no se ejecutó): {}", stmt.chars().take(80).collect::<String>()));
                }
                continue;
            }
            let v = self.run(&stmt, Some(json!({"profile": "timings"})), max_rows, out).await?;
            if let Some(t) = v.pointer("/profile/executionTimings") {
                out.plans.push(plan::from_json(&stmt, t, true));
            } else if plannable {
                let mut p = self.explain_plan(&stmt).await?;
                if let Some(m) = v.get("metrics").and_then(Value::as_object) {
                    for (k, val) in m {
                        p.root.props.push((format!("ejecución: {k}"), text_of(val)));
                    }
                }
                out.plans.push(p);
                out.info("El servidor no devolvió el perfil de ejecución (es una función de Couchbase Enterprise): se muestra el plan estimado.");
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
            let Some(id) = cancel.current.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                let stmt = format!("DELETE FROM system:active_requests WHERE clientContextID = {}", serde_json::to_string(&id).unwrap_or_default());
                if let Err(e) = conn.post_query(&json!({"statement": stmt})).await {
                    tracing::debug!("couchbase cancel failed: {e}");
                }
            });
        }))
    }

    /// A Couchbase bucket of 100 MB without flush (see [`create_db`]).
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.create_database_with_impl(name, &std::collections::BTreeMap::new()).await
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.conn.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()));
        }
        let rb = self.conn.http.delete(format!("{}/pools/default/buckets/{}", self.conn.mgmt, encode(name)));
        self.conn.mgmt_send(rb).await.map(|_| ())
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        match self.profiler.take() {
            Some(state) => profiler::stop(self, state).await,
            None => Ok(()),
        }
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

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        processes::processes(self).await
    }

    /// Requests are all the Query service knows: cancelling one deletes it
    /// from `system:active_requests`.
    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        processes::cancel(self, id).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        let pool = match self.cancel.run(self.conn.mgmt_get("/pools/default")).await {
            Ok(p) => Some(p),
            Err(e) => {
                snap.notes.push(format!("No se pudo leer el cluster manager ({e}): faltan CPU, memoria, disco y buckets (hace falta el rol de lectura del cluster)."));
                None
            }
        };
        let buckets = self.cancel.run(self.conn.mgmt_get("/pools/default/buckets?skipMap=true")).await.ok();
        let vitals = self
            .cancel
            .run(async {
                let resp = self
                    .conn
                    .http
                    .get(format!("{}/admin/vitals", self.conn.query))
                    .basic_auth(&self.conn.user, Some(&self.conn.password))
                    .send()
                    .await
                    .map_err(http_error)?;
                resp.json::<Value>().await.map_err(Error::query)
            })
            .await
            .ok();
        let range = self
            .cancel
            .run(async {
                let body = json!([
                    {"metric": [{"label": "name", "value": "kv_curr_connections"}], "applyFunctions": ["sum"], "start": -10, "step": 10},
                    {"metric": [{"label": "name", "value": "n1ql_requests"}], "applyFunctions": ["sum"], "start": -10, "step": 10}
                ]);
                let text = self.conn.mgmt_send(self.conn.http.post(format!("{}/pools/default/stats/range", self.conn.mgmt)).json(&body)).await?;
                serde_json::from_str::<Value>(&text).map_err(Error::query)
            })
            .await
            .ok();
        let last = |i: usize| -> Option<f64> {
            let vals = range.as_ref()?.get(i)?.get("data")?.as_array()?.first()?.get("values")?.as_array()?.last()?.get(1)?.as_str()?.parse().ok();
            vals
        };
        let active = self.results("SELECT r.requestId, r.users, r.state, r.elapsedTime, r.statement, r.remoteAddr FROM system:active_requests AS r").await.ok();
        let completed = self
            .results("SELECT c.requestTime, c.elapsedTime, c.resultCount, c.state, c.users, c.statement FROM system:completed_requests AS c ORDER BY c.requestTime DESC LIMIT 20")
            .await
            .ok();

        let nodes: Vec<Value> = pool.as_ref().and_then(|p| p.get("nodes")).and_then(Value::as_array).cloned().unwrap_or_default();
        let nsum = |f: &dyn Fn(&Value) -> Option<f64>| -> Option<f64> {
            let v: Vec<f64> = nodes.iter().filter_map(f).collect();
            (!v.is_empty()).then(|| v.iter().sum())
        };
        let cpu = nsum(&|n| num(n.pointer("/systemStats/cpu_utilization_rate"))).map(|s| s / nodes.len().max(1) as f64);
        let mem_total = nsum(&|n| num(n.pointer("/systemStats/mem_total")));
        let mem_free = nsum(&|n| num(n.pointer("/systemStats/mem_free")));
        let istat = |k: &'static str| nsum(&move |n: &Value| num(n.get("interestingStats").and_then(|s| s.get(k))));
        let (gets, hits) = (istat("cmd_get"), istat("get_hits"));
        let st = pool.as_ref().and_then(|p| p.get("storageTotals"));
        let v = |k: &str| vitals.as_ref().and_then(|x| num(x.get(k)));
        use MetricUnit::*;
        snap.metrics = vec![
            Metric::new("cpu", "CPU del servidor", "CPU", Percent, cpu),
            Metric::new("cpu_query", "CPU del servicio de consultas", "CPU", Percent, v("cpu.user.percent").zip(v("cpu.sys.percent")).map(|(u, s)| (u + s) * 100.0)),
            Metric::new("mem_used", "Memoria usada (hosts)", "Memoria", Bytes, mem_total.zip(mem_free).map(|(t, f)| t - f)).max(mem_total),
            Metric::new("mem_cache", "RAM de datos (buckets)", "Memoria", Bytes, istat("mem_used")).max(st.and_then(|s| num(s.pointer("/ram/quotaTotal")))),
            Metric::new("connections", "Conexiones (datos)", "Conexiones", Count, last(0)),
            Metric::new("active_sessions", "Consultas en curso", "Conexiones", Count, v("request.active.count").or(active.as_ref().map(|a| a.len().saturating_sub(1) as f64))),
            Metric::new("queued", "Consultas en cola", "Conexiones", Count, v("request.queued.count")),
            Metric::new("queries", "Consultas", "Actividad", Count, last(1).or(v("request.completed.count"))).counter(),
            Metric::new("ops", "Operaciones de datos por segundo", "Actividad", Count, istat("ops")),
            Metric::new("query_time", "Tiempo medio de consulta", "Actividad", Millis, vitals.as_ref().and_then(|x| x.get("request_time.mean")).and_then(Value::as_str).and_then(plan::duration_ms)),
            Metric::new("cache_hit", "Aciertos de caché (gets)", "Caché", Percent, gets.zip(hits).filter(|(g, _)| *g > 0.0).map(|(g, h)| h / g * 100.0)),
            Metric::new("disk_fetches", "Lecturas desde disco por segundo", "Disco", Count, istat("ep_bg_fetched")),
            Metric::new("storage_used", "Espacio en disco de los datos", "Almacenamiento", Bytes, istat("couch_docs_actual_disk_size")).max(st.and_then(|s| num(s.pointer("/hdd/total")))),
            Metric::new("items", "Documentos", "Datos", Count, istat("curr_items")),
            Metric::new("uptime", "Tiempo activo", "Servidor", Seconds, nodes.iter().filter_map(|n| num(n.get("uptime"))).reduce(f64::min)),
        ];

        let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["host", "servicios", "estado", "membresía", "versión", "CPU %", "memoria libre", "tiempo activo (s)"]);
        for n in &nodes {
            t.rows.push(vec![
                n.get("hostname").cloned().unwrap_or(Value::Null),
                Value::String(n.get("services").and_then(Value::as_array).map(|a| a.iter().map(text).collect::<Vec<_>>().join(", ")).unwrap_or_default()),
                n.get("status").cloned().unwrap_or(Value::Null),
                n.get("clusterMembership").cloned().unwrap_or(Value::Null),
                n.get("version").cloned().unwrap_or(Value::Null),
                n.pointer("/systemStats/cpu_utilization_rate").cloned().unwrap_or(Value::Null),
                n.pointer("/systemStats/mem_free").cloned().unwrap_or(Value::Null),
                n.get("uptime").cloned().unwrap_or(Value::Null),
            ]);
        }
        snap.tables.push(t);
        let mut t = MonitorTable::new("databases", "Buckets", &["bucket", "tipo", "documentos", "RAM usada", "cuota RAM", "% cuota", "disco", "datos", "ops/s"]);
        for b in buckets.as_ref().and_then(Value::as_array).into_iter().flatten().take(200) {
            let bs = |k: &str| b.pointer(&format!("/basicStats/{k}")).cloned().unwrap_or(Value::Null);
            t.rows.push(vec![
                b.get("name").cloned().unwrap_or(Value::Null),
                b.get("bucketType").cloned().unwrap_or(Value::Null),
                bs("itemCount"),
                bs("memUsed"),
                b.pointer("/quota/ram").cloned().unwrap_or(Value::Null),
                bs("quotaPercentUsed"),
                bs("diskUsed"),
                bs("dataUsed"),
                bs("opsPerSec"),
            ]);
        }
        snap.tables.push(t);
        let mut t = MonitorTable::new("queries", "Consultas en curso", &["id", "usuarios", "estado", "duración", "cliente", "consulta"]);
        for r in active.iter().flatten().filter(|r| !r.get("statement").map(text).unwrap_or_default().contains("system:active_requests")).take(200) {
            t.rows.push(vec![
                r.get("requestId").cloned().unwrap_or(Value::Null),
                r.get("users").cloned().unwrap_or(Value::Null),
                r.get("state").cloned().unwrap_or(Value::Null),
                r.get("elapsedTime").cloned().unwrap_or(Value::Null),
                r.get("remoteAddr").cloned().unwrap_or(Value::Null),
                Value::String(r.get("statement").map(text).unwrap_or_default().chars().take(2000).collect()),
            ]);
        }
        snap.tables.push(t);
        let mut t = MonitorTable::new("completed", "Consultas recientes lentas", &["inicio", "duración", "filas", "estado", "usuarios", "consulta"]);
        for r in completed.iter().flatten() {
            t.rows.push(vec![
                r.get("requestTime").cloned().unwrap_or(Value::Null),
                r.get("elapsedTime").cloned().unwrap_or(Value::Null),
                r.get("resultCount").cloned().unwrap_or(Value::Null),
                r.get("state").cloned().unwrap_or(Value::Null),
                r.get("users").cloned().unwrap_or(Value::Null),
                Value::String(r.get("statement").map(text).unwrap_or_default().chars().take(2000).collect()),
            ]);
        }
        snap.tables.push(t);

        if let Some(p) = &pool {
            if let Some(name) = p.get("clusterName").map(text).filter(|n| !n.is_empty()) {
                snap.info.push(("Cluster".into(), name));
            }
            if let Some(q) = num(p.get("memoryQuota")) {
                snap.info.push(("Cuota de memoria de datos".into(), format!("{q} MiB")));
            }
        }
        if let Some(n) = nodes.first() {
            snap.info.push(("Versión".into(), n.get("version").map(text).unwrap_or_default()));
        }
        snap.info.push(("Nodos".into(), nodes.len().to_string()));
        if let Some(x) = &vitals {
            snap.info.push(("Servicio de consultas".into(), x.get("version").map(text).unwrap_or_default()));
            snap.info.push(("Núcleos (consultas)".into(), x.get("cores").map(text).unwrap_or_default()));
        } else {
            snap.notes.push("No se pudo leer /admin/vitals del servicio de consultas: faltan su CPU y los tiempos de consulta.".into());
        }
        snap.notes.push("Las consultas recientes son las que superaron el umbral de system:completed_requests (1 s por defecto).".into());
        snap.notes.push("Operaciones, aciertos de caché y lecturas de disco son tasas por segundo que calcula Couchbase.".into());
        Ok(snap)
    }

    /// `checkPermissions` of the cluster manager (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        self.index_usage_report(table).await
    }
}

fn text_of(v: &Value) -> String {
    text(v)
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
            let grant = |grantable| d.schema_grant_script(Some("b"), "s", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    #[test]
    fn documents_become_rows() {
        let r = vec![json!({"_id": "k1", "a": 1, "n": {"z": true}}), json!({"_id": "k2", "b": [1, 2], "a": 9007199254740993i64})];
        let (cols, rows) = tabulate(&r, None);
        assert_eq!(cols, ["_id", "a", "n", "b"]);
        assert_eq!(rows[0], vec![json!("k1"), json!(1), json!("{\"z\":true}"), Value::Null]);
        assert_eq!(rows[1][1], json!("9007199254740993"));
        let (cols, _) = tabulate(&[json!({"b": 1})], Some(&json!({"a": "json", "b": "number"})));
        assert_eq!(cols, ["a", "b"]);
        let (cols, rows) = tabulate(&[json!("8.0.2"), json!(null)], None);
        assert_eq!((cols, rows.len()), (vec!["value".to_string()], 2));
        let (cols, rows) = tabulate(&[], Some(&json!({"_id": "json", "*": "*"})));
        assert_eq!((cols, rows.len()), (vec!["_id".to_string()], 0));
    }

    #[test]
    fn errors_and_statements() {
        assert!(matches!(query_error(&[json!({"code": 10000, "msg": "Authentication Failed"})], None), Error::AuthFailed(_)));
        let e = query_error(&[json!({"code": 3000, "msg": "syntax error", "line": 2, "column": 3})], Some("SELECT\n  SELEC 1")).to_script_error();
        assert_eq!((e.code.as_deref(), e.line, e.offset), (Some("3000"), Some(2), Some(9)));
        assert!(query_error(&[json!({"code": 12003, "msg": "x"}), json!({"msg": "y"})], None).to_string() == "x\ny");
        let e = json!({"code": 17007, "msg": "Commit Transaction statement error", "cause": {"cause": {"error_description": "Durability requirements are impossible to achieve"}}});
        assert_eq!(query_error(&[e], None).to_string(), "Commit Transaction statement error (Durability requirements are impossible to achieve)");
        assert!(tx_gone(&query_error(&[json!({"code": 17004, "msg": "transaction (x) is not present"})], None)));
        assert_eq!(tx_effect("begin work"), TxEffect::Begin);
        assert_eq!(tx_effect("START TRANSACTION ISOLATION LEVEL READ COMMITTED"), TxEffect::Begin);
        assert_eq!(tx_effect("COMMIT WORK"), TxEffect::Commit);
        assert_eq!(tx_effect("ROLLBACK TRANSACTION TO SAVEPOINT s1"), TxEffect::Other);
        assert_eq!(tx_effect("rollback"), TxEffect::Rollback);
        assert_eq!(tx_effect("UPSERT INTO b VALUES ('k', {})"), TxEffect::Data);
        assert_eq!(tx_effect("CREATE INDEX i ON b(a)"), TxEffect::Other);
        assert_eq!(txid_of(&json!({"results": [{"txid": "t1"}]})).as_deref(), Some("t1"));
        assert!(explainable("select 1") && explainable("UPSERT INTO x VALUES ('k', {})") && !explainable("CREATE INDEX i ON c(a)"));
        assert_ne!(context_id(), context_id());
        assert_eq!(created_collection("CREATE COLLECTION `b`.`s`.`c` IF NOT EXISTS").as_deref(), Some("`b`.`s`.`c`"));
        assert_eq!(created_collection("create collection b.s.c WITH {\"maxTTL\": 1}").as_deref(), Some("b.s.c"));
        assert_eq!(created_collection("CREATE COLLECTION IF NOT EXISTS b.s.c").as_deref(), Some("b.s.c"));
        assert_eq!(created_collection("CREATE INDEX i ON c(a)"), None);
    }

    #[test]
    fn browse_and_info() {
        let d = drivers();
        assert!(d[0].capabilities().monitor && d[0].supports_explain());
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let s = CbSession {
            conn: Arc::new(Conn { http: reqwest::Client::new(), query: String::new(), mgmt: String::new(), user: String::new(), password: String::new(), read_only: true }),
            bucket: Some("b".into()),
            cancel: Arc::new(Cancel::default()),
            rt: rt.handle().clone(),
            profiler: None,
            txid: None,
            manual: false,
        };
        let o = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("b.inv".into()), name: "hotel".into() };
        assert_eq!(s.browse_query(&o, 5), "SELECT META(d).id AS _id, d.*\nFROM `b`.`inv`.`hotel` AS d\nLIMIT 5");
        let b = s.body("select 1", Some("x"));
        assert_eq!(b["query_context"], json!("default:`b`.`_default`"));
        assert_eq!(b["readonly"], json!(true));
        // Read-only refuses bucket create/drop before any request.
        let mut s = s;
        assert!(matches!(rt.block_on(s.create_database("x")), Err(Error::Query(m)) if m.contains("solo lectura")));
        assert!(matches!(rt.block_on(s.drop_database("b")), Err(Error::Query(m)) if m.contains("solo lectura")));
    }
}

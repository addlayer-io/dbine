//! Apache CouchDB over its HTTP API (reqwest, basic auth).
//!
//! # Query language (`Language::Json`)
//!
//! A script holds one or more statements:
//!
//! - **Mango queries**: a JSON document, sent to `POST /{db}/_find`:
//!   `{"selector": {"age": {"$gt": 30}}, "fields": ["name"], "sort": [{"name": "asc"}], "limit": 20}`.
//!   Without `limit`, the driver asks for `max_rows + 1` documents (CouchDB
//!   would stop at 25).
//! - **HTTP console lines**: `METHOD path [JSON body]`, the body on the
//!   same line or on the following ones:
//!   `GET /mydb/_all_docs?include_docs=true&limit=10`,
//!   `POST /mydb/_find {"selector": {}}`,
//!   `GET _design/app/_view/by_name?limit=5`.
//!   A path starting with `/` is relative to the server; otherwise it's
//!   relative to the session's database. `POST`/`PUT` take the first JSON
//!   value after them as their body; any other JSON that follows is a
//!   Mango query.
//!
//! Generated scripts (templates, `table_ddl`, `insert_script`) use this
//! same syntax with paths relative to the session's database:
//! `PUT _design/app {…}`, `POST _index {…}`, `POST _bulk_docs {"docs": […]}`.
//! DBine extension: `DELETE _index/<name>` drops a Mango index by its name
//! alone (CouchDB wants `DELETE _index/<design doc>/json/<name>`): the
//! driver looks its design document up in `GET _index` first.
//!
//! Several statements follow each other: each HTTP line starts a new one,
//! and several Mango documents can be written one after the other. Lines
//! starting with `//` or `#` are comments.
//!
//! # Results
//!
//! `docs` (from `_find`), `rows` (views, `_all_docs`; a row's `doc` when
//! `include_docs=true`) and `results` (`_changes`) become one row per
//! document; a top-level array one row per element; any other reply a
//! single row. Columns are the union of top-level keys (`_id` first);
//! nested values are compact JSON. Mango warnings go to the messages.
//!
//! # Read-only and cancel
//!
//! Read-only connections allow `GET`/`HEAD` and `POST` only to read
//! endpoints (`_find`, `_explain`, `_all_docs`, `_design_docs`, `_bulk_get`,
//! views, `_changes`, `_dbs_info`). Every call is one HTTP request, so
//! dropping the session cancels it; there's no interrupter.

mod ddl;
mod index_usage;
mod sync;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod security;
mod steps;
mod transfer;

use dbine_driver::{
    async_trait, json_f64, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig,
    CreateTemplate, DbObject, DdlParts, Driver, DriverInfo, Error, Family, Field, Language, MonitorSnapshot, ObjectKindInfo,
    ObjectRef, QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Syntax help for the editor.
pub const QUERY_HELP: &str = "Consultas Mango en JSON (se envían a POST /base/_find):\n\
{ \"selector\": { \"edad\": { \"$gt\": 30 } }, \"fields\": [\"nombre\"], \"limit\": 20 }\n\
O líneas de consola HTTP, con el cuerpo JSON en la misma línea o en las siguientes:\n\
GET /base/_all_docs?include_docs=true&limit=10\n\
POST /base/_find { \"selector\": {} }\n\
GET _design/app/_view/por_nombre?limit=5   (sin «/» inicial, relativo a la base actual)\n\
PUT _design/app { \"views\": { … } } · POST _index { \"index\": { \"fields\": [\"campo\"] } } · POST _bulk_docs { \"docs\": [ … ] }\n\
PUT /nueva_base · DELETE /base (crear y borrar bases)";

/// The pseudo-object for all the documents of a database.
pub const ALL_DOCS: &str = "_all_docs";

const TIMEOUT: Duration = Duration::from_secs(15);

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(CouchDriver)]
}

pub struct CouchDriver;

fn info() -> &'static DriverInfo {
    static INFO: OnceLock<DriverInfo> = OnceLock::new();
    INFO.get_or_init(|| DriverInfo {
        id: "couchdb",
        name: "CouchDB",
        family: Family::Document,
        language: Language::Json,
        dialect: "",
        default_port: 5984,
        fields: vec![
            Field::host().placeholder("localhost").help("Un nombre de host o una URL completa (https://servidor:6984)."),
            Field::port(),
            Field::database(),
            Field::username(),
            Field::password(),
            Field::encrypt().help("Usar https."),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(kinds::COLLECTION, "Documentos", true, true, true),
            ObjectKindInfo::new(kinds::VIEW, "Vistas", true, true, true),
        ],
    })
}

/// Characters escaped in a path segment (database, design doc, view).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'-').remove(b'.');

fn seg(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// `http(s)://host:port`, without a trailing slash.
pub fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.trim_end_matches('/').to_string();
    }
    let scheme = if cfg.encrypt { "https" } else { "http" };
    format!("{scheme}://{host}:{}", cfg.port_or(5984))
}

#[async_trait]
impl Driver for CouchDriver {
    fn info(&self) -> &DriverInfo {
        info()
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// `_all_docs` and the Mango indexes, without counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    /// `_bulk_docs` in chunks (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: false,
            monitor: true,
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
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

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let http = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .user_agent("DBine")
            .build()
            .map_err(Error::connect)?;
        let db = database.filter(|d| !d.is_empty()).unwrap_or(&cfg.database).to_string();
        let auth = cfg.username.clone().filter(|u| !u.is_empty()).map(|u| (u, cfg.password.clone()));
        let mut s = CouchSession { http, base: base_url(cfg), db, auth, read_only: cfg.read_only, version: String::new() };
        let welcome = s.call(Method::GET, "/", None).await.map_err(|e| if e.is_query() { Error::Connect(e.to_string()) } else { e })?;
        s.version = welcome.get("version").and_then(Value::as_str).unwrap_or("?").to_string();
        // With no database chosen, the first user database.
        if s.db.is_empty() {
            s.db = s.list_databases().await.ok().and_then(|v| v.into_iter().next()).unwrap_or_default();
        }
        Ok(Box::new(s))
    }
}

pub struct CouchSession {
    http: reqwest::Client,
    base: String,
    db: String,
    auth: Option<(String, Option<String>)>,
    read_only: bool,
    version: String,
}

impl CouchSession {
    /// One request; `path` starts with `/`. Non-2xx → error with the
    /// server's `error: reason`.
    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut rq = self.http.request(method, format!("{}{path}", self.base)).header("Accept", "application/json");
        if let Some((u, p)) = &self.auth {
            rq = rq.basic_auth(u, p.as_deref());
        }
        if let Some(b) = body {
            rq = rq.json(b);
        }
        let resp = rq.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        let v: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        if status.is_success() {
            return Ok(v);
        }
        let msg = match (&v.get("error"), &v.get("reason")) {
            (Some(e), Some(r)) => format!("{}: {}", as_text(e), as_text(r)),
            _ => format!("HTTP {status}: {}", as_text(&v)),
        };
        if status == StatusCode::UNAUTHORIZED {
            return Err(Error::AuthFailed(msg));
        }
        // CouchDB's `error` ("not_found", "bad_request", "conflict"…) is its code.
        Err(match v.get("error").map(as_text).filter(|c| !c.is_empty()) {
            Some(code) => Error::Statement(Box::new(dbine_driver::ScriptError::new(msg).with_code(code))),
            None => Error::Query(msg),
        })
    }

    fn db_path(&self) -> Result<String> {
        if self.db.is_empty() {
            return Err(Error::Query("Elegí una base de datos.".into()));
        }
        Ok(format!("/{}", seg(&self.db)))
    }

    /// Path of a view object `ddoc/view`.
    fn view_path(&self, name: &str) -> Result<String> {
        let (d, v) = name.split_once('/').ok_or_else(|| Error::Query(format!("vista inválida: {name}")))?;
        Ok(format!("{}/_design/{}/_view/{}", self.db_path()?, seg(d), seg(v)))
    }

    async fn sample(&self, obj: &ObjectRef) -> Result<Vec<Value>> {
        let r = if obj.kind == kinds::VIEW {
            self.call(Method::GET, &format!("{}?limit=100", self.view_path(&obj.name)?), None).await?
        } else {
            self.call(Method::POST, &format!("{}/_find", self.db_path()?), Some(&json!({ "selector": {}, "limit": 100 })))
                .await?
        };
        Ok(documents(&r).unwrap_or_default())
    }
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A statement of the script.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// A Mango query for `_find`.
    Mango(Value),
    /// `METHOD path [body]`.
    Http { method: String, path: String, body: Option<Value> },
}

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "COPY"];

fn http_line(line: &str) -> Option<(String, String, &str)> {
    let t = line.trim_start();
    let (m, rest) = t.split_once(char::is_whitespace)?;
    let m = m.to_ascii_uppercase();
    if !METHODS.contains(&m.as_str()) {
        return None;
    }
    let rest = rest.trim_start();
    let (path, tail) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    (!path.is_empty()).then(|| (m, path.to_string(), tail))
}

fn json_values(text: &str) -> std::result::Result<Vec<Value>, String> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| format!("JSON inválido: {e}"))
}

/// Split a script into statements.
pub fn parse_script(text: &str) -> std::result::Result<Vec<Stmt>, String> {
    parse_located(text).map(|v| v.into_iter().map(|(s, _)| s).collect()).map_err(|(e, _)| e)
}

/// [`parse_script`], with where each statement's block starts (its HTTP
/// line, or the first line of loose JSON). An error carries its block's.
pub fn parse_located(text: &str) -> std::result::Result<Vec<(Stmt, usize)>, (String, usize)> {
    // Blocks: an HTTP line plus the lines until the next HTTP line, or the
    // loose JSON before the first one.
    let mut blocks: Vec<(Option<(String, String)>, String, Option<usize>)> = vec![(None, String::new(), None)];
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at + (line.len() - line.trim_start().len());
        at += line.len();
        let line = line.strip_suffix('\n').unwrap_or(line);
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if let Some((m, p, tail)) = http_line(line) {
            blocks.push((Some((m, p)), format!("{tail}\n"), Some(start)));
        } else {
            let b = blocks.last_mut().expect("a block");
            if b.2.is_none() && !t.trim().is_empty() {
                b.2 = Some(start);
            }
            b.1.push_str(line);
            b.1.push('\n');
        }
    }
    let mut out = Vec::new();
    for (head, body, start) in blocks {
        let start = start.unwrap_or(0);
        let mut values = json_values(&body).map_err(|e| (e, start))?.into_iter();
        if let Some((method, path)) = head {
            // POST/PUT take the JSON after them as their body; loose JSON
            // after a GET is the next statement.
            let body = if matches!(method.as_str(), "POST" | "PUT") { values.next() } else { None };
            out.push((Stmt::Http { method, path, body }, start));
        }
        for v in values {
            if !v.is_object() {
                return Err((format!("se esperaba una consulta Mango {{\"selector\": …}}, no {v}"), start));
            }
            out.push((Stmt::Mango(v), start));
        }
    }
    Ok(out)
}

/// POST endpoints that only read.
const READ_POSTS: &[&str] = &["_find", "_explain", "_all_docs", "_design_docs", "_bulk_get", "_changes", "_dbs_info", "_all_dbs"];

/// `Some(what)` when the statement may write.
pub fn write_reason(stmt: &Stmt) -> Option<String> {
    let Stmt::Http { method, path, .. } = stmt else { return None };
    let clean = path.split('?').next().unwrap_or_default().trim_end_matches('/');
    let last = clean.rsplit('/').next().unwrap_or_default();
    let read = match method.as_str() {
        "GET" | "HEAD" => true,
        "POST" => READ_POSTS.contains(&last) || clean.contains("/_view/") || clean.contains("/_index/_explain"),
        _ => false,
    };
    (!read).then(|| format!("{method} {path}"))
}

/// The documents of a reply, if it holds a list of them.
pub fn documents(v: &Value) -> Option<Vec<Value>> {
    if let Value::Array(a) = v {
        return Some(a.iter().map(|x| if x.is_object() { x.clone() } else { json!({ "value": x }) }).collect());
    }
    let obj = v.as_object()?;
    if let Some(Value::Array(docs)) = obj.get("docs") {
        return Some(docs.clone());
    }
    if let Some(Value::Array(rows)) = obj.get("rows").or_else(|| obj.get("results")) {
        return Some(
            rows.iter()
                .map(|r| match r.get("doc") {
                    Some(d @ Value::Object(_)) => d.clone(),
                    _ => r.clone(),
                })
                .collect(),
        );
    }
    None
}

/// Top-level keys, `_id` first, then in order of appearance.
pub fn union_keys(docs: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut has_id = false;
    for d in docs {
        if let Some(o) = d.as_object() {
            for k in o.keys() {
                if k == "_id" {
                    has_id = true;
                } else if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    if has_id {
        keys.insert(0, "_id".into());
    }
    keys
}

/// A JSON value as a cell: nested values as compact JSON text.
pub fn cell(v: &Value) -> Value {
    match v {
        Value::Object(_) | Value::Array(_) => Value::String(v.to_string()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                json_i64(i)
            } else if let Some(u) = n.as_u64() {
                json_u64(u)
            } else {
                json_f64(n.as_f64().unwrap_or(0.0))
            }
        }
        other => other.clone(),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Columns from sample documents: types seen (most frequent first).
pub fn infer_columns(docs: &[Value]) -> Vec<ColumnInfo> {
    union_keys(docs)
        .into_iter()
        .map(|name| {
            let mut types: Vec<(&'static str, usize)> = Vec::new();
            let mut present = 0;
            let mut null_seen = false;
            for v in docs.iter().filter_map(|d| d.get(&name)) {
                present += 1;
                if v.is_null() {
                    null_seen = true;
                    continue;
                }
                let t = type_name(v);
                match types.iter_mut().find(|(n, _)| *n == t) {
                    Some(e) => e.1 += 1,
                    None => types.push((t, 1)),
                }
            }
            types.sort_by(|a, b| b.1.cmp(&a.1));
            ColumnInfo {
                data_type: if types.is_empty() { "null".into() } else { types.iter().map(|t| t.0).collect::<Vec<_>>().join("|") },
                nullable: present < docs.len() || null_seen,
                primary_key: name == "_id",
                auto_increment: false,
                default_value: None,
                name,
            }
        })
        .collect()
}

/// One row per document.
pub fn push_docs(out: &mut QueryOutcome, docs: &[Value], max_rows: usize) {
    let keys = union_keys(docs);
    out.begin_result(keys.iter().map(|k| ResultColumn { name: k.clone(), type_name: String::new() }).collect());
    for d in docs {
        out.push_row(keys.iter().map(|k| d.get(k).map_or(Value::Null, cell)).collect(), max_rows);
    }
}

#[async_trait]
impl Session for CouchSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(format!("CouchDB {}", self.version))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self.call(Method::GET, "/_all_dbs", None).await {
            Ok(Value::Array(a)) => {
                Ok(a.iter().filter_map(Value::as_str).filter(|n| !n.starts_with('_')).map(str::to_string).collect())
            }
            Ok(other) => Err(Error::Query(format!("respuesta inesperada de /_all_dbs: {other}"))),
            // Non-admins can't list databases (CouchDB 3): the session's one.
            Err(Error::Query(_) | Error::Statement(_) | Error::AuthFailed(_)) if !self.db.is_empty() => Ok(vec![self.db.clone()]),
            Err(e) => Err(e),
        }
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let r = self.call(Method::GET, &format!("{}/_design_docs?include_docs=true", self.db_path()?), None).await?;
        let mut out = vec![DbObject { kind: kinds::COLLECTION.into(), schema: None, name: ALL_DOCS.into(), parent: None }];
        let mut views = Vec::new();
        for row in r.get("rows").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(doc) = row.get("doc") else { continue };
            let ddoc = doc.get("_id").and_then(Value::as_str).unwrap_or_default().trim_start_matches("_design/");
            for v in doc.get("views").and_then(Value::as_object).map(|m| m.keys().collect::<Vec<_>>()).unwrap_or_default() {
                views.push(DbObject {
                    kind: kinds::VIEW.into(),
                    schema: None,
                    name: format!("{ddoc}/{v}"),
                    parent: Some(ddoc.to_string()),
                });
            }
        }
        views.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(views);
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let docs = self.sample(obj).await?;
        Ok(infer_columns(&docs))
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if obj.kind == kinds::VIEW {
            let (d, v) = obj.name.split_once('/').ok_or_else(|| Error::Query(format!("vista inválida: {}", obj.name)))?;
            let doc = self.call(Method::GET, &format!("{}/_design/{}", self.db_path()?, seg(d)), None).await?;
            let Some(view) = doc.get("views").and_then(|vs| vs.get(v)) else { return Ok(None) };
            let mut text = String::new();
            if let Some(lang) = doc.get("language").and_then(Value::as_str) {
                text.push_str(&format!("// language: {lang}\n"));
            }
            for part in ["map", "reduce"] {
                match view.get(part) {
                    Some(Value::String(src)) => text.push_str(&format!("// {part}\n{src}\n\n")),
                    Some(other) => text.push_str(&format!("// {part}\n{}\n\n", serde_json::to_string_pretty(other)?)),
                    None => {}
                }
            }
            if let Some(opts) = view.get("options") {
                text.push_str(&format!("// options\n{}\n", serde_json::to_string_pretty(opts)?));
            }
            return Ok(Some(text.trim_end().to_string()));
        }
        let path = self.db_path()?;
        let mut info = self.call(Method::GET, &path, None).await?;
        if let Ok(ix) = self.call(Method::GET, &format!("{path}/_index"), None).await {
            if let (Some(m), Some(list)) = (info.as_object_mut(), ix.get("indexes")) {
                m.insert("indexes".into(), list.clone());
            }
        }
        Ok(Some(serde_json::to_string_pretty(&info)?))
    }

    /// The `_all_docs` pseudo-table: fields from a sample of 100
    /// documents, `_id` as key, the Mango indexes and the design documents
    /// (see `ddl.rs`).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let path = self.db_path()?;
        let columns = infer_columns(&self.sample(&ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: ALL_DOCS.into() }).await?)
            .into_iter()
            .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
            .collect();
        let indexes = self.call(Method::GET, &format!("{path}/_index"), None).await?;
        let ddocs = self.call(Method::GET, &format!("{path}/_design_docs?include_docs=true"), None).await?;
        Ok(vec![ddl::all_docs_schema(columns, &indexes, &ddocs)])
    }

    /// `PUT /{name}`.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("crear una base")?;
        ddl::check_database_name(name)?;
        self.call(Method::PUT, &format!("/{}", seg(name)), None).await.map(|_| ())
    }

    /// `DELETE /{name}`.
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("borrar una base")?;
        if name.is_empty() || name.starts_with('_') {
            return Err(Error::Query(format!("«{name}» no es una base de usuario")));
        }
        self.call(Method::DELETE, &format!("/{}", seg(name)), None).await.map(|_| ())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        if obj.kind == kinds::VIEW {
            if let Some((d, v)) = obj.name.split_once('/') {
                return format!("GET /{}/_design/{}/_view/{}?limit={limit}", seg(&self.db), seg(d), seg(v));
            }
        }
        format!("{{\n  \"selector\": {{}},\n  \"limit\": {limit}\n}}")
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.snapshot().await
    }

    /// `/_active_tasks`: indexers, compactions and replications.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        CouchSession::processes(self).await
    }

    /// Only replications started with `_replicate` can be stopped.
    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_task(id).await
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

    /// Statements one by one; a script that doesn't parse runs nothing,
    /// and the first failing statement stops it (the app gets it whole).
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = parse_located(text).map_err(|(m, at)| {
            Error::from(dbine_driver::ScriptError::new(m).at_offset(at).at_line(steps::line_at(text, at)))
        })?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let own = out.current_statement.is_none();
        for (i, (stmt, at)) in stmts.into_iter().enumerate() {
            let step = steps::Step::start(out, own, i, at, steps::line_at(text, at));
            let r = match self.check_read_only(&stmt) {
                Ok(()) => self.send(&stmt, max_rows, false).await.map(|reply| push_reply(out, &reply, max_rows)),
                Err(e) => Err(e),
            };
            step.end(out, r)?;
        }
        Ok(())
    }

    /// Mango queries (and `POST …/_find` lines) get `_explain`'s plan;
    /// with `analyze` they run with `execution_stats: true` and the plan
    /// carries those figures. Other requests have no plan: with `analyze`
    /// they run as in `execute`, without it they're skipped.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = parse_script(text).map_err(Error::Query)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for stmt in stmts {
            let label = stmt_label(&stmt);
            let Some(explain_path) = self.explain_path(&stmt)? else {
                out.info(format!("`{label}`: solo las consultas Mango (_find) tienen plan de ejecución."));
                if analyze {
                    self.check_read_only(&stmt)?;
                    let reply = self.send(&stmt, max_rows, false).await?;
                    push_reply(out, &reply, max_rows);
                }
                continue;
            };
            let body = find_body(&stmt, max_rows);
            let explain = self.call(Method::POST, &explain_path, Some(&body)).await?;
            let stats = if analyze {
                let reply = self.send(&stmt, max_rows, true).await?;
                push_reply(out, &reply, max_rows);
                Some(reply.get("execution_stats").cloned().unwrap_or(Value::Null))
            } else {
                None
            };
            out.plans.push(plan::from_explain(&label, &explain, stats.as_ref()));
        }
        Ok(())
    }

    /// `_session` roles and the database's `_security` (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    /// Only `_all_docs` has indexes (views are indexes themselves).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        if table.name != ALL_DOCS {
            return Ok(None);
        }
        self.index_usage_report().await
    }
}

impl CouchSession {
    fn refuse_if_read_only(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se puede {what}.")));
        }
        Ok(())
    }

    fn check_read_only(&self, stmt: &Stmt) -> Result<()> {
        if self.read_only {
            if let Some(w) = write_reason(stmt) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó `{w}`. Solo se permiten lecturas (GET, _find, vistas…)."
                )));
            }
        }
        Ok(())
    }

    fn full_path(&self, path: &str) -> Result<String> {
        Ok(if path.starts_with('/') { path.to_string() } else { format!("{}/{path}", self.db_path()?) })
    }

    /// Where the statement's `_explain` goes, when it's a Mango query.
    fn explain_path(&self, stmt: &Stmt) -> Result<Option<String>> {
        Ok(match stmt {
            Stmt::Mango(_) => Some(format!("{}/_explain", self.db_path()?)),
            Stmt::Http { method, path, .. } if method == "POST" => {
                let full = self.full_path(path)?;
                let clean = full.split('?').next().unwrap_or_default().trim_end_matches('/');
                clean.strip_suffix("/_find").map(|p| format!("{p}/_explain"))
            }
            _ => None,
        })
    }

    /// Run one statement; `stats` asks `_find` for its execution stats.
    async fn send(&self, stmt: &Stmt, max_rows: usize, stats: bool) -> Result<Value> {
        match stmt {
            Stmt::Mango(_) => {
                let mut q = find_body(stmt, max_rows);
                if stats {
                    q["execution_stats"] = Value::Bool(true);
                }
                self.call(Method::POST, &format!("{}/_find", self.db_path()?), Some(&q)).await
            }
            Stmt::Http { method, path, body } => {
                let path = self.full_path(path)?;
                if method == "DELETE" {
                    if let Some(p) = self.index_by_name(&path).await? {
                        return self.call(Method::DELETE, &p, None).await;
                    }
                }
                let m = Method::from_bytes(method.as_bytes()).map_err(Error::query)?;
                if stats {
                    let q = find_body(stmt, max_rows);
                    let mut q = if q.is_object() { q } else { json!({}) };
                    q["execution_stats"] = Value::Bool(true);
                    return self.call(m, &path, Some(&q)).await;
                }
                self.call(m, &path, body.as_ref()).await
            }
        }
    }
}

/// `…/{db}/_index/<name>` (one segment after `_index`): the database path and the name.
fn index_shorthand(path: &str) -> Option<(&str, &str)> {
    let clean = path.split('?').next().unwrap_or_default();
    let (db, name) = clean.rsplit_once("/_index/")?;
    (!db.is_empty() && !name.is_empty() && !name.contains('/')).then_some((db, name))
}

impl CouchSession {
    /// `DELETE _index/<name>` (see the module docs): CouchDB's path for
    /// that index, with its design document; `None` for any other path.
    async fn index_by_name(&self, path: &str) -> Result<Option<String>> {
        let Some((db, name)) = index_shorthand(path) else { return Ok(None) };
        let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy().to_string();
        let list = self.call(Method::GET, &format!("{db}/_index"), None).await?;
        let found = list.get("indexes").and_then(Value::as_array).into_iter().flatten().find(|i| i.get("name").and_then(Value::as_str) == Some(name.as_str()));
        let Some(ix) = found else { return Err(Error::Query(format!("No hay un índice Mango llamado «{name}»."))) };
        let ddoc = ix.get("ddoc").and_then(Value::as_str).unwrap_or_default().trim_start_matches("_design/");
        if ddoc.is_empty() {
            return Err(Error::Query(format!("«{name}» es el índice de _id de CouchDB y no se borra.")));
        }
        let kind = ix.get("type").and_then(Value::as_str).unwrap_or("json");
        Ok(Some(format!("{db}/_index/{}/{}/{}", seg(ddoc), seg(kind), seg(&name))))
    }
}

/// The body sent to `_find`: the query, with a `limit` when it has none
/// (CouchDB would stop at 25).
fn find_body(stmt: &Stmt, max_rows: usize) -> Value {
    let mut q = match stmt {
        Stmt::Mango(q) => q.clone(),
        Stmt::Http { body, .. } => body.clone().unwrap_or_else(|| json!({ "selector": {} })),
    };
    if let (Stmt::Mango(_), Some(m)) = (stmt, q.as_object_mut()) {
        m.entry("limit").or_insert_with(|| json!(max_rows.saturating_add(1)));
    }
    q
}

/// One line describing a statement, for plans and messages.
fn stmt_label(stmt: &Stmt) -> String {
    let t = match stmt {
        Stmt::Mango(q) => q.to_string(),
        Stmt::Http { method, path, body } => match body {
            Some(b) => format!("{method} {path} {b}"),
            None => format!("{method} {path}"),
        },
    };
    if t.chars().count() > 300 {
        format!("{}…", t.chars().take(300).collect::<String>())
    } else {
        t
    }
}

fn push_reply(out: &mut QueryOutcome, reply: &Value, max_rows: usize) {
    if let Some(w) = reply.get("warning").and_then(Value::as_str) {
        out.info(w.to_string());
    }
    // `_bulk_docs` answers 201 even when some documents failed.
    if let Value::Array(items) = reply {
        let failed: Vec<&Value> = items.iter().filter(|i| i.get("error").is_some()).collect();
        if let Some(first) = failed.first() {
            out.info(format!(
                "{} de {} documentos fallaron; el primero ({}): {} {}",
                failed.len(),
                items.len(),
                first.get("id").map(as_text).unwrap_or_default(),
                first.get("error").map(as_text).unwrap_or_default(),
                first.get("reason").map(as_text).unwrap_or_default(),
            ));
        }
    }
    match documents(reply) {
        Some(docs) => push_docs(out, &docs, max_rows),
        None if reply.is_object() => push_docs(out, std::slice::from_ref(reply), max_rows),
        None => {
            out.begin_result(vec![ResultColumn { name: "result".into(), type_name: String::new() }]);
            out.push_row(vec![cell(reply)], max_rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_keep_their_block() {
        let t = "{\"selector\": {}}\n// c\nGET /db/_all_docs\n  POST _find {\"selector\": {\"a\": 1}}\n{\"selector\": {}}";
        let v = parse_located(t).unwrap();
        let at: Vec<usize> = v.iter().map(|(_, a)| *a).collect();
        assert_eq!(at, [0, 22, 42, 42]);
        let e = parse_located("GET /db\nPOST _find {nope").unwrap_err();
        assert_eq!(e.1, 8);
    }

    #[test]
    fn script_mixes_mango_and_http_lines() {
        let s = parse_script(
            "// comment\n{\"selector\": {}}\n{\"selector\": {\"a\": 1}, \"limit\": 2}\n\
             GET /db/_all_docs?include_docs=true\n\
             post _find\n{\n  \"selector\": {}\n}\n\
             DELETE /db/doc1?rev=1-x\n{\"selector\": {}}",
        )
        .unwrap();
        assert_eq!(s.len(), 6);
        assert!(matches!(&s[5], Stmt::Mango(_)));
        assert!(matches!(&s[0], Stmt::Mango(_)));
        assert_eq!(s[2], Stmt::Http { method: "GET".into(), path: "/db/_all_docs?include_docs=true".into(), body: None });
        assert_eq!(s[3], Stmt::Http { method: "POST".into(), path: "_find".into(), body: Some(json!({ "selector": {} })) });
        assert!(parse_script("{ selector: 1 }").is_err());
        assert!(parse_script("[1,2]").is_err());
    }

    #[test]
    fn read_only_rules() {
        let p = |t: &str| parse_script(t).unwrap().remove(0);
        assert_eq!(write_reason(&p("{\"selector\": {}}")), None);
        assert_eq!(write_reason(&p("GET /db/_all_docs")), None);
        assert_eq!(write_reason(&p("POST /db/_find {}")), None);
        assert_eq!(write_reason(&p("POST /db/_design/a/_view/b {}")), None);
        assert_eq!(write_reason(&p("POST /db/_all_docs {\"keys\": []}")), None);
        assert!(write_reason(&p("POST /db {}")).is_some());
        assert!(write_reason(&p("POST /db/_bulk_docs {}")).is_some());
        assert!(write_reason(&p("PUT /newdb")).is_some());
        assert!(write_reason(&p("DELETE /db/x")).is_some());
    }

    #[test]
    fn replies_become_documents() {
        let find = json!({ "docs": [{ "_id": "a", "n": 1 }], "bookmark": "x" });
        assert_eq!(documents(&find).unwrap().len(), 1);
        let all = json!({ "total_rows": 2, "rows": [{ "id": "a", "key": "a", "value": {}, "doc": { "_id": "a", "x": 1 } }] });
        assert_eq!(documents(&all).unwrap()[0], json!({ "_id": "a", "x": 1 }));
        let view = json!({ "rows": [{ "id": "a", "key": 1, "value": null }] });
        assert_eq!(documents(&view).unwrap()[0]["key"], json!(1));
        assert_eq!(documents(&json!(["db1", "db2"])).unwrap()[1], json!({ "value": "db2" }));
        assert!(documents(&json!({ "ok": true })).is_none());
    }

    #[test]
    fn flattening_and_inference() {
        let docs = vec![json!({ "n": 1, "_id": "a", "o": { "x": [1] } }), json!({ "_id": "b", "n": 2.5, "big": 9007199254740993_i64 })];
        let mut out = QueryOutcome::default();
        push_docs(&mut out, &docs, 10);
        let r = &out.results[0];
        assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["_id", "n", "o", "big"]);
        assert_eq!(r.rows[0][2], json!("{\"x\":[1]}"));
        assert_eq!(r.rows[1][3], json!("9007199254740993"));
        let cols = infer_columns(&docs);
        assert_eq!(cols[1].data_type, "integer|number");
        assert!(cols[2].nullable && !cols[0].nullable && cols[0].primary_key);
    }

    #[test]
    fn index_shorthand_paths() {
        assert_eq!(index_shorthand("/db/_index/ix_a"), Some(("/db", "ix_a")));
        assert_eq!(index_shorthand("/db/_index/_design/x/json/ix_a"), None);
        assert_eq!(index_shorthand("/db/_index"), None);
        assert_eq!(index_shorthand("/db/_find"), None);
    }

    #[test]
    fn urls() {
        let mut c = ConnectionConfig { host: "db.local".into(), ..Default::default() };
        assert_eq!(base_url(&c), "http://db.local:5984");
        c.encrypt = true;
        c.port = 6984;
        assert_eq!(base_url(&c), "https://db.local:6984");
        c.host = "https://x.example/couch/".into();
        assert_eq!(base_url(&c), "https://x.example/couch");
        assert_eq!(seg("a/b+c"), "a%2Fb%2Bc");
    }
}

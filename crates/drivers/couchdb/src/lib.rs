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

mod create_db;
mod ddl;
mod functions;
mod index_usage;
mod jobs;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod properties;
mod replication;
mod security;
mod stats;
mod steps;
mod sync;
mod tls;
mod transfer;

use dbine_driver::{
    async_trait, json_f64, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo,
    ConnectionConfig, CreateTemplate, DbObject, DdlParts, Driver, DriverInfo, Error, Family, Field,
    Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn,
    Session, TableSchema, TlsLevel, TlsSupport,
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
        tls: TlsSupport::Configurable,
        tls_port: Some(6984),
        id: "couchdb",
        name: "CouchDB",
        family: Family::Document,
        language: Language::Json,
        dialect: "",
        default_port: 5984,
        fields: vec![
            Field::host()
                .placeholder("localhost")
                .help("Un nombre de host o una URL completa (https://servidor:6984)."),
            Field::port(),
            Field::database(),
            Field::username(),
            Field::password(),
        ]
        .into_iter()
        .chain(tls_fields())
        .chain([Field::read_only()])
        .collect(),
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

/// The TLS options: the URL's `scheme`, and curl's `insecure` / `cacert`
/// (CouchDB's own documentation uses curl; it has no client names for
/// them). Saved connections from before them: `encrypt` gives https,
/// `trust` doesn't verify (an https URL typed in the host too).
fn tls_fields() -> Vec<Field> {
    vec![
        Field::tls_mode(
            "scheme",
            "scheme",
            &[
                ("http", "http", TlsLevel::Off),
                ("https", "https", TlsLevel::VerifyFull),
            ],
            "https",
            ["http", "http", "https", "https"],
        )
        .help("Una URL escrita en el servidor usa su propio esquema (http:// o https://)."),
        Field::tls_insecure(["false", "true", "false", "true"]),
        Field::tls_cacert(),
    ]
}

/// The TLS settings of a connection whose host may be a URL (its scheme
/// decides over `scheme`).
fn http_tls(cfg: &ConnectionConfig) -> Result<tls::HttpTls> {
    let host = cfg.host.trim();
    let url = (host.starts_with("http://") || host.starts_with("https://"))
        .then(|| host.starts_with("https://"));
    tls::HttpTls::new(cfg, &tls_fields(), url)
}

/// `http(s)://host:port`, without a trailing slash.
pub fn base_url(cfg: &ConnectionConfig, tls: &tls::HttpTls) -> String {
    let host = cfg.host.trim();
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return tls.url(host.trim_end_matches('/').to_string());
    }
    let scheme = tls.scheme();
    format!("{scheme}://{}:{}", tls.url_host(host), cfg.port_or(5984))
}

#[async_trait]
impl Driver for CouchDriver {
    fn info(&self) -> &DriverInfo {
        info()
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    /// One unit per HTTP line (with its body) or Mango query, numbered as
    /// `execute` numbers them (see [`split`]).
    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        split(text)
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

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields()
    }

    fn create_database_script(
        &self,
        name: &str,
        options: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        create_db::script(name, options)
    }

    /// "Propiedades" of a database (see [`properties`]).
    fn alter_database_script(
        &self,
        database: &str,
        changes: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
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
            jobs: true,
            scheduler: true,
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

    fn sync_script(
        &self,
        changes: &[dbine_driver::TableChange],
    ) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn insert_script(
        &self,
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<String> {
        ddl::insert_script(target, columns, rows)
    }

    fn update_script(
        &self,
        target: &ObjectRef,
        changes: &[dbine_driver::RowChange],
    ) -> Result<String> {
        ddl::update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    fn filtered_browse(
        &self,
        browse: &str,
        filters: &[dbine_driver::ColumnFilter],
    ) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    fn sorted_browse(&self, query: &str, sort: &[dbine_driver::SortKey]) -> Result<String> {
        ddl::sorted_browse(query, sort)
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        database: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let tls = http_tls(cfg)?;
        let http = tls
            .builder(
                reqwest::Client::builder()
                    .connect_timeout(TIMEOUT)
                    .user_agent("DBine"),
            )?
            .build()
            .map_err(Error::connect)?;
        let db = database
            .filter(|d| !d.is_empty())
            .unwrap_or(&cfg.database)
            .to_string();
        let auth = cfg
            .username
            .clone()
            .filter(|u| !u.is_empty())
            .map(|u| (u, cfg.password.clone()));
        let mut s = CouchSession {
            tls_pin: None,
            http,
            base: base_url(cfg, &tls),
            db,
            auth,
            read_only: cfg.read_only,
            version: String::new(),
        };
        let welcome = s.call(Method::GET, "/", None).await.map_err(|e| {
            if e.is_query() {
                Error::Connect(e.to_string())
            } else {
                tls.explain(e)
            }
        })?;
        s.version = welcome
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        // With no database chosen, the first user database.
        if s.db.is_empty() {
            s.db = s
                .list_databases()
                .await
                .ok()
                .and_then(|v| v.into_iter().next())
                .unwrap_or_default();
        }
        s.tls_pin = tls.pin_seen();
        Ok(Box::new(s))
    }
}

pub struct CouchSession {
    /// The certificate accepted on trust, to pin (`Session::tls_pin_seen`).
    tls_pin: Option<String>,
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
        let mut rq = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("Accept", "application/json");
        if let Some((u, p)) = &self.auth {
            rq = rq.basic_auth(u, p.as_deref());
        }
        if let Some(b) = body {
            rq = rq.json(b);
        }
        let resp = rq.send().await.map_err(|e| {
            if e.is_connect() {
                tls::net_error(&e)
            } else {
                Error::Connect(e.to_string())
            }
        })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let v: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
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
        Err(
            match v.get("error").map(as_text).filter(|c| !c.is_empty()) {
                Some(code) => Error::Statement(Box::new(
                    dbine_driver::ScriptError::new(msg).with_code(code),
                )),
                None => Error::Query(msg),
            },
        )
    }

    fn db_path(&self) -> Result<String> {
        if self.db.is_empty() {
            return Err(Error::Query("Elegí una base de datos.".into()));
        }
        Ok(format!("/{}", seg(&self.db)))
    }

    /// Path of a view object `ddoc/view`.
    fn view_path(&self, name: &str) -> Result<String> {
        let (d, v) = name
            .split_once('/')
            .ok_or_else(|| Error::Query(format!("vista inválida: {name}")))?;
        Ok(format!(
            "{}/_design/{}/_view/{}",
            self.db_path()?,
            seg(d),
            seg(v)
        ))
    }

    async fn sample(&self, obj: &ObjectRef) -> Result<Vec<Value>> {
        let r = if obj.kind == kinds::VIEW {
            self.call(
                Method::GET,
                &format!("{}?limit=100", self.view_path(&obj.name)?),
                None,
            )
            .await?
        } else {
            self.call(
                Method::POST,
                &format!("{}/_find", self.db_path()?),
                Some(&json!({ "selector": {}, "limit": 100 })),
            )
            .await?
        };
        Ok(documents(&r).unwrap_or_default())
    }
}

/// Text for a `//` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
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
    Http {
        method: String,
        path: String,
        body: Option<Value>,
    },
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

/// A JSON value with its start and end in the script, or why it doesn't
/// parse and where it starts.
type LocatedValue = std::result::Result<(Value, usize, usize), (String, usize)>;

/// The JSON values of `body` (which starts at byte `from` of the script),
/// up to the first one that doesn't parse.
fn located_values(body: &str, from: usize) -> Vec<LocatedValue> {
    let mut values = serde_json::Deserializer::from_str(body).into_iter::<Value>();
    let mut out = Vec::new();
    loop {
        let before = values.byte_offset();
        let rest = &body[before..];
        let start = from + before + (rest.len() - rest.trim_start_matches(JSON_SPACE).len());
        match values.next() {
            None => return out,
            Some(Ok(v)) => out.push(Ok((v, start, from + values.byte_offset()))),
            Some(Err(e)) => {
                out.push(Err((format!("JSON inválido: {e}"), start)));
                return out;
            }
        }
    }
}

const JSON_SPACE: [char; 4] = [' ', '\t', '\n', '\r'];

/// Split a script into statements.
pub fn parse_script(text: &str) -> std::result::Result<Vec<Stmt>, String> {
    parse_located(text)
        .map(|v| v.into_iter().map(|l| l.stmt).collect())
        .map_err(|(e, _)| e)
}

/// A statement and where it is in the script.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub stmt: Stmt,
    /// Byte offset of its first character: its HTTP line's method, or the
    /// `{` of a Mango query.
    pub start: usize,
    /// Byte offset of its end: the end of its path, or of its JSON value
    /// (a POST / PUT body, a Mango query).
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
}

/// A part of the script that doesn't parse: from where the failing
/// statement starts to the end of its block (up to the next HTTP line).
struct Bad {
    message: String,
    start: usize,
    end: usize,
    line: u32,
}

/// [`parse_script`], with where each statement is: its own start, end and
/// line, also for each of several Mango queries in a row. An error carries
/// where its statement starts (the HTTP line whose body doesn't parse, or
/// the loose JSON value that isn't a Mango query).
pub fn parse_located(text: &str) -> std::result::Result<Vec<Located>, (String, usize)> {
    scan(text)
        .into_iter()
        .map(|r| r.map_err(|b| (b.message, b.start)))
        .collect()
}

/// The script cut into the statements `execute` runs and numbers, in the
/// same order and at the same places ([`parse_located`]): an HTTP line with
/// its body, or a Mango query. Comment and blank lines before one aren't
/// part of it. What doesn't parse is still a unit (up to the next HTTP
/// line), without an `error`, and the rest of the script is still cut: a
/// Mango query being typed keeps the lenses of the others, and running it
/// reports the parse error at its place (`execute` refuses the script).
pub fn split(text: &str) -> Vec<dbine_driver::ScriptStatement> {
    scan(text)
        .into_iter()
        .map(|r| {
            let (start, end, line) = match r {
                Ok(l) => (l.start, l.end, l.line),
                Err(b) => (b.start, b.end, b.line),
            };
            dbine_driver::ScriptStatement {
                text: text[start..end].to_string(),
                start,
                end,
                line,
                kind: dbine_driver::StatementKind::Sql,
                repeat: 1,
                error: None,
            }
        })
        .collect()
}

/// 1-based lines of byte offsets, without counting from the start each time.
struct LineIndex(Vec<usize>);

impl LineIndex {
    fn new(text: &str) -> Self {
        Self(text.match_indices('\n').map(|(i, _)| i).collect())
    }

    fn line(&self, offset: usize) -> u32 {
        self.0.partition_point(|&nl| nl < offset) as u32 + 1
    }
}

/// Every statement of the script in order, and an `Err` for each part that
/// doesn't parse (the scan goes on with the next HTTP line). Before the
/// first `Err` it is what [`parse_located`] returns.
fn scan(text: &str) -> Vec<std::result::Result<Located, Bad>> {
    // Blocks: an HTTP line (method, path, where it starts and where its
    // path ends) plus the text after its path up to the next HTTP line; the
    // loose JSON before the first one has no HTTP line. Comment lines are
    // blanked out of `masked`, which keeps every offset of `text`.
    struct Block {
        head: Option<(String, String, usize, usize)>,
        from: usize,
        to: usize,
    }
    let mut masked = String::with_capacity(text.len());
    let mut blocks = vec![Block {
        head: None,
        from: 0,
        to: text.len(),
    }];
    let mut at = 0;
    for raw in text.split_inclusive('\n') {
        let line_start = at;
        at += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') {
            masked.extend(std::iter::repeat_n(' ', line.len()));
            masked.push_str(&raw[line.len()..]);
            continue;
        }
        masked.push_str(raw);
        if let Some((m, p, tail)) = http_line(line) {
            let start = line_start + (line.len() - t.len());
            let path_end = line_start + line.len() - tail.len();
            blocks.last_mut().expect("a block").to = line_start;
            blocks.push(Block {
                head: Some((m, p, start, path_end)),
                from: path_end,
                to: text.len(),
            });
        }
    }
    let lines = LineIndex::new(text);
    let mut out = Vec::new();
    for b in blocks {
        let body = &masked[b.from..b.to];
        // Where what fails in this block ends: past its last character that
        // isn't JSON whitespace (so never before where the failure starts).
        let block_end = b.from + body.trim_end_matches(JSON_SPACE).len();
        let bad = |message: String, start: usize| {
            Err(Bad {
                message,
                start,
                end: block_end,
                line: lines.line(start),
            })
        };
        let ok = |stmt: Stmt, start: usize, end: usize| {
            Ok(Located {
                stmt,
                start,
                end,
                line: lines.line(start),
            })
        };
        let mut values = located_values(body, b.from).into_iter();
        if let Some((method, path, start, path_end)) = b.head {
            // POST/PUT take the JSON after them as their body; loose JSON
            // after a GET is the next statement.
            let (mut body, mut end) = (None, path_end);
            if matches!(method.as_str(), "POST" | "PUT") {
                match values.next() {
                    Some(Ok((v, _, e))) => (body, end) = (Some(v), e),
                    Some(Err((message, _))) => {
                        out.push(bad(message, start));
                        continue;
                    }
                    None => {}
                }
            }
            out.push(ok(Stmt::Http { method, path, body }, start, end));
        }
        for v in values {
            match v {
                Ok((v, start, end)) if v.is_object() => out.push(ok(Stmt::Mango(v), start, end)),
                Ok((v, start, _)) => {
                    out.push(bad(
                        format!("se esperaba una consulta Mango {{\"selector\": …}}, no {v}"),
                        start,
                    ));
                    break;
                }
                Err((message, start)) => {
                    out.push(bad(message, start));
                    break;
                }
            }
        }
    }
    out
}

/// POST endpoints that only read.
const READ_POSTS: &[&str] = &[
    "_find",
    "_explain",
    "_all_docs",
    "_design_docs",
    "_bulk_get",
    "_changes",
    "_dbs_info",
    "_all_dbs",
];

/// `Some(what)` when the statement may write.
pub fn write_reason(stmt: &Stmt) -> Option<String> {
    let Stmt::Http { method, path, .. } = stmt else {
        return None;
    };
    let clean = path
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let last = clean.rsplit('/').next().unwrap_or_default();
    let read = match method.as_str() {
        "GET" | "HEAD" => true,
        "POST" => {
            READ_POSTS.contains(&last)
                || clean.contains("/_view/")
                || clean.contains("/_index/_explain")
        }
        _ => false,
    };
    (!read).then(|| format!("{method} {path}"))
}

/// The documents of a reply, if it holds a list of them.
pub fn documents(v: &Value) -> Option<Vec<Value>> {
    if let Value::Array(a) = v {
        return Some(
            a.iter()
                .map(|x| {
                    if x.is_object() {
                        x.clone()
                    } else {
                        json!({ "value": x })
                    }
                })
                .collect(),
        );
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
            types.sort_by_key(|t| std::cmp::Reverse(t.1));
            ColumnInfo {
                data_type: if types.is_empty() {
                    "null".into()
                } else {
                    types.iter().map(|t| t.0).collect::<Vec<_>>().join("|")
                },
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
    out.begin_result(
        keys.iter()
            .map(|k| ResultColumn {
                name: k.clone(),
                type_name: String::new(),
            })
            .collect(),
    );
    for d in docs {
        out.push_row(
            keys.iter()
                .map(|k| d.get(k).map_or(Value::Null, cell))
                .collect(),
            max_rows,
        );
    }
}

#[async_trait]
impl Session for CouchSession {
    fn tls_pin_seen(&self) -> Option<String> {
        self.tls_pin.clone()
    }
    async fn server_version(&mut self) -> Result<String> {
        Ok(format!("CouchDB {}", self.version))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self.call(Method::GET, "/_all_dbs", None).await {
            Ok(Value::Array(a)) => Ok(a
                .iter()
                .filter_map(Value::as_str)
                .filter(|n| !n.starts_with('_'))
                .map(str::to_string)
                .collect()),
            Ok(other) => Err(Error::Query(format!(
                "respuesta inesperada de /_all_dbs: {other}"
            ))),
            // Non-admins can't list databases (CouchDB 3): the session's one.
            Err(Error::Query(_) | Error::Statement(_) | Error::AuthFailed(_))
                if !self.db.is_empty() =>
            {
                Ok(vec![self.db.clone()])
            }
            Err(e) => Err(e),
        }
    }

    async fn row_estimates(&mut self) -> Result<Vec<dbine_driver::stats::RowEstimate>> {
        stats::row_estimates(self).await
    }

    /// CouchDB keeps no comments.
    async fn object_comments(&mut self) -> Result<Vec<dbine_driver::stats::ObjectComment>> {
        Ok(Vec::new())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let r = self
            .call(
                Method::GET,
                &format!("{}/_design_docs?include_docs=true", self.db_path()?),
                None,
            )
            .await?;
        let mut out = vec![DbObject {
            kind: kinds::COLLECTION.into(),
            schema: None,
            name: ALL_DOCS.into(),
            parent: None,
        }];
        let mut views = Vec::new();
        for row in r
            .get("rows")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let Some(doc) = row.get("doc") else { continue };
            let ddoc = doc
                .get("_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_start_matches("_design/");
            for v in doc
                .get("views")
                .and_then(Value::as_object)
                .map(|m| m.keys().collect::<Vec<_>>())
                .unwrap_or_default()
            {
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
            let (d, v) = obj
                .name
                .split_once('/')
                .ok_or_else(|| Error::Query(format!("vista inválida: {}", obj.name)))?;
            let doc = self
                .call(
                    Method::GET,
                    &format!("{}/_design/{}", self.db_path()?, seg(d)),
                    None,
                )
                .await?;
            let Some(view) = doc.get("views").and_then(|vs| vs.get(v)) else {
                return Ok(None);
            };
            let mut text = String::new();
            if let Some(lang) = doc.get("language").and_then(Value::as_str) {
                text.push_str(&format!("// language: {}\n", comment_text(lang)));
            }
            for part in ["map", "reduce"] {
                match view.get(part) {
                    Some(Value::String(src)) => text.push_str(&format!("// {part}\n{src}\n\n")),
                    Some(other) => text.push_str(&format!(
                        "// {part}\n{}\n\n",
                        serde_json::to_string_pretty(other)?
                    )),
                    None => {}
                }
            }
            if let Some(opts) = view.get("options") {
                text.push_str(&format!(
                    "// options\n{}\n",
                    serde_json::to_string_pretty(opts)?
                ));
            }
            return Ok(Some(text.trim_end().to_string()));
        }
        let path = self.db_path()?;
        let mut info = self.call(Method::GET, &path, None).await?;
        if let Ok(ix) = self
            .call(Method::GET, &format!("{path}/_index"), None)
            .await
        {
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
        let columns = infer_columns(
            &self
                .sample(&ObjectRef {
                    kind: kinds::COLLECTION.into(),
                    schema: None,
                    name: ALL_DOCS.into(),
                })
                .await?,
        )
        .into_iter()
        .map(|c| ColumnDef {
            name: c.name,
            data_type: c.data_type,
            nullable: c.nullable,
            ..Default::default()
        })
        .collect();
        let indexes = self
            .call(Method::GET, &format!("{path}/_index"), None)
            .await?;
        let ddocs = self
            .call(
                Method::GET,
                &format!("{path}/_design_docs?include_docs=true"),
                None,
            )
            .await?;
        Ok(vec![ddl::all_docs_schema(columns, &indexes, &ddocs)])
    }

    /// `PUT /{name}`.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("Conexión de solo lectura: no se puede crear una base.")?;
        ddl::check_database_name(name)?;
        self.call(Method::PUT, &format!("/{}", seg(name)), None)
            .await
            .map(|_| ())
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(
        &mut self,
        name: &str,
        options: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    async fn database_properties(
        &mut self,
        database: &str,
    ) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(
        &mut self,
        database: &str,
        changes: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    /// `DELETE /{name}`.
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("Conexión de solo lectura: no se puede borrar una base.")?;
        if name.is_empty() || name.starts_with('_') {
            return Err(Error::Query(format!("«{name}» no es una base de usuario")));
        }
        self.call(Method::DELETE, &format!("/{}", seg(name)), None)
            .await
            .map(|_| ())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        if obj.kind == kinds::VIEW {
            if let Some((d, v)) = obj.name.split_once('/') {
                return format!(
                    "GET /{}/_design/{}/_view/{}?limit={limit}",
                    seg(&self.db),
                    seg(d),
                    seg(v)
                );
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

    async fn privileges(&mut self) -> Result<Vec<dbine_driver::Privilege>> {
        Ok(security::privileges())
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

    /// Replications, from the replication scheduler (see [`jobs`]).
    async fn jobs(&mut self) -> Result<dbine_driver::JobList> {
        self.server_jobs().await
    }

    async fn job_detail(&mut self, id: &str) -> Result<dbine_driver::JobDetail> {
        self.server_job_detail(id).await
    }

    async fn job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<dbine_driver::JobRun>> {
        self.server_job_runs(id, limit).await
    }

    /// Restarts a `_replicator` replication by saving its document again.
    async fn run_job(&mut self, id: &str) -> Result<()> {
        self.run_server_job(id).await
    }

    /// Deletes the `_replicator` document, or cancels a `_replicate` one.
    async fn stop_job(&mut self, id: &str) -> Result<()> {
        self.stop_server_job(id).await
    }

    async fn job_script(&mut self, id: &str) -> Result<String> {
        self.server_job_script(id).await
    }

    /// Replications and the replicator's settings, managed whole (see
    /// [`replication`]).
    async fn scheduler_info(&mut self) -> Result<dbine_driver::SchedulerInfo> {
        replication::info(self).await
    }

    async fn scheduler_items(&mut self, kind: &str) -> Result<dbine_driver::SchedulerList> {
        replication::items(self, kind).await
    }

    async fn scheduler_form(
        &mut self,
        kind: &str,
        id: Option<&str>,
    ) -> Result<dbine_driver::ObjectForm> {
        replication::form(self, kind, id).await
    }

    async fn scheduler_script(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &dbine_driver::FormValues,
    ) -> Result<String> {
        replication::script(self, kind, id, values).await
    }

    async fn scheduler_save(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &dbine_driver::FormValues,
    ) -> Result<Option<String>> {
        replication::save(self, kind, id, values).await
    }

    async fn scheduler_delete_script(&mut self, kind: &str, id: &str) -> Result<String> {
        replication::delete_script(self, kind, id).await
    }

    async fn scheduler_delete(&mut self, kind: &str, id: &str) -> Result<()> {
        replication::delete(self, kind, id).await
    }

    /// A replication has one step and runs again whole with "run now":
    /// refused, as its job kind says (`run_from_step` false).
    async fn run_job_from(&mut self, id: &str, _step: &str) -> Result<()> {
        replication::run_from(self, id).await
    }

    async fn clear_job_history(&mut self, id: &str) -> Result<()> {
        replication::clear_history(self, id).await
    }

    async fn read_batches(
        &mut self,
        spec: &dbine_driver::ReadSpec,
        sink: dbine_driver::BatchSinkRef,
    ) -> Result<u64> {
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
            Error::from(
                dbine_driver::ScriptError::new(m)
                    .at_offset(at)
                    .at_line(steps::line_at(text, at)),
            )
        })?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let own = out.current_statement.is_none();
        for (i, l) in stmts.iter().enumerate() {
            let step = steps::Step::start(out, own, i, l.start, l.line);
            let r = match self.check_read_only(&l.stmt) {
                Ok(()) => self
                    .send(&l.stmt, max_rows, false)
                    .await
                    .map(|reply| push_reply(out, &reply, max_rows)),
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
    async fn explain(
        &mut self,
        text: &str,
        analyze: bool,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> Result<()> {
        let stmts = parse_script(text).map_err(Error::Query)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for stmt in stmts {
            let label = stmt_label(&stmt);
            let Some(explain_path) = self.explain_path(&stmt)? else {
                out.info(format!(
                    "`{label}`: solo las consultas Mango (_find) tienen plan de ejecución."
                ));
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
            out.plans
                .push(plan::from_explain(&label, &explain, stats.as_ref()));
        }
        Ok(())
    }

    /// `_session` roles and the database's `_security` (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    /// The Mango operators of the server's version (see [`functions`]).
    async fn function_catalog(&mut self) -> Result<Vec<dbine_driver::FunctionInfo>> {
        Ok(functions::catalog(Some(&self.version)))
    }

    /// Only `_all_docs` has indexes (views are indexes themselves).
    async fn index_usage(
        &mut self,
        table: &ObjectRef,
    ) -> Result<Option<dbine_driver::IndexUsageReport>> {
        if table.name != ALL_DOCS {
            return Ok(None);
        }
        self.index_usage_report().await
    }
}

impl CouchSession {
    /// `msg` is the whole refusal ("Conexión de solo lectura: no se puede …"),
    /// one message per action so it translates as a sentence.
    fn refuse_if_read_only(&self, msg: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(msg.into()));
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
        Ok(if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{path}", self.db_path()?)
        })
    }

    /// Where the statement's `_explain` goes, when it's a Mango query.
    fn explain_path(&self, stmt: &Stmt) -> Result<Option<String>> {
        Ok(match stmt {
            Stmt::Mango(_) => Some(format!("{}/_explain", self.db_path()?)),
            Stmt::Http { method, path, .. } if method == "POST" => {
                let full = self.full_path(path)?;
                let clean = full
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('/');
                clean
                    .strip_suffix("/_find")
                    .map(|p| format!("{p}/_explain"))
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
                self.call(
                    Method::POST,
                    &format!("{}/_find", self.db_path()?),
                    Some(&q),
                )
                .await
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
        let Some((db, name)) = index_shorthand(path) else {
            return Ok(None);
        };
        let name = percent_encoding::percent_decode_str(name)
            .decode_utf8_lossy()
            .to_string();
        let list = self
            .call(Method::GET, &format!("{db}/_index"), None)
            .await?;
        let found = list
            .get("indexes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|i| i.get("name").and_then(Value::as_str) == Some(name.as_str()));
        let Some(ix) = found else {
            return Err(Error::Query(format!(
                "No hay un índice Mango llamado «{name}»."
            )));
        };
        let ddoc = ix
            .get("ddoc")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches("_design/");
        if ddoc.is_empty() {
            return Err(Error::Query(format!(
                "«{name}» es el índice de _id de CouchDB y no se borra."
            )));
        }
        let kind = ix.get("type").and_then(Value::as_str).unwrap_or("json");
        Ok(Some(format!(
            "{db}/_index/{}/{}/{}",
            seg(ddoc),
            seg(kind),
            seg(&name)
        )))
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
        m.entry("limit")
            .or_insert_with(|| json!(max_rows.saturating_add(1)));
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
            out.begin_result(vec![ResultColumn {
                name: "result".into(),
                type_name: String::new(),
            }]);
            out.push_row(vec![cell(reply)], max_rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audit's scripts: two units each, where `execute` numbers its
    /// statements.
    #[test]
    fn split_script_gives_one_unit_per_statement() {
        let d = &drivers()[0];
        for s in [
            "GET /_all_dbs\nGET /db/_all_docs",
            "{\"selector\": {\"a\": 1}}\n{\"selector\": {\"b\": 2}}",
            "POST /db/_find\n{\"selector\": {\"x\": \"x;y\"}}\nGET /db",
            "GET /_all_dbs\r\n\r\n{\"selector\": {}}\r\n",
        ] {
            let units = d.split_script(s);
            let at: Vec<_> = units.iter().map(|u| (u.start, u.line)).collect();
            let numbered: Vec<_> = parse_located(s)
                .unwrap()
                .iter()
                .map(|l| (l.start, l.line))
                .collect();
            assert_eq!(units.len(), 2, "{s:?} {units:?}");
            assert_eq!(at, numbered);
        }
    }

    #[test]
    fn a_language_cannot_end_its_comment() {
        let c = format!(
            "// language: {}",
            comment_text("js\nDELETE /db\r\u{85}\u{2028}\u{2029}x")
        );
        assert_eq!(c, "// language: js?DELETE /db????x");
        assert_eq!(c.lines().count(), 1);
    }

    #[test]
    fn statements_keep_their_place() {
        let t = "{\"selector\": {}}\n// c\nGET /db/_all_docs\n  POST _find {\"selector\": {\"a\": 1}}\n{\"selector\": {}}";
        let v = parse_located(t).unwrap();
        let at: Vec<(usize, usize, u32)> = v.iter().map(|l| (l.start, l.end, l.line)).collect();
        assert_eq!(at, [(0, 16, 1), (22, 39, 3), (42, 75, 4), (76, 92, 5)]);
        let e = parse_located("GET /db\nPOST _find {nope").unwrap_err();
        assert_eq!(e.1, 8);
        // A loose value that isn't a query: there, not at its block.
        let e = parse_located("GET /db\n{\"selector\": {}}\n[1]").unwrap_err();
        assert_eq!(e.1, 25);
    }

    /// `(line, text)` of each unit, checking that the units are where
    /// `execute` numbers its statements and that each unit's text alone
    /// parses to that one statement (what "run the statement at the
    /// cursor" sends).
    fn units(s: &str) -> Vec<(u32, String)> {
        let units = split(s);
        let located = parse_located(s).unwrap();
        assert_eq!(units.len(), located.len(), "{units:?}");
        for (u, l) in units.iter().zip(&located) {
            assert_eq!((u.start, u.end, u.line), (l.start, l.end, l.line));
            assert_eq!(u.error, None);
            assert_eq!(&s[u.start..u.end], u.text);
            assert_eq!(parse_script(&u.text).unwrap(), vec![l.stmt.clone()]);
        }
        units.into_iter().map(|u| (u.line, u.text)).collect()
    }

    #[test]
    fn split_http_lines_and_mango_queries() {
        assert_eq!(
            units("GET /_all_dbs\nGET /db/_all_docs"),
            [(1, "GET /_all_dbs".into()), (2, "GET /db/_all_docs".into())]
        );
        // Two loose Mango queries, each with its own place.
        assert_eq!(
            units("{\"selector\": {\"a\": 1}}\n\n// next\n{\n  \"selector\": {}\n}\n"),
            [
                (1, "{\"selector\": {\"a\": 1}}".into()),
                (4, "{\n  \"selector\": {}\n}".into())
            ]
        );
        // A body holding separators and brace-like text in its strings.
        let u = units("POST /db/_find\n{\"selector\": {\"x\": \"x;y } GET /z\"}}\nGET /db");
        assert_eq!(u.len(), 2);
        assert_eq!(
            u[0].1,
            "POST /db/_find\n{\"selector\": {\"x\": \"x;y } GET /z\"}}"
        );
        assert_eq!(u[1], (3, "GET /db".into()));
    }

    #[test]
    fn split_three_with_comments_and_crlf() {
        let s = "# a\r\nPUT /db/doc1\r\n// body\r\n{\"a\": 1}\r\n\r\n{\"selector\": {}} {\"selector\": {\"a\": 1}}\r\nDELETE /db/doc1?rev=1-x\r\n";
        let u = units(s);
        assert_eq!(
            u,
            [
                (2, "PUT /db/doc1\r\n// body\r\n{\"a\": 1}".into()),
                (6, "{\"selector\": {}}".into()),
                (6, "{\"selector\": {\"a\": 1}}".into()),
                (7, "DELETE /db/doc1?rev=1-x".into()),
            ]
        );
        // A GET takes no body: the JSON after it is a query of its own.
        let u = units("GET /db {\"selector\": {}}");
        assert_eq!(u, [(1, "GET /db".into()), (1, "{\"selector\": {}}".into())]);
    }

    #[test]
    fn split_cuts_around_what_does_not_parse() {
        let s = "GET /db\n{nope\n\nPOST _find\n[1]\nGET /db/_all_docs\n";
        let u = split(s);
        let got: Vec<_> = u.iter().map(|u| (u.text.as_str(), u.line)).collect();
        assert_eq!(
            got,
            [
                ("GET /db", 1),
                ("{nope", 2),
                ("POST _find\n[1]", 4),
                ("GET /db/_all_docs", 6),
            ]
        );
        // No unit carries an error (the editor keeps every lens while a
        // query is being typed): running one reports it at its place.
        assert!(u.iter().all(|u| u.error.is_none()));
        let e = parse_located(s).unwrap_err();
        assert_eq!(e.1, u[1].start);
        assert_eq!(parse_located(&u[1].text).unwrap_err().1, 0);
        let typing = "GET /db\n{\"selector\": {\"a\": \nGET /db/_all_docs";
        let u = split(typing);
        assert_eq!(u.len(), 3);
        assert!(u.iter().all(|u| u.error.is_none()));
        assert!(parse_script(typing).is_err());
        assert!(split("// solo comentarios\n\n# nada\n").is_empty());
        // Whitespace JSON doesn't take is what fails, not past the end.
        let u = split("GET /db\n\u{a0}\n");
        assert_eq!((u[1].text.as_str(), u[1].line), ("\u{a0}", 2));
        assert!(parse_script(&u[1].text).is_err());
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
        assert_eq!(
            s[2],
            Stmt::Http {
                method: "GET".into(),
                path: "/db/_all_docs?include_docs=true".into(),
                body: None
            }
        );
        assert_eq!(
            s[3],
            Stmt::Http {
                method: "POST".into(),
                path: "_find".into(),
                body: Some(json!({ "selector": {} }))
            }
        );
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
        assert_eq!(
            documents(&json!(["db1", "db2"])).unwrap()[1],
            json!({ "value": "db2" })
        );
        assert!(documents(&json!({ "ok": true })).is_none());
    }

    #[test]
    fn flattening_and_inference() {
        let docs = vec![
            json!({ "n": 1, "_id": "a", "o": { "x": [1] } }),
            json!({ "_id": "b", "n": 2.5, "big": 9007199254740993_i64 }),
        ];
        let mut out = QueryOutcome::default();
        push_docs(&mut out, &docs, 10);
        let r = &out.results[0];
        assert_eq!(
            r.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["_id", "n", "o", "big"]
        );
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
        let mut c = ConnectionConfig {
            host: "db.local".into(),
            ..Default::default()
        };
        let url = |c: &ConnectionConfig| base_url(c, &http_tls(c).unwrap());
        assert_eq!(url(&c), "http://db.local:5984");
        c.encrypt = true;
        c.port = 6984;
        assert_eq!(url(&c), "https://db.local:6984");
        c.host = "https://x.example/couch/".into();
        assert_eq!(url(&c), "https://x.example/couch");
        assert_eq!(seg("a/b+c"), "a%2Fb%2Bc");
    }
}

#[cfg(test)]
mod tls_options_tests {
    use super::*;

    fn legacy(encrypt: bool, trust: bool) -> ConnectionConfig {
        ConnectionConfig {
            host: "db.example.com".into(),
            encrypt,
            trust_server_certificate: trust,
            ..Default::default()
        }
    }

    /// What the form saves for a new connection: every field's default.
    fn new_connection() -> ConnectionConfig {
        let mut c = legacy(false, false);
        for f in tls_fields() {
            if !f.default.is_empty() {
                c.options.insert(f.key.into(), f.default.into());
            }
        }
        c
    }

    fn level(c: &ConnectionConfig) -> TlsLevel {
        http_tls(c).unwrap().level
    }

    #[test]
    fn saved_connections_keep_their_tls_and_new_ones_verify() {
        // Today: ff and ft plain http, tf https verified, tt https without
        // verifying.
        assert_eq!(level(&legacy(false, false)), TlsLevel::Off);
        assert_eq!(level(&legacy(false, true)), TlsLevel::Off);
        assert_eq!(level(&legacy(true, false)), TlsLevel::VerifyFull);
        assert_eq!(level(&legacy(true, true)), TlsLevel::Unverified);
        // A new connection: TLS, verified.
        assert_eq!(level(&new_connection()), TlsLevel::VerifyFull);
        // Each one can be turned off.
        let mut plain = new_connection();
        plain.options.insert("scheme".into(), "http".into());
        assert_eq!(level(&plain), TlsLevel::Off);
        let mut skip = new_connection();
        skip.options.insert("insecure".into(), "true".into());
        assert_eq!(level(&skip), TlsLevel::Unverified);
        // No legacy field is left in the form.
        assert!(!tls_fields().iter().any(Field::is_legacy_tls));
    }

    #[test]
    fn an_https_url_in_the_host_keeps_trust_from_saved_connections() {
        // Today `trust` skips verification for an https URL typed in the
        // host even with `encrypt` off.
        let mut c = legacy(false, true);
        c.host = "https://db.example.com:443".into();
        assert_eq!(level(&c), TlsLevel::Unverified);
        c.trust_server_certificate = false;
        assert_eq!(level(&c), TlsLevel::VerifyFull);
        let mut n = new_connection();
        n.host = "http://db.example.com".into();
        assert_eq!(level(&n), TlsLevel::Off);
    }
}

//! OrientDB over its HTTP REST API (reqwest, basic auth).
//!
//! # Query language (`Language::Sql`, dialect `orientdb`)
//!
//! A script holds OrientDB SQL statements separated by `;` (`SELECT`,
//! `MATCH`, `TRAVERSE`, `INSERT`, `CREATE VERTEX` / `EDGE`, `UPDATE`,
//! `DELETE`, `CREATE CLASS` / `PROPERTY` / `INDEX`…), each sent to
//! `POST /command/{db}/sql`. A statement starting with `g.` is Gremlin and
//! goes to `/command/{db}/gremlin` (only servers with the TinkerPop plugin,
//! the `-tp3` images, have it).
//!
//! # Results
//!
//! One result set per statement; records flattened: `@rid` and `@class`
//! first, then the fields in the order the server wrote them (the union
//! over the rows); `@type`, `@version` and `@fieldTypes` are dropped;
//! embedded documents, lists and links go as compact JSON / `#c:p` text.
//! `UPDATE` / `DELETE` report `count` as affected rows.
//!
//! # Read-only and cancel
//!
//! Read-only connections send statements through `GET /query/…`, where
//! the server itself refuses anything that isn't idempotent; Gremlin
//! scripts with mutating steps are refused before sending. The
//! interrupter finds the server connection running the session's statement
//! (`GET /server`) and calls `POST /connection/interrupt/{id}`.

mod create_db;
mod ddl;
mod index_usage;
mod sync;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod properties;
mod security;
mod steps;
mod transfer;

use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, IndexDef, Language, MonitorSnapshot, ObjectKindInfo,
    ObjectRef, QueryOutcome, Result, ResultColumn, ScriptError, Session, TableSchema,
};
use dbine_driver::{json_f64, json_i64, json_u64};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Explorer kinds besides the standard ones.
pub const VERTEX: &str = "vertex";
pub const EDGE: &str = "edge";

pub const QUERY_HELP: &str = "SQL de OrientDB, con las sentencias separadas por «;»:\n\
SELECT FROM Persona WHERE edad > 30 LIMIT 20\n\
MATCH {class: Persona, as: p}-Conoce->{as: q} RETURN p.nombre, q.nombre\n\
TRAVERSE out() FROM #12:0 MAXDEPTH 3\n\
CREATE VERTEX Persona SET nombre = 'Ana' · CREATE EDGE Conoce FROM #12:0 TO #12:1\n\
INSERT INTO Clase CONTENT {\"a\": 1} · UPDATE … · DELETE VERTEX …\n\
EXPLAIN / PROFILE delante de una consulta muestran su plan.\n\
Una sentencia que empieza con g. es Gremlin (solo en servidores con TinkerPop).";

/// How the console reads a script: `;` ends a statement, strings take
/// backslash escapes, and there are no `BEGIN … END` bodies.
fn dialect() -> dbine_driver::ScriptDialect {
    dbine_driver::ScriptDialect { backslash_escapes: true, compound_blocks: false, ..dbine_driver::ScriptDialect::generic() }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(OrientDriver)]
}

pub struct OrientDriver;

fn info() -> &'static DriverInfo {
    static INFO: OnceLock<DriverInfo> = OnceLock::new();
    INFO.get_or_init(|| DriverInfo {
        id: "orientdb",
        name: "OrientDB",
        family: Family::Graph,
        language: Language::Sql,
        dialect: "orientdb",
        default_port: 2480,
        fields: vec![
            Field::host().help("Un nombre de host o una URL completa (https://servidor:2480)."),
            Field::port(),
            Field::database(),
            Field::username().default_value("root"),
            Field::password(),
            Field::encrypt().help("Usar https."),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(VERTEX, "Vértices", true, true, true),
            ObjectKindInfo::new(EDGE, "Aristas", true, true, true),
            ObjectKindInfo::new(kinds::TABLE, "Clases de documentos", true, true, true),
            ObjectKindInfo::new(kinds::INDEX, "Índices", false, false, true),
            ObjectKindInfo::functions(),
            ObjectKindInfo::sequences(),
        ],
    })
}

/// `http(s)://host:port`, without a trailing slash.
pub fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.trim_end_matches('/').to_string();
    }
    let scheme = if cfg.encrypt { "https" } else { "http" };
    format!("{scheme}://{host}:{}", cfg.port_or(2480))
}

fn seg(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

#[async_trait]
impl Driver for OrientDriver {
    fn info(&self) -> &DriverInfo {
        info()
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// The class's indexes and LINKs, without counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    /// One command per HTTP request (`/command`), so the app runs the
    /// script statement by statement; as the console (`ignoreErrors`
    /// off), a failure stops it unless the tab says otherwise.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        dialect()
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields()
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(name, options)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            processes: true,
            cancel_query: true,
            kill_session: true,
            database_properties: true,
            ..Default::default()
        }
    }

    /// `ALTER DATABASE …` statements (see [`properties`]).
    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(database, changes)
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

    /// SQL scripts over `/batch`, one transaction per chunk (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    /// The database's OUser / ORole records and their rules.
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
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
        let db = database.filter(|d| !d.is_empty()).unwrap_or(cfg.database.trim()).to_string();
        let auth = cfg.username.clone().filter(|u| !u.is_empty()).map(|u| (u, cfg.password.clone()));
        let mut s = OrientSession {
            http,
            base: base_url(cfg),
            db,
            auth,
            read_only: cfg.read_only,
            current: Arc::new(Mutex::new(String::new())),
        };
        if s.db.is_empty() {
            s.db = s.list_databases().await.map_err(to_connect)?.into_iter().next().unwrap_or_default();
        } else {
            s.call(Method::GET, &format!("/database/{}", seg(&s.db)), None).await.map_err(to_connect)?;
        }
        Ok(Box::new(s))
    }
}

fn to_connect(e: Error) -> Error {
    match e {
        Error::Query(m) => Error::Connect(m),
        e => e,
    }
}

pub struct OrientSession {
    http: reqwest::Client,
    base: String,
    db: String,
    auth: Option<(String, Option<String>)>,
    read_only: bool,
    /// The statement in flight, for the interrupter.
    current: Arc<Mutex<String>>,
}

/// The message of an error reply (`{"errors": [{"content": …}]}`).
fn error_text(status: StatusCode, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let content = v.get("errors").and_then(|e| e.get(0)).and_then(|e| e.get("content")).and_then(Value::as_str);
    let text = content.map(str::to_string).unwrap_or_else(|| if body.trim().is_empty() { format!("HTTP {status}") } else { body.trim().to_string() });
    // Strip the Java class prefix and the trailing `DB name="…"`.
    let text = text.split("\tDB name=").next().unwrap_or(&text).trim().to_string();
    match text.split_once(": ") {
        Some((class, rest)) if class.contains("Exception") && !class.contains(' ') => rest.to_string(),
        _ => text,
    }
}

/// The engine's code of an error reply: the Java exception's class
/// (`OCommandSQLParsingException`), else the reply's `errors[0].code`.
fn error_code(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let e = v.get("errors")?.get(0)?;
    let content = e.get("content").and_then(Value::as_str).unwrap_or_default();
    let class = content.split_once(':').map(|(c, _)| c.trim()).filter(|c| c.ends_with("Exception") && !c.contains(char::is_whitespace));
    match class {
        Some(c) => Some(c.rsplit('.').next().unwrap_or(c).to_string()),
        None => e.get("code").filter(|c| !c.is_null()).map(|c| c.as_str().map_or_else(|| c.to_string(), str::to_string)),
    }
}

/// [`request_coded`] with plain errors, for the internal requests that
/// read the message.
async fn request(
    http: &reqwest::Client,
    base: &str,
    auth: &Option<(String, Option<String>)>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<String> {
    request_coded(http, base, auth, method, path, body).await.map_err(|e| match e {
        Error::Statement(se) => Error::Query(se.message),
        e => e,
    })
}

/// A request whose failure carries the engine's code (editor statements).
async fn request_coded(
    http: &reqwest::Client,
    base: &str,
    auth: &Option<(String, Option<String>)>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<String> {
    let mut rq = http.request(method, format!("{base}{path}")).header("Accept", "application/json");
    if let Some((u, p)) = auth {
        rq = rq.basic_auth(u, p.as_deref());
    }
    if let Some(b) = body {
        rq = rq.json(b);
    }
    let resp = rq.send().await.map_err(|e| Error::Connect(format!("no se pudo llegar a OrientDB: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
    if status.is_success() {
        return Ok(text);
    }
    let msg = error_text(status, &text);
    Err(if status == StatusCode::UNAUTHORIZED {
        Error::AuthFailed(msg)
    } else {
        match error_code(&text) {
            Some(code) => Error::Statement(Box::new(ScriptError::new(msg).with_code(code))),
            None => Error::Query(msg),
        }
    })
}

impl OrientSession {
    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let t = request(&self.http, &self.base, &self.auth, method, path, body).await?;
        if t.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&t).map_err(|e| Error::Query(format!("respuesta inesperada: {e}")))
    }

    fn db_seg(&self) -> Result<String> {
        if self.db.is_empty() {
            return Err(Error::Query("Elegí una base de datos.".into()));
        }
        Ok(seg(&self.db))
    }

    /// Run one statement; `limit` rows at most (-1 = all). Returns the rows
    /// with their keys in server order, plus the raw reply (for plans).
    /// One editor statement into `out`.
    async fn statement(&self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.read_only && is_gremlin(stmt) && gremlin_writes(stmt) {
            return Err(Error::Query("Conexión de solo lectura: el script Gremlin tiene pasos que escriben.".into()));
        }
        let limit = if is_read(strip_plan_prefix(stmt)) { max_rows as i64 + 1 } else { -1 };
        let rows = self.command(stmt, limit).await?;
        // EXPLAIN / PROFILE typed in the editor: the plan too.
        if let Some(p) = rows.records.first().and_then(|r| r.iter().find(|(k, _)| k == "executionPlan")) {
            let actual = stmt.trim_start().to_ascii_uppercase().starts_with("PROFILE");
            out.plans.push(plan::from_execution_plan(strip_plan_prefix(stmt), &p.1, actual));
        }
        push_rows(out, &rows, max_rows);
        Ok(())
    }

    async fn command(&self, stmt: &str, limit: i64) -> Result<Rows> {
        let db = self.db_seg()?;
        let gremlin = is_gremlin(stmt);
        let lang = if gremlin { "gremlin" } else { "sql" };
        *self.current.lock().expect("current") = stmt.to_string();
        let r = if self.read_only && !gremlin {
            // The server refuses non-idempotent statements here.
            let path = format!("/query/{db}/sql/{}/{limit}", seg(stmt));
            request_coded(&self.http, &self.base, &self.auth, Method::GET, &path, None).await
        } else {
            let path = format!("/command/{db}/{lang}/-/{limit}");
            request_coded(&self.http, &self.base, &self.auth, Method::POST, &path, Some(&json!({ "command": stmt }))).await
        };
        self.current.lock().expect("current").clear();
        let text = r.map_err(|e| {
            let m = e.to_string();
            if e.is_query() && m.contains("Cannot execute query on non idempotent") {
                Error::Query("Conexión de solo lectura: el servidor rechazó una sentencia que escribe.".into())
            } else if e.is_query() && gremlin && m.contains("script executor") {
                Error::Query(format!(
                    "Este servidor no tiene Gremlin (hace falta OrientDB con el plugin TinkerPop, p. ej. la imagen orientdb:3.x-tp3): {m}"
                ))
            } else {
                e
            }
        })?;
        parse_reply(&text).map_err(Error::Query)
    }

    async fn metadata(&self) -> Result<Value> {
        self.call(Method::GET, &format!("/database/{}", self.db_seg()?), None).await
    }

    /// User classes: (name, kind, class JSON).
    async fn classes(&self) -> Result<Vec<(String, &'static str, Value)>> {
        let meta = self.metadata().await?;
        let all: Vec<Value> = meta.get("classes").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(classify(&all))
    }

    async fn sample(&self, class: &str) -> Result<Vec<Vec<(String, Value)>>> {
        Ok(self.command(&format!("SELECT FROM {} LIMIT 100", ident(class)), 100).await?.records)
    }

    async fn columns_of(&self, class: &str, meta: &Value) -> Result<Vec<ColumnInfo>> {
        let mut cols: Vec<ColumnInfo> = meta
            .get("properties")
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .map(|p| ColumnInfo {
                        name: p.get("name").map(as_text).unwrap_or_default(),
                        data_type: p.get("type").map(as_text).unwrap_or_default(),
                        nullable: !p.get("notNull").and_then(Value::as_bool).unwrap_or(false)
                            && !p.get("mandatory").and_then(Value::as_bool).unwrap_or(false),
                        primary_key: false,
                        auto_increment: false,
                        default_value: p.get("defaultValue").filter(|v| !v.is_null()).map(as_text),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Inherited properties (V / E have none, other superclasses may).
        // Schemaless fields from a sample.
        let sample = self.sample(class).await?;
        for c in infer_columns(&sample) {
            if !cols.iter().any(|x| x.name == c.name) && !is_graph_field(&c.name) {
                cols.push(c);
            }
        }
        Ok(cols)
    }
}

/// Graph bookkeeping fields of vertices / edges (`out_Knows`, `in_`, `out`, `in`).
fn is_graph_field(name: &str) -> bool {
    name.starts_with("out_") || name.starts_with("in_") || name == "out" || name == "in"
}

pub fn is_gremlin(stmt: &str) -> bool {
    let t = stmt.trim_start();
    t.starts_with("g.") || t.starts_with("g\n.")
}

/// Mutating Gremlin steps, for read-only connections.
pub fn gremlin_writes(stmt: &str) -> bool {
    ["addV(", "addE(", ".drop(", ".property(", "addVertex(", "addEdge(", ".remove("].iter().any(|s| stmt.contains(s))
}

/// Parsed `{"result": [...]}` with each record's keys in server order.
#[derive(Debug, Default)]
pub struct Rows {
    pub records: Vec<Vec<(String, Value)>>,
}

pub fn parse_reply(text: &str) -> std::result::Result<Rows, String> {
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
    struct Top;
    struct List;
    struct Rec;
    type Recs = Vec<Vec<(String, Value)>>;
    impl<'de> Visitor<'de> for Top {
        type Value = Recs;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("{result: […]}")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<Recs, A::Error> {
            let mut out = Vec::new();
            while let Some(k) = m.next_key::<String>()? {
                if k == "result" {
                    out = m.next_value_seed(List)?;
                } else {
                    m.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(out)
        }
    }
    impl<'de> DeserializeSeed<'de> for List {
        type Value = Recs;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Recs, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for List {
        type Value = Recs;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("una lista de registros")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> std::result::Result<Recs, A::Error> {
            let mut out = Vec::new();
            while let Some(r) = s.next_element_seed(Rec)? {
                out.push(r);
            }
            Ok(out)
        }
        // Gremlin scalars: `{"result": 3}`.
        fn visit_i64<E>(self, v: i64) -> std::result::Result<Recs, E> {
            Ok(vec![vec![("result".into(), json!(v))]])
        }
        fn visit_u64<E>(self, v: u64) -> std::result::Result<Recs, E> {
            Ok(vec![vec![("result".into(), json!(v))]])
        }
        fn visit_f64<E>(self, v: f64) -> std::result::Result<Recs, E> {
            Ok(vec![vec![("result".into(), json!(v))]])
        }
        fn visit_str<E>(self, v: &str) -> std::result::Result<Recs, E> {
            Ok(vec![vec![("result".into(), json!(v))]])
        }
        fn visit_bool<E>(self, v: bool) -> std::result::Result<Recs, E> {
            Ok(vec![vec![("result".into(), json!(v))]])
        }
        fn visit_unit<E>(self) -> std::result::Result<Recs, E> {
            Ok(Vec::new())
        }
    }
    impl<'de> DeserializeSeed<'de> for Rec {
        type Value = Vec<(String, Value)>;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Rec {
        type Value = Vec<(String, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("un registro")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, Value>()? {
                out.push((k, v));
            }
            Ok(out)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> std::result::Result<Self::Value, A::Error> {
            let mut a = Vec::new();
            while let Some(v) = s.next_element::<Value>()? {
                a.push(v);
            }
            Ok(vec![("value".into(), Value::Array(a))])
        }
        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), json!(v))])
        }
        fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), json!(v))])
        }
        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), json!(v))])
        }
        fn visit_f64<E>(self, v: f64) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), json!(v))])
        }
        fn visit_bool<E>(self, v: bool) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), json!(v))])
        }
        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(vec![("value".into(), Value::Null)])
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let records = de.deserialize_map(Top).map_err(|e| format!("respuesta inesperada de OrientDB: {e}"))?;
    Ok(Rows { records })
}

/// Metadata fields left out of the grid.
const HIDDEN: &[&str] = &["@type", "@version", "@fieldTypes"];

/// Columns: `@rid`, `@class`, then the union of fields in order of appearance.
pub fn union_keys(records: &[Vec<(String, Value)>]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for r in records {
        for (k, _) in r {
            if !HIDDEN.contains(&k.as_str()) && !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    for (i, first) in ["@rid", "@class"].iter().enumerate() {
        if let Some(p) = keys.iter().position(|k| k == first) {
            let k = keys.remove(p);
            keys.insert(i.min(keys.len()), k);
        }
    }
    keys
}

pub fn cell(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let clean: serde_json::Map<String, Value> =
                o.iter().filter(|(k, _)| !HIDDEN.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
            Value::String(Value::Object(clean).to_string())
        }
        Value::Array(_) => Value::String(v.to_string()),
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

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A class or property name, between backticks when it isn't plain.
pub fn ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "\\`"))
    }
}

/// System classes of every database.
const SYSTEM_CLASSES: &[&str] = &["OUser", "ORole", "OIdentity", "OFunction", "OSequence", "OSchedule", "OTriggered", "ORestricted", "OShape", "OSecurityPolicy", "OGeometryCollection"];

fn is_system(name: &str) -> bool {
    SYSTEM_CLASSES.contains(&name) || is_spatial(name)
}

fn is_spatial(name: &str) -> bool {
    ["OPoint", "OMultiPoint", "OLineString", "OMultiLineString", "OPolygon", "OMultiPolygon", "ORectangle"].contains(&name)
}

/// Vertex / edge / document classes (system ones left out). `V` and `E`
/// themselves are listed: records can live right in them.
pub fn classify(all: &[Value]) -> Vec<(String, &'static str, Value)> {
    let supers = |c: &Value| -> Vec<String> {
        let mut s: Vec<String> = c.get("superClasses").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default();
        if let Some(x) = c.get("superClass").map(as_text).filter(|x| !x.is_empty()) {
            if !s.contains(&x) {
                s.push(x);
            }
        }
        s
    };
    let by_name = |n: &str| all.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(n));
    let kind_of = |name: &str| -> &'static str {
        let mut stack = vec![name.to_string()];
        let mut seen = Vec::new();
        while let Some(n) = stack.pop() {
            if n == "V" {
                return VERTEX;
            }
            if n == "E" {
                return EDGE;
            }
            if seen.contains(&n) {
                continue;
            }
            if let Some(c) = by_name(&n) {
                stack.extend(supers(c));
            }
            seen.push(n);
        }
        kinds::TABLE
    };
    let mut out: Vec<(String, &'static str, Value)> = all
        .iter()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?.to_string();
            (!is_system(&name)).then(|| (name.clone(), kind_of(&name), c.clone()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn infer_columns(records: &[Vec<(String, Value)>]) -> Vec<ColumnInfo> {
    let keys: Vec<String> = union_keys(records).into_iter().filter(|k| !k.starts_with('@')).collect();
    keys.into_iter()
        .map(|name| {
            let mut types: Vec<(&'static str, usize)> = Vec::new();
            let mut present = 0;
            for v in records.iter().filter_map(|r| r.iter().find(|(k, _)| *k == name).map(|x| &x.1)).filter(|v| !v.is_null()) {
                present += 1;
                let t = match v {
                    Value::Bool(_) => "BOOLEAN",
                    Value::Number(n) if n.is_f64() => "DOUBLE",
                    Value::Number(_) => "LONG",
                    Value::String(s) if s.starts_with('#') && s[1..].contains(':') => "LINK",
                    Value::String(_) => "STRING",
                    Value::Array(_) => "EMBEDDEDLIST",
                    Value::Object(_) => "EMBEDDED",
                    Value::Null => "ANY",
                };
                match types.iter_mut().find(|(n, _)| *n == t) {
                    Some(e) => e.1 += 1,
                    None => types.push((t, 1)),
                }
            }
            types.sort_by(|a, b| b.1.cmp(&a.1));
            ColumnInfo {
                data_type: if types.is_empty() { "ANY".into() } else { types.iter().map(|t| t.0).collect::<Vec<_>>().join("|") },
                nullable: present < records.len(),
                primary_key: false,
                auto_increment: false,
                default_value: None,
                name,
            }
        })
        .collect()
}

fn push_rows(out: &mut QueryOutcome, rows: &Rows, max_rows: usize) {
    // `UPDATE` / `DELETE` answer `[{"count": n}]`.
    if let [r] = rows.records.as_slice() {
        if let [(k, Value::Number(n))] = r.as_slice() {
            if k == "count" {
                out.push_affected(n.as_u64().unwrap_or(0));
                return;
            }
        }
    }
    let keys = union_keys(&rows.records);
    out.begin_result(keys.iter().map(|k| ResultColumn { name: k.clone(), type_name: String::new() }).collect());
    for r in &rows.records {
        out.push_row(keys.iter().map(|k| r.iter().find(|(x, _)| x == k).map_or(Value::Null, |x| cell(&x.1))).collect(), max_rows);
    }
}

/// `EXPLAIN` / `PROFILE` prefix the user wrote.
fn strip_plan_prefix(stmt: &str) -> &str {
    let t = stmt.trim_start();
    for p in ["EXPLAIN", "PROFILE"] {
        if t.len() > p.len() && t[..p.len()].eq_ignore_ascii_case(p) && t[p.len()..].starts_with(char::is_whitespace) {
            return t[p.len()..].trim_start();
        }
    }
    t
}

fn is_read(stmt: &str) -> bool {
    let w: String = stmt.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    matches!(w.as_str(), "SELECT" | "MATCH" | "TRAVERSE")
}

#[async_trait]
impl Session for OrientSession {
    async fn server_version(&mut self) -> Result<String> {
        let meta = self.metadata().await?;
        let v = meta.get("server").and_then(|s| s.get("version")).map(as_text).unwrap_or_default();
        Ok(format!("OrientDB {v}").trim().to_string())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let v = self.call(Method::GET, "/listDatabases", None).await?;
        let mut dbs: Vec<String> = v.get("databases").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default();
        dbs.sort();
        if dbs.is_empty() && !self.db.is_empty() {
            dbs.push(self.db.clone());
        }
        Ok(dbs)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let meta = self.metadata().await?;
        let classes = classify(meta.get("classes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
        let mut out: Vec<DbObject> =
            classes.iter().map(|(n, k, _)| DbObject { kind: k.to_string(), schema: None, name: n.clone(), parent: None }).collect();
        let class_names: Vec<&str> = classes.iter().map(|c| c.0.as_str()).collect();
        for ix in meta.get("indexes").and_then(Value::as_array).cloned().unwrap_or_default() {
            let name = ix.get("name").map(as_text).unwrap_or_default();
            let class = ix.pointer("/configuration/indexDefinition/className").map(as_text);
            // Indexes of the user's classes, and manual ones (no class).
            if class.as_deref().is_none_or(|c| class_names.contains(&c)) {
                out.push(DbObject { kind: kinds::INDEX.into(), schema: None, name, parent: class });
            }
        }
        if let Ok(r) = self.command("SELECT name FROM OFunction ORDER BY name", -1).await {
            out.extend(r.records.iter().filter_map(|r| r.iter().find(|(k, _)| k == "name")).map(|(_, v)| DbObject {
                kind: kinds::FUNCTION.into(),
                schema: None,
                name: as_text(v),
                parent: None,
            }));
        }
        if let Ok(r) = self.command("SELECT name FROM OSequence ORDER BY name", -1).await {
            out.extend(r.records.iter().filter_map(|r| r.iter().find(|(k, _)| k == "name")).map(|(_, v)| DbObject {
                kind: kinds::SEQUENCE.into(),
                schema: None,
                name: as_text(v),
                parent: None,
            }));
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if ![VERTEX, EDGE, kinds::TABLE].contains(&obj.kind.as_str()) {
            return Ok(Vec::new());
        }
        let classes = self.classes().await?;
        let Some((_, _, meta)) = classes.iter().find(|c| c.0 == obj.name) else {
            return Err(Error::Query(format!("no existe la clase {}", obj.name)));
        };
        self.columns_of(&obj.name, meta).await
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        match obj.kind.as_str() {
            k if k == VERTEX || k == EDGE || k == kinds::TABLE => {
                let t = self.database_schema().await?.into_iter().find(|t| t.name == obj.name);
                Ok(match t {
                    Some(t) => Some(ddl::table_ddl(&t, DdlParts { create: true, indexes: true, ..Default::default() })?),
                    None => None,
                })
            }
            k if k == kinds::INDEX => {
                let meta = self.metadata().await?;
                let ix = meta.get("indexes").and_then(Value::as_array).and_then(|a| {
                    a.iter().find(|i| i.get("name").and_then(Value::as_str) == Some(obj.name.as_str())).cloned()
                });
                Ok(ix.map(|i| ddl::index_statement(&i)))
            }
            k if k == kinds::FUNCTION => {
                let r = self
                    .command(&format!("SELECT name, code, language, parameters, idempotent FROM OFunction WHERE name = {}", ddl::string(&obj.name)), 1)
                    .await?;
                Ok(r.records.first().map(ddl::function_statement))
            }
            k if k == kinds::SEQUENCE => {
                let r = self
                    .command(&format!("SELECT name, type, value, incr, start, cacheSize FROM OSequence WHERE name = {}", ddl::string(&obj.name)), 1)
                    .await?;
                Ok(r.records.first().map(ddl::sequence_statement))
            }
            _ => Ok(None),
        }
    }

    /// Classes with their properties, indexes and links (LINK properties
    /// with a linked class become foreign keys for the ER diagram; edge
    /// classes with typed `out` / `in` too).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let meta = self.metadata().await?;
        let classes = classify(meta.get("classes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
        let indexes: Vec<Value> = meta.get("indexes").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for (name, kind, c) in &classes {
            let cols = self.columns_of(name, c).await?;
            let mut t = TableSchema {
                kind: kind.to_string(),
                name: name.clone(),
                columns: cols
                    .into_iter()
                    .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, default_value: c.default_value, ..Default::default() })
                    .collect(),
                ..Default::default()
            };
            let sup: Vec<String> = c.get("superClasses").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default();
            if !sup.is_empty() {
                t.options.insert("extends".into(), sup.join(", "));
            }
            if c.get("abstract").and_then(Value::as_bool) == Some(true) {
                t.options.insert("abstract".into(), "true".into());
            }
            // Fields only seen in the data: shown, but no CREATE PROPERTY for them.
            let declared: Vec<String> =
                c.get("properties").and_then(Value::as_array).map(|a| a.iter().filter_map(|p| p.get("name").map(as_text)).collect()).unwrap_or_default();
            for col in t.columns.iter_mut().filter(|col| !declared.contains(&col.name)) {
                col.options.insert("inferred".into(), "true".into());
            }
            for p in c.get("properties").and_then(Value::as_array).cloned().unwrap_or_default() {
                let pname = p.get("name").map(as_text).unwrap_or_default();
                if let Some(col) = t.columns.iter_mut().find(|x| x.name == pname) {
                    if let Some(lc) = p.get("linkedClass").and_then(Value::as_str) {
                        col.options.insert("linked".into(), lc.to_string());
                        if p.get("type").and_then(Value::as_str).is_some_and(|ty| ty.starts_with("LINK")) {
                            t.foreign_keys.push(dbine_driver::ForeignKeyDef {
                                name: None,
                                columns: vec![pname.clone()],
                                ref_schema: None,
                                ref_table: lc.to_string(),
                                ref_columns: vec!["@rid".into()],
                                on_delete: None,
                                on_update: None,
                            });
                        }
                    }
                    for (k, o) in [("mandatory", "mandatory"), ("readonly", "readonly"), ("min", "min"), ("max", "max"), ("regexp", "regexp")] {
                        if let Some(v) = p.get(k).filter(|v| !v.is_null() && **v != Value::Bool(false)) {
                            col.options.insert(o.into(), as_text(v));
                        }
                    }
                }
            }
            for ix in &indexes {
                if ix.pointer("/configuration/indexDefinition/className").and_then(Value::as_str) != Some(name.as_str()) {
                    continue;
                }
                let (fields, options) = ddl::index_parts(ix);
                let ty = ix.pointer("/configuration/type").map(as_text).unwrap_or_default();
                t.indexes.push(IndexDef {
                    name: ix.get("name").map(as_text).unwrap_or_default(),
                    columns: fields,
                    unique: ty.starts_with("UNIQUE"),
                    kind: Some(ty),
                    filter: None,
                    options,
                    ..Default::default()
                });
            }
            out.push(t);
        }
        Ok(out)
    }

    /// `POST /database/{name}/plocal/graph` (see [`create_db`]).
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.create_database_with_impl(name, &std::collections::BTreeMap::new()).await
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
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se puede borrar una base.".into()));
        }
        self.call(Method::DELETE, &format!("/database/{}", seg(name.trim())), None).await.map(|_| ())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT FROM {} LIMIT {limit}", ident(&obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let units: Vec<_> = dbine_driver::sql::split_script(text, &dialect())
            .into_iter()
            .filter(|u| u.kind != dbine_driver::StatementKind::ClientCommand)
            .collect();
        if units.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let own = out.current_statement.is_none();
        for (i, u) in units.iter().enumerate() {
            let step = steps::Step::start(out, own, i, u.start, u.line);
            let r = self.statement(&u.text, max_rows, out).await;
            step.end(out, r)?;
        }
        Ok(())
    }

    /// `EXPLAIN` gives the estimated plan; with `analyze`, reads run under
    /// `PROFILE` (actual cost per step) and then once more for their rows;
    /// writes get the estimated plan and run once.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = dbine_driver::sql::split_statements(text);
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for stmt in stmts {
            let body = strip_plan_prefix(&stmt).to_string();
            if is_gremlin(&body) {
                out.info("Gremlin no tiene plan de ejecución en OrientDB.");
                if analyze {
                    self.execute(&body, max_rows, out).await?;
                }
                continue;
            }
            let profile = analyze && is_read(&body);
            let q = format!("{} {body}", if profile { "PROFILE" } else { "EXPLAIN" });
            match self.command(&q, -1).await {
                Ok(rows) => match rows.records.first().and_then(|r| r.iter().find(|(k, _)| k == "executionPlan")) {
                    Some(p) => out.plans.push(plan::from_execution_plan(&body, &p.1, profile)),
                    None => out.info(format!("`{body}`: el servidor no devolvió un plan.")),
                },
                Err(e) if e.is_query() => out.info(format!("`{body}`: sin plan de ejecución ({e}).")),
                Err(e) => return Err(e),
            }
            if analyze {
                if !profile {
                    out.info(format!("`{body}` no es una consulta: se muestra el plan estimado y se ejecutó una sola vez."));
                }
                self.execute(&body, max_rows, out).await?;
            }
        }
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let (http, base, auth, db, current) = (self.http.clone(), self.base.clone(), self.auth.clone(), self.db.clone(), self.current.clone());
        Some(Arc::new(move || {
            let q = current.lock().expect("current").clone();
            if q.is_empty() {
                return;
            }
            let (http, base, auth, db) = (http.clone(), base.clone(), auth.clone(), db.clone());
            handle.spawn(async move {
                if let Err(e) = interrupt(&http, &base, &auth, &db, &q).await {
                    tracing::warn!("no se pudo cancelar la consulta: {e}");
                }
            });
        }))
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        monitor::snapshot(self).await
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        OrientSession::processes(self).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.end_connection(id, "interrupt").await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        self.end_connection(id, "kill").await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, columns, source, progress).await
    }

    /// Server user or all-powerful database role (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let meta = self.metadata().await?;
        Ok(index_usage::report(&meta, &table.name))
    }
}

/// Interrupt the server connections running `query` on `db`.
async fn interrupt(http: &reqwest::Client, base: &str, auth: &Option<(String, Option<String>)>, db: &str, query: &str) -> Result<()> {
    let server: Value = serde_json::from_str(&request(http, base, auth, Method::GET, "/server", None).await?)?;
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let q = norm(query);
    for c in server.get("connections").and_then(Value::as_array).cloned().unwrap_or_default() {
        let detail = norm(&c.get("commandDetail").map(as_text).unwrap_or_default());
        let same_db = c.get("db").map(as_text).is_none_or(|d| d == db || d == "-");
        if same_db && !q.is_empty() && (detail.contains(&q) || (detail.len() > 20 && q.contains(&detail))) {
            if let Some(id) = c.get("connectionId").map(as_text) {
                let _ = request(http, base, auth, Method::POST, &format!("/connection/interrupt/{}", seg(&id)), None).await;
            }
        }
    }
    Ok(())
}

impl OrientSession {
    pub(crate) async fn server(&self) -> Result<Value> {
        self.call(Method::GET, "/server", None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_keep_field_order() {
        let r = parse_reply(r##"{"result":[{"name":"Ann","age":3,"@rid":"#1:0","@class":"P","@version":1},{"x":1}],"dbStats":{}}"##).unwrap();
        assert_eq!(union_keys(&r.records), ["@rid", "@class", "name", "age", "x"]);
        let mut out = QueryOutcome::default();
        push_rows(&mut out, &r, 10);
        assert_eq!(out.results[0].rows[0], vec![json!("#1:0"), json!("P"), json!("Ann"), json!(3), Value::Null]);
        let r = parse_reply(r#"{"result":[{"count":4}]}"#).unwrap();
        let mut out = QueryOutcome::default();
        push_rows(&mut out, &r, 10);
        assert_eq!(out.results[0].rows_affected, Some(4));
        assert_eq!(parse_reply(r#"{"result":7}"#).unwrap().records, vec![vec![("result".to_string(), json!(7))]]);
        assert_eq!(cell(&json!({ "@type": "d", "a": 1 })), json!("{\"a\":1}"));
    }

    #[test]
    fn classes_by_kind() {
        let all = vec![
            json!({ "name": "V", "superClass": "" }),
            json!({ "name": "E", "superClass": "" }),
            json!({ "name": "OUser", "superClass": "OIdentity" }),
            json!({ "name": "Person", "superClass": "V", "superClasses": ["V"] }),
            json!({ "name": "Dev", "superClass": "Person", "superClasses": ["Person"] }),
            json!({ "name": "Knows", "superClass": "E" }),
            json!({ "name": "Doc", "superClass": "" }),
        ];
        let c: Vec<(String, &str)> = classify(&all).into_iter().map(|(n, k, _)| (n, k)).collect();
        assert_eq!(
            c,
            [("Dev".into(), VERTEX), ("Doc".into(), kinds::TABLE), ("E".into(), EDGE), ("Knows".into(), EDGE), ("Person".into(), VERTEX), ("V".into(), VERTEX)]
        );
    }

    #[test]
    fn errors_and_urls() {
        let e = error_text(StatusCode::INTERNAL_SERVER_ERROR, r#"{"errors":[{"code":500,"content":"com.x.OCommandExecutionException: Class not found: X\tDB name=\"db\""}]}"#);
        assert_eq!(e, "Class not found: X");
        let e = error_text(StatusCode::BAD_REQUEST, r#"{"errors":[{"content":"Error parsing query:\nSELEC x\n    ^"}]}"#);
        assert!(e.starts_with("Error parsing query"));
        let body = r#"{"errors":[{"code":500,"content":"com.x.OCommandExecutionException: Class not found: X"}]}"#;
        assert_eq!(error_code(body).as_deref(), Some("OCommandExecutionException"));
        assert_eq!(error_code(r#"{"errors":[{"code":400,"content":"Error parsing query"}]}"#).as_deref(), Some("400"));
        assert_eq!(error_code("not json"), None);
        let c = ConnectionConfig { host: "db".into(), encrypt: true, ..Default::default() };
        assert_eq!(base_url(&c), "https://db:2480");
        assert!(is_gremlin(" g.V().count()") && !is_gremlin("SELECT g.x FROM T"));
        assert!(gremlin_writes("g.addV('P')") && !gremlin_writes("g.V().has('name','x')"));
        assert_eq!(strip_plan_prefix("profile SELECT 1"), "SELECT 1");
        assert!(is_read("  select from X") && is_read("MATCH {as: a} RETURN a") && !is_read("UPDATE X SET a = 1"));
        assert_eq!(ident("a b"), "`a b`");
    }

    #[test]
    fn inference() {
        let r = parse_reply(r##"{"result":[{"a":1,"l":"#9:1"},{"a":1.5,"e":{"x":1}}]}"##).unwrap();
        let c = infer_columns(&r.records);
        assert_eq!(c[0].data_type, "LONG|DOUBLE");
        assert_eq!(c[1].data_type, "LINK");
        assert!(c[1].nullable && !c[0].nullable);
        assert_eq!(c[2].data_type, "EMBEDDED");
    }
}

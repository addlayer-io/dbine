//! Dremio (Software / OSS) through its REST API v3: `POST /api/v3/sql`
//! submits a job, `GET /api/v3/job/{id}` is polled until it ends and
//! `GET /api/v3/job/{id}/results` pages the rows (500 at a time). The
//! session logs in at `/apiv2/login` (token `_dremio…`, renewed on 401) or
//! uses a personal access token (`Bearer`). The "database" is a top-level
//! container (space, source, home, `$scratch`) sent as the job's `context`;
//! `USE` changes it.
//!
//! Cancel is `POST /api/v3/job/{id}/cancel`. Plans come from `EXPLAIN PLAN
//! INCLUDING ALL ATTRIBUTES FOR`; actual figures from the job's profile
//! (`/apiv2/profiles/{id}.json`). The monitor reads `sys.nodes`,
//! `sys.memory` and `sys.jobs`.

mod ddl;
mod index_usage;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod script;
mod security;
mod sync;
mod transfer;

use base64::Engine as _;
use ddl::{lit, path};
use dbine_driver::sql::split_statements;
use dbine_driver::{
    async_trait, json_bytes, json_i64, json_u64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, Metric, MetricUnit, MonitorSnapshot,
    MonitorTable, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, RowChange, SchemaInfo, Session, TableSchema,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Rows per results page (the API's maximum).
const PAGE: usize = 500;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DremioDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "dremio",
        name: "Dremio",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "dremio",
        default_port: 9047,
        fields: vec![
            Field::host(),
            Field::port().placeholder("9047").help("Puerto HTTP de Dremio (la API REST)."),
            Field { label: "Espacio u origen", placeholder: "(ninguno)", ..Field::database() },
            Field::username(),
            Field::password(),
            Field::new("token", "Token de acceso personal", FieldKind::Password)
                .secret()
                .help("En lugar de usuario y contraseña (Dremio Software 24 o posterior)."),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Espacios y orígenes",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views()],
    }
}

pub struct DremioDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for DremioDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// Each statement is a job of its own, as the SQL runner sends them;
    /// `USE` stays in the session (the jobs' context).
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        dbine_driver::ScriptDialect { backtick_idents: false, ..dbine_driver::ScriptDialect::generic() }
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Batched `INSERT … SELECT … FROM (VALUES …)` into tables that take
    /// DML (Iceberg), see `transfer.rs`.
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Databases are spaces: created and dropped through the catalog API.
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

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
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
        sync::sync_script(changes)
    }

    /// A table's reflections, Dremio's nearest thing to indexes (see
    /// `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(ddl::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(ddl::update_script(target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        Ok(ddl::delete_script(target.schema(), &target.name, keys))
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Schemas are folders (see `security::schema_spec`).
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec())
    }

    /// In the menu's space or source (see `security::folder_in`).
    fn create_schema_script(&self, database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(database, name)
    }

    fn schema_grant_script(&self, database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
        security::schema_grant(database, name, privileges, to, grantable)
    }

    fn drop_schema_script(&self, database: Option<&str>, name: &str, _cascade: bool) -> Result<String> {
        security::drop_schema(database, name)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let conn = Arc::new(Conn {
            http,
            base: format!("{scheme}://{host}:{}", cfg.port_or(9047)),
            user: cfg.username.clone().unwrap_or_default(),
            password: cfg.password.clone().unwrap_or_default(),
            pat: cfg.option("token").map(str::to_string),
            auth: Mutex::new(None),
        });
        let db = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let s = DremioSession { conn: conn.clone(), context: db.map(str::to_string), cancel: Arc::new(Cancel::default()), rt: tokio::runtime::Handle::current(), profiler: None };
        let check = async {
            conn.login().await?;
            match &s.context {
                Some(c) => {
                    let rows = s.strings(&format!("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {} LIMIT 1", lit(c))).await?;
                    if rows.is_empty() {
                        return Err(Error::Query(format!("No existe el espacio u origen {c}.")));
                    }
                    Ok(())
                }
                None => s.strings("SELECT 1").await.map(|_| ()),
            }
        };
        tokio::time::timeout(Duration::from_secs(30), check)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    pat: Option<String>,
    /// The `Authorization` header value.
    auth: Mutex<Option<String>>,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

fn error_text(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("errorMessage").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.trim().chars().take(500).collect())
}

impl Conn {
    async fn login(&self) -> Result<()> {
        if let Some(pat) = &self.pat {
            *self.auth.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("Bearer {pat}"));
            return Ok(());
        }
        let resp = self
            .http
            .post(format!("{}/apiv2/login", self.base))
            .json(&json!({"userName": self.user, "password": self.password}))
            .send()
            .await
            .map_err(http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(http_error)?;
        if !status.is_success() {
            return Err(if status.as_u16() == 401 || status.as_u16() == 403 {
                Error::AuthFailed(error_text(&text))
            } else {
                Error::Connect(format!("HTTP {status}: {}", error_text(&text)))
            });
        }
        let token = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_string));
        match token {
            Some(t) => {
                *self.auth.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("_dremio{t}"));
                Ok(())
            }
            None => Err(Error::AuthFailed("Dremio no devolvió un token.".into())),
        }
    }

    /// A request with the session's auth; logs in again once on a 401.
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        for attempt in 0..2 {
            let mut rb = self.http.request(method.clone(), format!("{}{path}", self.base));
            if let Some(a) = self.auth.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                rb = rb.header("Authorization", a);
            }
            if let Some(b) = body {
                rb = rb.json(b);
            }
            let resp = rb.send().await.map_err(http_error)?;
            let status = resp.status();
            let text = resp.text().await.map_err(http_error)?;
            if status.as_u16() == 401 && attempt == 0 && self.pat.is_none() {
                self.login().await?;
                continue;
            }
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 => Error::AuthFailed(error_text(&text)),
                    _ => Error::Query(error_text(&text)),
                });
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).map_err(Error::query);
        }
        Err(Error::AuthFailed("Dremio rechazó la sesión.".into()))
    }
}

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
    /// Job in flight.
    job: Mutex<Option<String>>,
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

pub struct DremioSession {
    conn: Arc<Conn>,
    /// Top-level container the jobs run in (their `context`).
    context: Option<String>,
    cancel: Arc<Cancel>,
    rt: tokio::runtime::Handle,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn type_name(t: &Value) -> String {
    let name = t.get("name").map(text).unwrap_or_default();
    match (t.get("precision").and_then(Value::as_u64), t.get("scale").and_then(Value::as_u64)) {
        (Some(p), Some(s)) => format!("{name}({p},{s})"),
        _ => name,
    }
}

/// A cell given its Dremio type: binaries come base64, nested values as JSON.
fn cell(v: Value, ty: &str) -> Value {
    match v {
        Value::Number(n) if ty.starts_with("DECIMAL") => Value::String(n.to_string()),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => Value::Number(n),
        },
        Value::String(s) if ty == "VARBINARY" => match base64::engine::general_purpose::STANDARD.decode(&s) {
            Ok(b) => json_bytes(&b),
            Err(_) => Value::String(s),
        },
        v @ (Value::Array(_) | Value::Object(_)) => Value::String(v.to_string()),
        v => v,
    }
}

fn first_words(stmt: &str, n: usize) -> String {
    stmt.split_whitespace().take(n).collect::<Vec<_>>().join(" ").to_ascii_uppercase()
}

/// `USE a.b` → `a.b` (quotes dropped).
fn use_target(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    (first_words(s, 1) == "USE").then(|| s[3..].trim().replace('"', "")).filter(|x| !x.is_empty())
}

fn plannable(stmt: &str) -> bool {
    matches!(first_words(stmt, 1).as_str(), "SELECT" | "WITH" | "VALUES" | "INSERT" | "UPDATE" | "DELETE" | "MERGE")
        || first_words(stmt, 2) == "CREATE TABLE" && stmt.to_ascii_uppercase().contains(" AS ")
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

impl DremioSession {
    fn job_body(&self, sql: &str) -> Value {
        let mut b = json!({"sql": sql});
        if let Some(c) = &self.context {
            b["context"] = Value::Array(c.split('.').map(|p| Value::String(p.to_string())).collect());
        }
        b
    }

    /// Submit and wait for a job; its id and final state.
    async fn job(&self, sql: &str) -> Result<(String, Value)> {
        let submitted = self.cancel.run(self.conn.send(reqwest::Method::POST, "/api/v3/sql", Some(&self.job_body(sql)))).await?;
        let id = submitted.get("id").map(text).ok_or_else(|| Error::Query("Dremio no devolvió el id del trabajo.".into()))?;
        *self.cancel.job.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        let mut wait = 50u64;
        let r = loop {
            let st = match self.cancel.run(self.conn.send(reqwest::Method::GET, &format!("/api/v3/job/{id}"), None)).await {
                Ok(s) => s,
                Err(e) => break Err(e),
            };
            match st.get("jobState").and_then(Value::as_str).unwrap_or("") {
                "COMPLETED" => break Ok(st),
                "FAILED" => break Err(Error::Query(st.get("errorMessage").map(text).unwrap_or_else(|| "El trabajo falló.".into()))),
                "CANCELED" | "CANCELLED" => break Err(Error::Cancelled),
                _ => {}
            }
            if let Err(e) = self.cancel.run(async {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                Ok(())
            })
            .await
            {
                break Err(e);
            }
            wait = (wait * 2).min(500);
        };
        *self.cancel.job.lock().unwrap_or_else(|e| e.into_inner()) = None;
        r.map(|st| (id, st))
    }

    /// Results of a finished job: columns and at most `limit` rows.
    async fn results(&self, id: &str, total: usize, limit: usize) -> Result<(Vec<(String, String)>, Vec<Vec<Value>>)> {
        let mut columns: Vec<(String, String)> = Vec::new();
        let mut rows = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .cancel
                .run(self.conn.send(reqwest::Method::GET, &format!("/api/v3/job/{id}/results?offset={offset}&limit={PAGE}"), None))
                .await?;
            if columns.is_empty() {
                columns = page
                    .get("schema")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|c| (c.get("name").map(text).unwrap_or_default(), c.get("type").map(type_name).unwrap_or_default()))
                    .collect();
            }
            let got = page.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
            let n = got.len();
            for r in got {
                rows.push(columns.iter().map(|(name, _)| r.get(name).cloned().unwrap_or(Value::Null)).collect());
            }
            offset += n;
            if n == 0 || offset >= total || rows.len() >= limit {
                break;
            }
        }
        rows.truncate(limit);
        Ok((columns, rows))
    }

    async fn strings(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        let (id, st) = self.job(sql).await?;
        let total = st.get("rowCount").and_then(Value::as_u64).unwrap_or(0) as usize;
        let (_, rows) = self.results(&id, total, 100_000).await?;
        Ok(rows.into_iter().map(|r| r.iter().map(text).collect()).collect())
    }

    async fn records(&self, sql: &str) -> Result<Vec<serde_json::Map<String, Value>>> {
        let (id, st) = self.job(sql).await?;
        let total = st.get("rowCount").and_then(Value::as_u64).unwrap_or(0) as usize;
        let (cols, rows) = self.results(&id, total, 10_000).await?;
        Ok(rows.into_iter().map(|r| cols.iter().map(|(n, _)| n.clone()).zip(r).collect()).collect())
    }

    /// Run one statement into `out`; returns the job id.
    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<String> {
        if let Some(c) = use_target(stmt) {
            // Dremio has no USE: the context travels with each job.
            self.context = Some(c.clone());
            out.push_affected(0);
            if let Some(r) = out.results.last_mut() {
                r.tag = Some("USE".into());
            }
            out.info(format!("Contexto: {c}"));
            // The connection's database is that context: the tab follows.
            out.database = Some(c);
            return Ok(String::new());
        }
        let (id, st) = self.job(stmt).await?;
        let total = st.get("rowCount").and_then(Value::as_u64).unwrap_or(0) as usize;
        // Exports (a sink) take every row; the grid only `max_rows`.
        let limit = if out.sink.is_some() { usize::MAX } else { max_rows };
        let (cols, rows) = self.results(&id, total, limit).await?;
        let types: Vec<String> = cols.iter().map(|c| c.1.clone()).collect();
        out.begin_result(cols.into_iter().map(|(name, type_name)| ResultColumn { name, type_name }).collect());
        let fetched = rows.len();
        for row in rows {
            out.push_row(row.into_iter().enumerate().map(|(i, v)| cell(v, types.get(i).map_or("", String::as_str))).collect(), max_rows);
        }
        if let Some(r) = out.results.last_mut() {
            if total > fetched {
                r.total_rows = total as u64;
                r.truncated = true;
            }
        }
        Ok(id)
    }

    fn context(&self) -> Result<String> {
        self.context.clone().ok_or_else(|| Error::Query("Elegí un espacio u origen para ver sus objetos.".into()))
    }

    async fn plan_text(&self, stmt: &str) -> Result<String> {
        let rows = self.strings(&format!("EXPLAIN PLAN INCLUDING ALL ATTRIBUTES FOR {stmt}")).await?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or_default())
    }

    fn schema_of(&self, obj: &ObjectRef) -> Result<String> {
        obj.schema().map(str::to_string).map_or_else(|| self.context(), Ok)
    }
}

#[async_trait]
impl Session for DremioSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.strings("SELECT version FROM sys.version").await?;
        let v = rows.first().and_then(|r| r.first()).cloned().unwrap_or_default();
        Ok(format!("Dremio {}", v.split('-').next().unwrap_or(&v)))
    }

    /// Top-level containers: spaces, sources, homes and `$scratch`.
    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        if let Ok(cat) = self.cancel.run(self.conn.send(reqwest::Method::GET, "/api/v3/catalog", None)).await {
            for e in cat.get("data").and_then(Value::as_array).into_iter().flatten() {
                if let Some(p) = e.get("path").and_then(Value::as_array).and_then(|p| p.first()).map(text) {
                    out.push(p);
                }
            }
        }
        for r in self.strings("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA").await? {
            let top = r.first().map(|s| s.split('.').next().unwrap_or(s).to_string()).unwrap_or_default();
            if !top.is_empty() && !top.eq_ignore_ascii_case("INFORMATION_SCHEMA") && !out.contains(&top) {
                out.push(top);
            }
        }
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let c = self.context()?;
        let rows = self
            .strings(&format!(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.\"TABLES\"
                 WHERE TABLE_SCHEMA = {0} OR TABLE_SCHEMA LIKE {1} ESCAPE '\\' ORDER BY TABLE_SCHEMA, TABLE_NAME",
                lit(&c),
                lit(&format!("{}.%", c.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.len() == 3)
            .map(|r| DbObject {
                kind: if r[2] == "VIEW" { kinds::VIEW } else { kinds::TABLE }.into(),
                schema: Some(r[0].clone()),
                name: r[1].clone(),
                parent: None,
            })
            .collect())
    }

    /// The space or source and its folders, from INFORMATION_SCHEMA.SCHEMATA
    /// (which lists a space's folders even when empty), written as the
    /// objects' schemas are (plain dots).
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let Ok(c) = self.context() else { return Ok(None) };
        let rows = self
            .strings(&format!(
                "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA
                 WHERE SCHEMA_NAME = {0} OR SCHEMA_NAME LIKE {1} ESCAPE '\\' ORDER BY SCHEMA_NAME",
                lit(&c),
                lit(&format!("{}.%", c.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
            ))
            .await?;
        Ok(Some(rows.into_iter().filter_map(|r| r.into_iter().next()).map(|name| SchemaInfo { name, system: false }).collect()))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = self.schema_of(obj)?;
        let rows = self
            .strings(&format!(
                "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, NUMERIC_PRECISION, NUMERIC_SCALE, CHARACTER_MAXIMUM_LENGTH
                 FROM INFORMATION_SCHEMA.\"COLUMNS\" WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
                lit(&schema),
                lit(&obj.name)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.len() == 6)
            .map(|r| ColumnInfo {
                name: r[0].clone(),
                data_type: if r[1] == "DECIMAL" && !r[3].is_empty() { format!("DECIMAL({},{})", r[3], r[4]) } else { r[1].clone() },
                nullable: r[2] != "NO",
                primary_key: false,
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let objs = self.list_objects().await?;
        let mut out = Vec::new();
        for o in objs.into_iter().filter(|o| o.kind == kinds::TABLE) {
            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            let cols = self.columns(&r).await?;
            out.push(TableSchema {
                kind: o.kind,
                schema: o.schema,
                name: o.name,
                columns: cols.into_iter().map(|c| dbine_driver::ColumnDef { name: c.name, data_type: c.data_type, nullable: true, ..Default::default() }).collect(),
                ..Default::default()
            });
        }
        // Reflections as the tables' indexes; a login that can't read
        // sys.reflections gets the tables without them.
        match self.records(index_usage::SQL).await {
            Ok(rows) => index_usage::attach(&mut out, &rows),
            Err(e) => tracing::debug!("dremio: reflections not read: {e}"),
        }
        Ok(out)
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let schema = self.schema_of(table)?;
        match self.records(index_usage::SQL).await {
            Ok(rows) => Ok(Some(index_usage::report(&rows, Some(&schema), &table.name))),
            Err(Error::Query(e)) => Ok(Some(dbine_driver::IndexUsageReport {
                note: Some(format!("No se pudo leer sys.reflections (en Dremio Enterprise hace falta el privilegio VIEW REFLECTION): {e}")),
                seek_scan_split: false,
                writes_counted: false,
                ..Default::default()
            })),
            Err(e) => Err(e),
        }
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let schema = self.schema_of(obj)?;
        let name = path(Some(&schema), &obj.name);
        let what = if obj.kind == kinds::VIEW { "VIEW" } else { "TABLE" };
        match self.records(&format!("SHOW CREATE {what} {name}")).await {
            Ok(rows) => Ok(rows.first().and_then(|r| r.get("sql_definition")).map(text).map(|d| {
                if what == "VIEW" && !d.trim_start().to_ascii_uppercase().starts_with("CREATE") {
                    format!("CREATE OR REPLACE VIEW {name} AS\n{d};")
                } else {
                    format!("{d};")
                }
            })),
            // Older versions: views from INFORMATION_SCHEMA, tables without source.
            Err(Error::Query(_)) if what == "VIEW" => {
                let rows = self
                    .strings(&format!(
                        "SELECT VIEW_DEFINITION FROM INFORMATION_SCHEMA.VIEWS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                        lit(&schema),
                        lit(&obj.name)
                    ))
                    .await?;
                Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).map(|d| format!("CREATE OR REPLACE VIEW {name} AS\n{d};")))
            }
            Err(Error::Query(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT *\nFROM {}\nLIMIT {limit}", path(obj.schema(), &obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let d = dbine_driver::ScriptDialect { backtick_idents: false, ..dbine_driver::ScriptDialect::generic() };
        for unit in dbine_driver::sql::split_script(text, &d) {
            match self.run(&unit.text, max_rows, out).await {
                Ok(_) => {}
                Err(Error::Query(m)) => return Err(script::shift(script::job_error(&m, &unit.text), &unit)),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Estimated: `EXPLAIN PLAN INCLUDING ALL ATTRIBUTES` of each query or
    /// DML (nothing runs). Actual: each statement runs; its plan comes from
    /// the job's profile, with the rows and time of every operator.
    async fn explain(&mut self, script: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for stmt in split_statements(script) {
            let can = plannable(&stmt);
            if !analyze {
                if can {
                    let raw = self.plan_text(&stmt).await?;
                    out.plans.push(plan::estimated(&stmt, &raw));
                } else {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.chars().take(80).collect::<String>()));
                }
                continue;
            }
            let id = self.run(&stmt, max_rows, out).await?;
            if !can || id.is_empty() {
                continue;
            }
            let profile = self.cancel.run(self.conn.send(reqwest::Method::GET, &format!("/apiv2/profiles/{id}.json?attempt=0"), None)).await;
            let raw = profile.as_ref().ok().and_then(|p| {
                p.get("planPhases")?
                    .as_array()?
                    .iter()
                    .find(|ph| ph.get("phaseName").and_then(Value::as_str) == Some("Final Physical Transformation"))?
                    .get("plan")
                    .map(text)
            });
            match (profile, raw) {
                (Ok(p), Some(raw)) => out.plans.push(plan::actual(&stmt, &raw, &p)),
                (p, _) => {
                    let why = p.err().map(|e| e.to_string()).unwrap_or_else(|| "sin plan físico".into());
                    out.messages.push(format!("Sin el perfil del trabajo ({why}): se muestra el plan estimado."));
                    let raw = self.plan_text(&stmt).await?;
                    out.plans.push(plan::estimated(&stmt, &raw));
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

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let cancel = self.cancel.clone();
        let conn = self.conn.clone();
        let rt = self.rt.clone();
        Some(Arc::new(move || {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_waiters();
            let Some(id) = cancel.job.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                if let Err(e) = conn.send(reqwest::Method::POST, &format!("/api/v3/job/{id}/cancel"), None).await {
                    tracing::debug!("dremio cancel failed: {e}");
                }
            });
        }))
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

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.conn.send(reqwest::Method::POST, "/api/v3/catalog", Some(&json!({"entityType": "space", "name": name}))).await.map(|_| ())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let e = self.conn.send(reqwest::Method::GET, &format!("/api/v3/catalog/by-path/{}", encode(name)), None).await?;
        let id = e.get("id").map(text).ok_or_else(|| Error::Query(format!("No se encontró {name} en el catálogo.")))?;
        let tag = e.get("tag").map(text).unwrap_or_default();
        self.conn.send(reqwest::Method::DELETE, &format!("/api/v3/catalog/{id}?tag={}", encode(&tag)), None).await.map(|_| ())
    }

    /// The jobs that haven't ended (`sys.jobs`): Dremio has no sessions.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.processes_list().await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_running(id).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        let nodes = self.records("SELECT * FROM sys.nodes").await?;
        let memory = self.records("SELECT * FROM sys.memory").await.unwrap_or_default();
        let running = self
            .records(
                "SELECT job_id, user_name, status, query_type, queue_name, submitted_ts, submitted_epoch_millis, rows_scanned, query
                 FROM sys.jobs WHERE status NOT IN ('COMPLETED', 'FAILED', 'CANCELED', 'CANCELLED') ORDER BY submitted_ts LIMIT 200",
            )
            .await;
        let recent = self
            .records(
                "SELECT status, count(*) AS n, sum(execution_cpu_time_millis) AS cpu_ms, sum(rows_scanned) AS rows_scanned,
                        sum(bytes_scanned) AS bytes_scanned, sum(rows_returned) AS rows_returned
                 FROM sys.jobs WHERE submitted_ts > TIMESTAMPADD(MINUTE, -5, CURRENT_TIMESTAMP) GROUP BY status",
            )
            .await
            .ok();
        let version = self.strings("SELECT version FROM sys.version").await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next());
        let sum = |rows: &[serde_json::Map<String, Value>], col: &str| -> Option<f64> {
            let v: Vec<f64> = rows.iter().filter_map(|r| num(r.get(col))).collect();
            (!v.is_empty()).then(|| v.iter().sum())
        };
        let avg = |col: &str| sum(&nodes, col).map(|s| s / nodes.len().max(1) as f64);
        let recent_sum = |col: &str| recent.as_deref().and_then(|r| sum(r, col));
        let failed = recent.as_ref().map(|r| r.iter().filter(|x| x.get("status").map(text).as_deref() == Some("FAILED")).filter_map(|x| num(x.get("n"))).sum::<f64>());
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        let start = nodes.iter().filter_map(|n| n.get("start").map(text)).min();
        use MetricUnit::*;
        snap.metrics = vec![
            Metric::new("cpu", "CPU de los nodos", "CPU", Percent, avg("cpu")),
            Metric::new("load", "Carga de los nodos", "CPU", Percent, avg("load").map(|l| l * 100.0)),
            Metric::new("mem_used", "Heap usado", "Memoria", Bytes, sum(&memory, "heap_current")).max(sum(&memory, "heap_max")),
            Metric::new("direct_used", "Memoria directa usada", "Memoria", Bytes, sum(&memory, "direct_current")).max(sum(&memory, "direct_max")),
            Metric::new("active_sessions", "Trabajos en curso", "Conexiones", Count, running.as_ref().ok().map(|r| r.len() as f64)),
            Metric::new("queries", "Trabajos (últimos 5 min)", "Actividad", Count, recent_sum("n")),
            Metric::new("queries_failed", "Trabajos fallidos (últimos 5 min)", "Actividad", Count, failed),
            Metric::new("rows_read", "Filas leídas (últimos 5 min)", "Actividad", Count, recent_sum("rows_scanned")),
            Metric::new("bytes_scanned", "Bytes leídos (últimos 5 min)", "Actividad", Bytes, recent_sum("bytes_scanned")),
            Metric::new("cpu_time_jobs", "CPU de los trabajos (últimos 5 min)", "Actividad", Millis, recent_sum("cpu_ms")),
            Metric::new(
                "nodes_green",
                "Nodos sanos",
                "Cluster",
                Count,
                Some(nodes.iter().filter(|n| n.get("status").map(text).as_deref() == Some("green")).count() as f64),
            )
            .max(Some(nodes.len() as f64)),
            Metric::new("uptime", "Tiempo activo", "Servidor", Seconds, start.as_deref().and_then(epoch_ms).map(|s| ((now_ms - s) / 1000.0).max(0.0))),
        ];
        let mut t = MonitorTable::new("nodes", "Nodos", &["nodo", "IP", "estado", "coordinador", "ejecutor", "CPU %", "memoria %", "heap", "heap máx.", "iniciado"]);
        for n in &nodes {
            let host = n.get("hostname").map(text).unwrap_or_default();
            let port = n.get("fabric_port").and_then(Value::as_i64);
            let m = memory.iter().find(|m| m.get("hostname").map(text).as_deref() == Some(host.as_str()) && m.get("fabric_port").and_then(Value::as_i64) == port);
            t.rows.push(vec![
                n.get("name").cloned().unwrap_or(Value::Null),
                n.get("ip").cloned().unwrap_or(Value::Null),
                n.get("status").cloned().unwrap_or(Value::Null),
                n.get("is_coordinator").cloned().unwrap_or(Value::Null),
                n.get("is_executor").cloned().unwrap_or(Value::Null),
                n.get("cpu").cloned().unwrap_or(Value::Null),
                n.get("memory").cloned().unwrap_or(Value::Null),
                m.and_then(|m| m.get("heap_current")).cloned().unwrap_or(Value::Null),
                m.and_then(|m| m.get("heap_max")).cloned().unwrap_or(Value::Null),
                n.get("start").cloned().unwrap_or(Value::Null),
            ]);
        }
        snap.tables.push(t);
        match &running {
            Ok(rows) => {
                let mut t = MonitorTable::new("queries", "Trabajos en curso", &["id", "usuario", "estado", "tipo", "cola", "desde", "duración (s)", "filas leídas", "consulta"]);
                for r in rows.iter().filter(|r| !r.get("query").map(text).unwrap_or_default().contains("FROM sys.jobs WHERE status NOT IN")) {
                    let secs = num(r.get("submitted_epoch_millis")).map(|s| ((now_ms - s) / 1000.0).round());
                    t.rows.push(vec![
                        r.get("job_id").cloned().unwrap_or(Value::Null),
                        r.get("user_name").cloned().unwrap_or(Value::Null),
                        r.get("status").cloned().unwrap_or(Value::Null),
                        r.get("query_type").cloned().unwrap_or(Value::Null),
                        r.get("queue_name").cloned().unwrap_or(Value::Null),
                        r.get("submitted_ts").cloned().unwrap_or(Value::Null),
                        secs.map(Value::from).unwrap_or(Value::Null),
                        r.get("rows_scanned").cloned().unwrap_or(Value::Null),
                        Value::String(r.get("query").map(text).unwrap_or_default().chars().take(2000).collect()),
                    ]);
                }
                snap.tables.push(t);
            }
            Err(e) => snap.notes.push(format!("No se pudo leer sys.jobs ({e}): hace falta permiso de administrador para ver los trabajos de otros usuarios.")),
        }
        if let Some(v) = version {
            snap.info.push(("Versión".into(), v));
        }
        snap.info.push(("Nodos".into(), nodes.len().to_string()));
        if let Some(n) = nodes.iter().find(|n| n.get("is_master").and_then(Value::as_bool) == Some(true)) {
            snap.info.push(("Coordinador principal".into(), n.get("hostname").map(text).unwrap_or_default()));
        }
        if let Some(n) = nodes.first() {
            if let Some(w) = n.get("configured_max_width").map(text) {
                snap.info.push(("Paralelismo máximo por nodo".into(), w));
            }
        }
        snap.notes.push("Dremio no expone contadores acumulados por SQL: la actividad es la suma de los trabajos de los últimos 5 minutos (sys.jobs).".into());
        snap.notes.push("El CPU y la memoria % son los que informa cada nodo en sys.nodes; Dremio no expone el disco ni la red por SQL.".into());
        Ok(snap)
    }

    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        permissions::check(self, database).await
    }
}

/// `2024-01-31 13:45:00.123` (UTC) → epoch ms.
fn epoch_ms(s: &str) -> Option<f64> {
    let d = chrono_like(s)?;
    Some(d as f64)
}

/// Days-from-civil (no chrono needed for one timestamp).
fn chrono_like(s: &str) -> Option<i64> {
    let b = s.trim();
    let (date, time) = b.split_once([' ', 'T']).unwrap_or((b, "00:00:00"));
    let mut dp = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, d) = (dp.next()??, dp.next()??, dp.next()??);
    let mut tp = time.trim_end_matches('Z').split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: f64 = tp.next().unwrap_or("0").parse().ok()?;
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60) as f64 * 1000.0 + sec * 1000.0) as i64)
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
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
            let grant = |grantable| d.schema_grant_script(Some("nessie"), "ventas", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    #[test]
    fn cells_and_types() {
        assert_eq!(cell(json!(1.5), "DECIMAL(10,2)"), json!("1.5"));
        assert_eq!(cell(json!("YWJj"), "VARBINARY"), json!("0x616263"));
        assert_eq!(cell(json!([1, 2]), "LIST"), json!("[1,2]"));
        assert_eq!(cell(json!(9007199254740993i64), "BIGINT"), json!("9007199254740993"));
        assert_eq!(type_name(&json!({"name": "DECIMAL", "precision": 10, "scale": 2})), "DECIMAL(10,2)");
    }

    #[test]
    fn statements() {
        assert_eq!(use_target("USE \"$scratch\"").as_deref(), Some("$scratch"));
        assert_eq!(use_target("use sp.carpeta").as_deref(), Some("sp.carpeta"));
        assert!(plannable("select 1") && plannable("INSERT INTO t VALUES (1)") && plannable("CREATE TABLE t AS SELECT 1"));
        assert!(!plannable("CREATE TABLE t (a INT)") && !plannable("DROP TABLE t"));
        assert_eq!(chrono_like("1970-01-02 00:00:01.5"), Some(86_401_500));
        assert_eq!(chrono_like("2024-01-31 13:45:00.123"), Some(1_706_708_700_123));
    }

    #[test]
    fn job_context() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let s = DremioSession {
            conn: Arc::new(Conn { http: reqwest::Client::new(), base: String::new(), user: String::new(), password: String::new(), pat: None, auth: Mutex::new(None) }),
            context: Some("sp.carpeta".into()),
            cancel: Arc::new(Cancel::default()),
            rt: rt.handle().clone(),
            profiler: None,
        };
        assert_eq!(s.job_body("select 1")["context"], json!(["sp", "carpeta"]));
        let o = ObjectRef { kind: kinds::TABLE.into(), schema: Some("$scratch".into()), name: "t".into() };
        assert_eq!(s.browse_query(&o, 3), "SELECT *\nFROM \"$scratch\".\"t\"\nLIMIT 3");
        assert!(drivers()[0].capabilities().monitor);
    }
}

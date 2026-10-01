//! Apache Drill through its REST API: `POST /query.json` runs one
//! statement and answers with every row. REST is stateless, so the session
//! keeps what `USE` and `ALTER SESSION SET` change and sends it with each
//! request (`defaultSchema`, `options`); it also asks for verbose errors
//! (`drill.exec.http.rest.errors.verbose`), without which Drill doesn't say
//! why a statement failed. With a password, the session logs in with
//! Drill's form authentication and keeps the `JSESSIONID` cookie.
//!
//! Cancel drops the request and cancels the query on the server
//! (`/profiles/running.json` → `/profiles/cancel/{id}`). Plans come from
//! `EXPLAIN PLAN INCLUDING ALL ATTRIBUTES FOR`; actual figures from the
//! query's profile. The monitor reads `/status/metrics`, `sys.memory`,
//! `sys.drillbits`, `sys.connections` and the running queries.

mod permissions;
mod plan;
mod profiler;
mod transfer;

use base64::Engine as _;
use dbine_driver::sql::{split_script, split_statements, strip_comments, Quote, ScriptDefaults, ScriptDialect, ScriptMode, StatementKind};
use dbine_driver::{
    async_trait, json_bytes, json_i64, json_u64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    Driver, DriverInfo, Error, Family, Field, Language, Metric, MetricUnit, MonitorSnapshot, MonitorTable, ObjectKindInfo,
    ObjectRef, QueryOutcome, Result, ResultColumn, RowChange, Session,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const FILE: &str = "file";
const VERBOSE_ERRORS: &str = "drill.exec.http.rest.errors.verbose";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DrillDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "drill",
        name: "Apache Drill",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "drill",
        default_port: 8047,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8047").help("Puerto HTTP de un drillbit."),
            Field { label: "Esquema", placeholder: "dfs.tmp", ..Field::database() },
            Field::username().help("Solo si Drill tiene autenticación; sin ella, el usuario se ignora."),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Esquemas",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::new(FILE, "Archivos", true, true, false),
        ],
    }
}

pub struct DrillDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for DrillDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One REST query per statement; the session's `USE` and options are
    /// kept by the driver and sent with each one.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// sqlline stops at the first error.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: false, confirm_unsafe_dml: true }
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Drill's tables are files: CREATE TABLE AS writes them, nothing alters them.
    fn sync_script(&self, _changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported(
            "Drill consulta archivos: sus tablas se crean con CREATE TABLE AS y no tienen columnas que modificar con ALTER".into(),
        ))
    }

    /// Schemas are storage-plugin workspaces, configured in the plugin
    /// (no CREATE SCHEMA): no create / drop database.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Default::default() }
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        let t = |kind: &'static str, label: &'static str, template: &str| CreateTemplate { kind, label, template: template.into() };
        vec![
            t(
                kinds::TABLE,
                "Nueva tabla (CREATE TABLE AS)",
                "-- Drill crea tablas solo desde una consulta, en un workspace con escritura (p. ej. dfs.tmp).\n\
                 ALTER SESSION SET `store.format` = 'parquet';\n\
                 CREATE TABLE `{schema}`.`{name}` AS\nSELECT *\nFROM cp.`employee.json`\nLIMIT 100;\n",
            ),
            t(
                kinds::TABLE,
                "Nueva tabla temporal",
                "-- Vive mientras dura la sesión de Drill.\nCREATE TEMPORARY TABLE `{name}` AS\nSELECT *\nFROM cp.`employee.json`\nLIMIT 100;\n",
            ),
            t(
                kinds::VIEW,
                "Nueva vista",
                "CREATE OR REPLACE VIEW `{schema}`.`{name}` AS\nSELECT employee_id, full_name, salary\nFROM cp.`employee.json`\nWHERE salary > 1000;\n",
            ),
        ]
    }

    /// Drill has no INSERT: tables are written only by CREATE TABLE AS.
    fn insert_script(&self, _target: &ObjectRef, _columns: &[String], _rows: &[Vec<Value>]) -> Result<String> {
        Err(Error::Unsupported("Drill no tiene INSERT: las tablas se crean con CREATE TABLE AS SELECT.".into()))
    }

    /// Nor UPDATE: Drill's DML is limited to CTAS and DROP TABLE.
    fn update_script(&self, _target: &ObjectRef, _changes: &[RowChange]) -> Result<String> {
        Err(Error::Unsupported("Drill no tiene UPDATE: sus tablas no se modifican, se recrean con CREATE TABLE AS SELECT.".into()))
    }

    /// Nor DELETE: the same, rows can't be removed from a table.
    fn delete_script(&self, _target: &ObjectRef, _keys: &[Vec<(String, Value)>]) -> Result<String> {
        Err(Error::Unsupported("Drill no tiene DELETE: sus tablas no se modifican, se recrean con CREATE TABLE AS SELECT.".into()))
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::connect)?;
        let conn = Arc::new(Conn {
            http,
            base: format!("{scheme}://{host}:{}", cfg.port_or(8047)),
            user: cfg.username.clone().filter(|u| !u.is_empty()),
            password: cfg.password.clone().filter(|p| !p.is_empty()),
            cookie: Mutex::new(None),
        });
        let schema = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let s = DrillSession {
            conn: conn.clone(),
            schema: schema.map(str::to_string),
            options: BTreeMap::new(),
            cancel: Arc::new(Cancel::default()),
            rt: tokio::runtime::Handle::current(),
            profiler: None,
        };
        let check = async {
            if conn.password.is_some() {
                conn.login().await?;
            }
            match &s.schema {
                // `USE` fails on a schema that doesn't exist.
                Some(sc) => s.query(&format!("USE {}", quote(sc))).await.map(|_| ()),
                None => s.query("SELECT 1 FROM (VALUES(1))").await.map(|_| ()),
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
    user: Option<String>,
    password: Option<String>,
    cookie: Mutex<Option<String>>,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

impl Conn {
    /// Form authentication: `POST /j_security_check`; success redirects
    /// to the home page and sets `JSESSIONID`, failure to the login page.
    async fn login(&self) -> Result<()> {
        let user = self.user.clone().unwrap_or_default();
        let pass = self.password.clone().unwrap_or_default();
        let resp = self
            .http
            .post(format!("{}/j_security_check", self.base))
            .form(&[("j_username", user.as_str()), ("j_password", pass.as_str())])
            .send()
            .await
            .map_err(http_error)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Authentication is off: nothing to log in to.
            return Ok(());
        }
        let location = resp.headers().get("location").and_then(|l| l.to_str().ok()).unwrap_or("").to_string();
        let cookie = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with("JSESSIONID="))
            .map(|c| c.split(';').next().unwrap_or(c).to_string());
        if location.contains("login") || location.contains("error") {
            return Err(Error::AuthFailed("Drill rechazó el usuario o la contraseña.".into()));
        }
        if let Some(c) = cookie {
            *self.cookie.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
        }
        Ok(())
    }

    fn req(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb;
        if let Some(c) = self.cookie.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Cookie", c);
        }
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            rb = rb.basic_auth(u, Some(p));
        }
        rb
    }

    /// A response that is the login page (session expired) or a 401.
    fn needs_login(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
        status == reqwest::StatusCode::UNAUTHORIZED
            || (status.is_redirection() && headers.get("location").and_then(|l| l.to_str().ok()).is_some_and(|l| l.contains("login")))
    }

    async fn send_json(&self, body: &Value) -> Result<Value> {
        for attempt in 0..2 {
            let resp = self.req(self.http.post(format!("{}/query.json", self.base)).json(body)).send().await.map_err(http_error)?;
            let status = resp.status();
            if Self::needs_login(status, resp.headers()) {
                if attempt == 0 && self.password.is_some() {
                    self.login().await?;
                    continue;
                }
                return Err(Error::AuthFailed("Drill pide iniciar sesión: revisá el usuario y la contraseña.".into()));
            }
            let text = resp.text().await.map_err(http_error)?;
            return match serde_json::from_str::<Value>(&text) {
                Ok(v) => Ok(v),
                Err(_) => Err(Error::Query(format!("HTTP {status}: {}", text.trim().chars().take(500).collect::<String>()))),
            };
        }
        Err(Error::AuthFailed("Drill rechazó la sesión.".into()))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self.req(self.http.get(format!("{}{path}", self.base))).send().await.map_err(http_error)?;
        if !resp.status().is_success() {
            return Err(Error::Query(format!("HTTP {} en {path}", resp.status())));
        }
        resp.json().await.map_err(Error::query)
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

pub struct DrillSession {
    conn: Arc<Conn>,
    schema: Option<String>,
    /// `ALTER SESSION SET` values, sent with every request.
    options: BTreeMap<String, String>,
    cancel: Arc<Cancel>,
    rt: tokio::runtime::Handle,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

/// One answer of `/query.json`.
struct Answer {
    query_id: String,
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
}

/// `` `a`.`b` `` from `a.b`: Drill takes a dotted schema as one quoted name.
fn quote(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// The browse query restricted by the grid's column filters: the shared
/// SQL condition with Drill's backtick identifiers (double quotes aren't
/// identifiers there).
fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let flavor = dbine_driver::ddl::SqlFlavor::ansi();
    let literal = |v: &Value| dbine_driver::ddl::sql_literal(&flavor, v);
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &literal, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
    insert_where(browse, &sql_condition(filters, &style)?)
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A cell as the UI wants it, given its Drill type. Temporal values come
/// as epoch milliseconds (TIME: of the day), binaries as base64.
fn cell(v: Value, ty: &str) -> Value {
    let ty = ty.split('(').next().unwrap_or(ty).trim();
    match (ty, v) {
        ("TIMESTAMP", Value::Number(n)) => n
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.3f").to_string()))
            .unwrap_or(Value::Number(n)),
        ("DATE", Value::Number(n)) => n
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|d| Value::String(d.format("%Y-%m-%d").to_string()))
            .unwrap_or(Value::Number(n)),
        ("TIME", Value::Number(n)) => n
            .as_i64()
            .map(|ms| Value::String(format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)))
            .unwrap_or(Value::Number(n)),
        ("VARBINARY" | "BINARY", Value::String(s)) => match base64::engine::general_purpose::STANDARD.decode(&s) {
            Ok(b) => json_bytes(&b),
            Err(_) => Value::String(s),
        },
        ("VARDECIMAL" | "DECIMAL", Value::Number(n)) => Value::String(n.to_string()),
        (_, Value::Number(n)) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => Value::Number(n),
        },
        (_, v @ (Value::Array(_) | Value::Object(_))) => Value::String(v.to_string()),
        (_, v) => v,
    }
}

fn first_words(stmt: &str, n: usize) -> String {
    stmt.split_whitespace().take(n).collect::<Vec<_>>().join(" ").to_ascii_uppercase()
}

/// `ALTER SESSION SET `k` = v` / `SET k = v` → (k, v); `RESET` → (k, None).
fn session_option(stmt: &str) -> Option<(String, Option<String>)> {
    let s = stmt.trim();
    let upper = s.to_ascii_uppercase();
    let rest = if upper.starts_with("ALTER SESSION SET ") {
        &s[18..]
    } else if upper.starts_with("SET ") {
        &s[4..]
    } else if upper.starts_with("ALTER SESSION RESET ") {
        return Some((s[20..].trim().trim_matches('`').to_string(), None));
    } else if upper.starts_with("RESET ") {
        return Some((s[6..].trim().trim_matches('`').to_string(), None));
    } else {
        return None;
    };
    let (k, v) = rest.split_once('=')?;
    let v = v.trim();
    let v = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).map(|x| x.replace("''", "'")).unwrap_or_else(|| v.to_string());
    Some((k.trim().trim_matches('`').to_string(), Some(v)))
}

/// Drill's error as a script error: its kind as the code ("PARSE",
/// "VALIDATION"…, else the exception's class, "CalciteContextException")
/// and where, from "at line 1, column 8" or "From line 1, column 15 to …"
/// (in `sql`, the statement sent).
fn drill_error(msg: String, exception: Option<&str>, sql: &str) -> Error {
    let mut e = dbine_driver::ScriptError::new(msg.clone());
    let kind = msg.split_once(" ERROR:").map(|(k, _)| k).filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
    let class = exception.and_then(|x| x.rsplit('.').next()).filter(|c| !c.is_empty() && *c != "Exception");
    if let Some(code) = kind.or(class) {
        e = e.with_code(code);
    }
    let pos = msg.split("line ").nth(1).and_then(|r| {
        let (l, rest) = r.split_once(", column ")?;
        let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some((l.trim().parse::<usize>().ok()?, c.parse::<usize>().ok()?))
    });
    if let Some((line, col)) = pos.filter(|(l, c)| *l >= 1 && *c >= 1) {
        let line_start: usize = sql.split_inclusive('\n').take(line - 1).map(str::len).sum();
        if line_start <= sql.len() {
            let rest = &sql[line_start..];
            e = e.at_offset(line_start + rest.char_indices().nth(col - 1).map_or(rest.len(), |(b, _)| b)).at_line(line as u32);
        }
    }
    e.into()
}

/// sqlline's: `;` outside quotes and comments, `` `name` ``.
fn dialect() -> ScriptDialect {
    ScriptDialect { compound_blocks: false, ..ScriptDialect::generic() }
}

/// `USE x` → `x`.
fn use_target(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    (first_words(s, 1) == "USE").then(|| s[3..].trim().replace('`', "")).filter(|x| !x.is_empty())
}

/// The schema in Drill's `USE` summary: "Default schema changed to [x]".
fn changed_schema(summary: &str) -> Option<String> {
    let rest = &summary[summary.find("schema changed to [")? + "schema changed to [".len()..];
    Some(rest[..rest.rfind(']')?].to_string()).filter(|x| !x.is_empty())
}

impl DrillSession {
    fn body(&self, sql: &str) -> Value {
        let mut options: serde_json::Map<String, Value> = self.options.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        options.insert(VERBOSE_ERRORS.into(), Value::String("true".into()));
        let mut b = json!({"queryType": "SQL", "query": sql, "options": options});
        if let Some(s) = &self.schema {
            b["defaultSchema"] = Value::String(s.clone());
        }
        if let Some(u) = &self.conn.user {
            b["userName"] = Value::String(u.clone());
        }
        b
    }

    async fn query(&self, sql: &str) -> Result<Answer> {
        let v = self.cancel.run(self.conn.send_json(&self.body(sql))).await?;
        let state = v.get("queryState").and_then(Value::as_str).unwrap_or("");
        if state == "FAILED" || state == "CANCELED" || v.get("errorMessage").is_some() {
            if state == "CANCELED" {
                return Err(Error::Cancelled);
            }
            let msg = v.get("errorMessage").map(text).unwrap_or_else(|| {
                "La consulta falló durante la ejecución; Drill no informa el detalle por REST (mirá el perfil de la consulta).".into()
            });
            let exception = v.get("exception").map(text);
            return Err(drill_error(msg, exception.as_deref(), sql));
        }
        let names: Vec<String> = v.get("columns").and_then(Value::as_array).into_iter().flatten().map(text).collect();
        let types: Vec<String> = v.get("metadata").and_then(Value::as_array).into_iter().flatten().map(text).collect();
        let rows = v
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| names.iter().map(|n| r.get(n).cloned().unwrap_or(Value::Null)).collect())
            .collect();
        Ok(Answer {
            query_id: v.get("queryId").map(text).unwrap_or_default(),
            columns: names.into_iter().enumerate().map(|(i, n)| (n, types.get(i).cloned().unwrap_or_default())).collect(),
            rows,
        })
    }

    async fn strings(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        Ok(self.query(sql).await?.rows.iter().map(|r| r.iter().map(text).collect()).collect())
    }

    async fn records(&self, sql: &str) -> Result<Vec<serde_json::Map<String, Value>>> {
        let a = self.query(sql).await?;
        Ok(a.rows.into_iter().map(|r| a.columns.iter().map(|(n, _)| n.clone()).zip(r).collect()).collect())
    }

    /// Run one statement; its result goes to `out`. Returns the query id.
    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<String> {
        self.run_as(stmt, stmt, max_rows, out).await
    }

    /// [`Self::run`] of `stmt`, whose text without comments is `bare`.
    async fn run_as(&mut self, stmt: &str, bare: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<String> {
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(stmt.to_string());
        let r = self.query(stmt).await;
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let a = r?;
        if let Some(s) = use_target(bare) {
            // Drill's answer names the schema in full ("Default schema
            // changed to [dfs.tmp]"); the tab follows it.
            let s = a.rows.iter().flatten().find_map(|v| changed_schema(&text(v))).unwrap_or(s);
            self.schema = Some(s.clone());
            out.database = Some(s);
        }
        if let Some((k, v)) = session_option(bare) {
            match v {
                Some(v) => {
                    self.options.insert(k, v);
                }
                None if k.eq_ignore_ascii_case("ALL") => self.options.clear(),
                None => {
                    self.options.remove(&k);
                }
            }
        }
        let types: Vec<String> = a.columns.iter().map(|c| c.1.clone()).collect();
        out.begin_result(a.columns.into_iter().map(|(name, type_name)| ResultColumn { name, type_name }).collect());
        for row in a.rows {
            out.push_row(row.into_iter().enumerate().map(|(i, v)| cell(v, types.get(i).map_or("", String::as_str))).collect(), max_rows);
        }
        Ok(a.query_id)
    }

    fn schema(&self) -> Result<String> {
        self.schema.clone().ok_or_else(|| Error::Query("Elegí un esquema para ver sus objetos.".into()))
    }

    fn name(&self, obj: &ObjectRef) -> String {
        match obj.schema() {
            Some(s) => format!("{}.{}", quote(s), quote(&obj.name)),
            None => quote(&obj.name),
        }
    }

    async fn plan_text(&self, stmt: &str) -> Result<String> {
        let rows = self.strings(&format!("EXPLAIN PLAN INCLUDING ALL ATTRIBUTES FOR {stmt}")).await?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or_default())
    }
}

/// Whether a running query (its text as `/profiles/running.json` lists
/// it: cut to 150 characters) is `stmt`.
fn same_query(listed: &str, stmt: &str) -> bool {
    let l = listed.trim();
    let cut = l.trim_end_matches("...").trim_end();
    l == stmt.trim() || (cut.chars().count() >= 100 && stmt.trim().starts_with(cut))
}

fn is_read(stmt: &str) -> bool {
    matches!(first_words(stmt, 1).as_str(), "SELECT" | "WITH" | "VALUES")
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

#[async_trait]
impl Session for DrillSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.strings("SELECT version FROM sys.version").await?;
        Ok(format!("Apache Drill {}", rows.first().and_then(|r| r.first()).cloned().unwrap_or_default()))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.strings("SHOW SCHEMAS").await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).filter(|s| !s.eq_ignore_ascii_case("information_schema")).collect())
    }

    /// Tables and views from INFORMATION_SCHEMA; in file-system workspaces,
    /// also the files and directories (`SHOW FILES`), each queryable as a table.
    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let schema = self.schema()?;
        let rows = self
            .strings(&format!(
                "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.`TABLES` WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
                lit(&schema)
            ))
            .await?;
        let mut out: Vec<DbObject> = rows
            .into_iter()
            .filter(|r| r.len() == 2)
            .map(|r| DbObject {
                kind: if r[1] == "VIEW" { kinds::VIEW } else { kinds::TABLE }.into(),
                schema: Some(schema.clone()),
                name: r[0].clone(),
                parent: None,
            })
            .collect();
        if let Ok(files) = self.records(&format!("SHOW FILES IN {}", quote(&schema))).await {
            for f in files {
                let name = f.get("name").map(text).unwrap_or_default();
                if name.is_empty() || name.starts_with('.') || name.starts_with('_') || name.ends_with(".view.drill") || name.ends_with(".crc") {
                    continue;
                }
                if out.iter().any(|o| o.name == name) {
                    continue;
                }
                out.push(DbObject { kind: FILE.into(), schema: Some(schema.clone()), name, parent: None });
            }
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = obj.schema().map(str::to_string).map_or_else(|| self.schema(), Ok)?;
        if obj.kind != FILE {
            let rows = self
                .strings(&format!(
                    "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_DEFAULT
                     FROM INFORMATION_SCHEMA.`COLUMNS` WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
                    lit(&schema),
                    lit(&obj.name)
                ))
                .await?;
            if !rows.is_empty() {
                return Ok(rows
                    .into_iter()
                    .filter(|r| r.len() == 7)
                    .map(|r| {
                        let ty = match r[1].as_str() {
                            "CHARACTER VARYING" | "BINARY VARYING" if !r[3].is_empty() && r[3] != "65536" => format!("{}({})", r[1], r[3]),
                            "DECIMAL" if !r[4].is_empty() => format!("DECIMAL({},{})", r[4], r[5]),
                            t => t.to_string(),
                        };
                        ColumnInfo { name: r[0].clone(), data_type: ty, nullable: r[2] != "NO", primary_key: false, auto_increment: false, default_value: (!r[6].is_empty()).then(|| r[6].clone()) }
                    })
                    .collect());
            }
        }
        // Files (and tables INFORMATION_SCHEMA doesn't describe): the
        // columns of a one-row sample.
        let a = self.query(&format!("SELECT * FROM {}.{} LIMIT 1", quote(&schema), quote(&obj.name))).await?;
        Ok(a.columns
            .into_iter()
            .map(|(name, ty)| ColumnInfo { name, data_type: ty, nullable: true, primary_key: false, auto_increment: false, default_value: None })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if obj.kind != kinds::VIEW {
            return Ok(None);
        }
        let schema = obj.schema().map(str::to_string).map_or_else(|| self.schema(), Ok)?;
        let rows = self
            .strings(&format!(
                "SELECT VIEW_DEFINITION FROM INFORMATION_SCHEMA.VIEWS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                lit(&schema),
                lit(&obj.name)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .map(|def| format!("CREATE OR REPLACE VIEW {}.{} AS\n{def};", quote(&schema), quote(&obj.name))))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT *\nFROM {}\nLIMIT {limit}", self.name(obj))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let d = dialect();
        for unit in split_script(text, &d).into_iter().filter(|u| u.kind != StatementKind::ClientCommand) {
            // Sent as written (Drill reads comments), so its positions hold;
            // USE and SET are recognised without the comments.
            let bare = strip_comments(&unit.text, &d, false).trim().to_string();
            self.run_as(&unit.text, &bare, max_rows, out).await.map_err(|e| match e {
                Error::Statement(mut se) => {
                    se.offset = se.offset.map(|o| unit.start + o);
                    se.line = Some(se.line.map_or(unit.line, |l| unit.line + l - 1));
                    Error::Statement(se)
                }
                e => e,
            })?;
        }
        Ok(())
    }

    /// Estimated: `EXPLAIN PLAN INCLUDING ALL ATTRIBUTES` of each query
    /// (CTAS included), nothing runs. Actual: each statement runs; queries
    /// and CTAS get the plan with the figures of their profile.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for stmt in split_statements(text) {
            let plannable = is_read(&stmt) || first_words(&stmt, 2) == "CREATE TABLE";
            if !analyze {
                if plannable {
                    let raw = self.plan_text(&stmt).await?;
                    out.plans.push(plan::estimated(&stmt, &raw));
                } else {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.chars().take(80).collect::<String>()));
                }
                continue;
            }
            let raw = if plannable { Some(self.plan_text(&stmt).await?) } else { None };
            let id = self.run(&stmt, max_rows, out).await?;
            if let Some(raw) = raw {
                let profile = self.cancel.run(self.conn.get(&format!("/profiles/{id}.json"))).await;
                out.plans.push(match profile {
                    Ok(p) => plan::actual(&stmt, &raw, &p),
                    Err(e) => {
                        out.messages.push(format!("Sin el perfil de la consulta ({e}): se muestra el plan estimado."));
                        plan::estimated(&stmt, &raw)
                    }
                });
            }
        }
        Ok(())
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
            let Some(stmt) = cancel.current.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                // The query may not be registered yet right after it's sent.
                for _ in 0..10 {
                    if let Ok(v) = conn.get("/profiles/running.json").await {
                        let ids: Vec<String> = v
                            .get("runningQueries")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter(|q| q.get("query").and_then(Value::as_str).is_some_and(|t| same_query(t, &stmt)))
                            .filter_map(|q| q.get("queryId").and_then(Value::as_str).map(str::to_string))
                            .collect();
                        if !ids.is_empty() {
                            for id in ids {
                                let _ = conn.req(conn.http.get(format!("{}/profiles/cancel/{id}", conn.base))).send().await;
                            }
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            });
        }))
    }

    /// The streamed `/query.json` answer read row by row, typed (see `transfer.rs`).
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.transfer_read(spec, sink).await
    }

    /// Drill has no INSERT (only CREATE TABLE AS SELECT from what it reads).
    async fn bulk_load(
        &mut self,
        _spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        _source: &mut dyn dbine_driver::transfer::BatchSource,
        _progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        Err(Error::Unsupported("Drill no tiene INSERT: sus tablas solo se crean con CREATE TABLE AS SELECT a partir de lo que Drill lee, así que no puede recibir filas de otra base.".into()))
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        let metrics = match self.cancel.run(self.conn.get("/status/metrics")).await {
            Ok(m) => Some(m),
            Err(e) => {
                snap.notes.push(format!("No se pudo leer /status/metrics ({e}): hace falta un usuario administrador si Drill tiene autenticación."));
                None
            }
        };
        let g = |k: &str| metrics.as_ref().and_then(|m| num(m.pointer(&format!("/gauges/{k}/value"))));
        let c = |k: &str| metrics.as_ref().and_then(|m| num(m.pointer(&format!("/counters/{k}/count"))));
        let memory = self.records("SELECT * FROM sys.memory").await.unwrap_or_default();
        let bits = self.records("SELECT * FROM sys.drillbits").await.unwrap_or_default();
        let conns = self.records("SELECT * FROM sys.connections").await.ok();
        let running = self.cancel.run(self.conn.get("/profiles/running.json")).await.ok();
        let version = self.strings("SELECT version FROM sys.version").await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next());
        let sum = |col: &str| -> Option<f64> {
            let v: Vec<f64> = memory.iter().filter_map(|r| num(r.get(col))).collect();
            (!v.is_empty()).then(|| v.iter().sum())
        };
        let queries_done = match (c("drill.queries.succeeded"), c("drill.queries.failed"), c("drill.queries.canceled")) {
            (None, None, None) => c("drill.queries.completed"),
            (a, b, d) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0) + d.unwrap_or(0.0)),
        };
        let user_conns = match (c("drill.connections.rpc.user.unencrypted"), c("drill.connections.rpc.user.encrypted")) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        use MetricUnit::*;
        snap.metrics = vec![
            Metric::new("cpu", "CPU del drillbit", "CPU", Percent, g("drillbit.load.avg").map(|v| v * 100.0)),
            Metric::new("load_avg", "Carga del sistema (load average)", "CPU", Count, g("os.load.avg")),
            Metric::new("mem_used", "Heap usado (cluster)", "Memoria", Bytes, sum("heap_current")).max(sum("heap_max")),
            Metric::new("direct_used", "Memoria directa usada (cluster)", "Memoria", Bytes, sum("direct_current")).max(sum("direct_max")),
            Metric::new("connections", "Conexiones", "Conexiones", Count, conns.as_ref().map(|c| c.len() as f64).or(user_conns)),
            Metric::new("active_sessions", "Consultas en curso", "Conexiones", Count, c("drill.queries.running")),
            Metric::new("queued", "Consultas en cola", "Conexiones", Count, c("drill.queries.enqueued")),
            Metric::new("queries", "Consultas terminadas", "Actividad", Count, queries_done).counter(),
            Metric::new("queries_failed", "Consultas fallidas", "Actividad", Count, c("drill.queries.failed")).counter(),
            Metric::new("fragments", "Fragmentos en ejecución", "Actividad", Count, g("drill.fragments.running")),
            Metric::new("threads", "Hilos", "Servidor", Count, g("threads.count")),
            Metric::new("fd_usage", "Descriptores de archivo en uso", "Servidor", Percent, g("fd.usage").map(|v| v * 100.0)),
            Metric::new("uptime", "Tiempo activo", "Servidor", Seconds, g("drillbit.uptime").map(|ms| ms / 1000.0)),
        ];
        let mut nodes = MonitorTable::new("nodes", "Drillbits", &["host", "puerto", "estado", "versión", "heap", "heap máx.", "directa", "directa máx.", "actual"]);
        for b in &bits {
            let host = b.get("hostname").map(text).unwrap_or_default();
            let port = b.get("user_port").cloned().unwrap_or(Value::Null);
            let m = memory.iter().find(|m| m.get("hostname").map(text).as_deref() == Some(host.as_str()) && m.get("user_port") == Some(&port));
            let mv = |k: &str| m.and_then(|m| m.get(k)).cloned().unwrap_or(Value::Null);
            nodes.rows.push(vec![
                Value::String(host.clone()),
                port.clone(),
                b.get("state").cloned().unwrap_or(Value::Null),
                b.get("version").cloned().unwrap_or(Value::Null),
                mv("heap_current"),
                mv("heap_max"),
                mv("direct_current"),
                mv("direct_max"),
                b.get("current").cloned().unwrap_or(Value::Null),
            ]);
        }
        snap.tables.push(nodes);
        if let Some(conns) = &conns {
            let mut t = MonitorTable::new("sessions", "Sesiones", &["sesión", "usuario", "cliente", "drillbit", "desde", "duración", "consultas", "cifrada"]);
            for r in conns.iter().take(200) {
                t.rows.push(vec![
                    r.get("session").cloned().unwrap_or(Value::Null),
                    r.get("user").cloned().unwrap_or(Value::Null),
                    r.get("client").cloned().unwrap_or(Value::Null),
                    r.get("drillbit").cloned().unwrap_or(Value::Null),
                    r.get("established").cloned().map(|v| cell(v, "TIMESTAMP")).unwrap_or(Value::Null),
                    r.get("duration").cloned().unwrap_or(Value::Null),
                    r.get("queries").cloned().unwrap_or(Value::Null),
                    r.get("isEncrypted").cloned().unwrap_or(Value::Null),
                ]);
            }
            snap.tables.push(t);
        }
        let mut t = MonitorTable::new("queries", "Consultas en curso", &["id", "usuario", "foreman", "estado", "duración", "costo", "cola", "consulta"]);
        for r in running.as_ref().and_then(|r| r.get("runningQueries")).and_then(Value::as_array).into_iter().flatten().take(200) {
            t.rows.push(vec![
                r.get("queryId").cloned().unwrap_or(Value::Null),
                r.get("user").cloned().unwrap_or(Value::Null),
                r.get("foreman").cloned().unwrap_or(Value::Null),
                r.get("state").cloned().unwrap_or(Value::Null),
                r.get("duration").cloned().unwrap_or(Value::Null),
                r.get("totalCost").cloned().unwrap_or(Value::Null),
                r.get("queueName").cloned().unwrap_or(Value::Null),
                Value::String(r.get("query").map(text).unwrap_or_default().chars().take(2000).collect()),
            ]);
        }
        snap.tables.push(t);
        if let Some(v) = version {
            snap.info.push(("Versión".into(), v));
        }
        snap.info.push(("Drillbits".into(), bits.len().to_string()));
        if let Ok(cl) = self.cancel.run(self.conn.get("/cluster.json")).await {
            for (label, key) in [("Autenticación", "authEnabled"), ("Cifrado de usuarios", "userEncryptionEnabled"), ("Cifrado entre drillbits", "bitEncryptionEnabled")] {
                if let Some(b) = cl.get(key).and_then(Value::as_bool) {
                    snap.info.push((label.into(), if b { "sí" } else { "no" }.into()));
                }
            }
        }
        snap.notes.push("El CPU, los hilos, las consultas y el tiempo activo son del drillbit al que se conecta DBine; la memoria suma todos los drillbits (sys.memory).".into());
        snap.notes.push("Drill no guarda datos propios: no hay espacio usado, caché ni bloqueos que informar; esos dependen de cada origen (archivos, Hive, JDBC…).".into());
        Ok(snap)
    }

    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_carry_kind_and_position() {
        let sql = "SELECT *\n  FROM nope";
        let e = drill_error("VALIDATION ERROR: From line 2, column 8 to line 2, column 11: Object 'nope' not found\n\n[Error Id: x]".into(), None, sql)
            .to_script_error();
        assert_eq!((e.code.as_deref(), e.line, e.offset), (Some("VALIDATION"), Some(2), Some(sql.find("nope").unwrap())));
        let e = drill_error("At line 1, column 3: x".into(), Some("org.apache.calcite.runtime.CalciteContextException"), sql).to_script_error();
        assert_eq!((e.code.as_deref(), e.offset), (Some("CalciteContextException"), Some(2)));
        let e = drill_error("La consulta falló".into(), Some("java.lang.Exception"), sql).to_script_error();
        assert_eq!((e.code, e.line, e.offset), (None, None, None));
    }

    #[test]
    fn delete_script_is_unsupported() {
        let t = ObjectRef { kind: "table".into(), schema: Some("dfs.tmp".into()), name: "t".into() };
        let keys = vec![vec![("nombre".into(), serde_json::json!("O'Brien")), ("region".into(), Value::Null)]];
        assert!(matches!(drivers()[0].delete_script(&t, &keys), Err(Error::Unsupported(_))));
    }

    #[test]
    fn filtered_browse_with_backticks() {
        use dbine_driver::{ColumnFilter, FilterOp};
        use serde_json::json;
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT *\nFROM `dfs.tmp`.`t`\nLIMIT 200",
                &[
                    f("name", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("note", FilterOp::Contains, vec![json!("50%")]),
                    f("n", FilterOp::Lt, vec![json!(9)]),
                    f("gone", FilterOp::IsNull, vec![]),
                    f("id", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM `dfs.tmp`.`t`\nWHERE `name` = 'O''Brien'\n  AND `note` LIKE '%50\\%%' ESCAPE '\\'\n  AND `n` < 9\n  AND `gone` IS NULL\n  AND `id` IN (1, 2)\nLIMIT 200"
        );
    }

    #[test]
    fn updates_are_unsupported() {
        let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dfs.tmp".into()), name: "t".into() };
        let change = RowChange { key: vec![("id".into(), serde_json::json!(1))], set: vec![("v".into(), serde_json::json!(2))], ..Default::default() };
        assert!(matches!(DrillDriver { info: info() }.update_script(&target, &[change]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn cells() {
        assert_eq!(cell(json!(1706708700123i64), "TIMESTAMP"), json!("2024-01-31 13:45:00.123"));
        assert_eq!(cell(json!(1706659200000i64), "DATE"), json!("2024-01-31"));
        assert_eq!(cell(json!(49500123), "TIME"), json!("13:45:00.123"));
        assert_eq!(cell(json!("yv4="), "VARBINARY"), json!("0xCAFE"));
        assert_eq!(cell(json!(1.5), "VARDECIMAL(10, 2)"), json!("1.5"));
        assert_eq!(cell(json!(9007199254740993i64), "BIGINT"), json!("9007199254740993"));
        assert_eq!(cell(json!({"a": [1]}), "MAP"), json!("{\"a\":[1]}"));
    }

    #[test]
    fn session_statements() {
        assert_eq!(session_option("ALTER SESSION SET `store.format` = 'json'"), Some(("store.format".into(), Some("json".into()))));
        assert_eq!(session_option("set planner.width.max_per_node = 2"), Some(("planner.width.max_per_node".into(), Some("2".into()))));
        assert_eq!(session_option("ALTER SESSION RESET `store.format`"), Some(("store.format".into(), None)));
        assert_eq!(session_option("select 1"), None);
        assert_eq!(use_target("USE `dfs`.`tmp`").as_deref(), Some("dfs.tmp"));
        assert_eq!(use_target("use dfs.tmp").as_deref(), Some("dfs.tmp"));
        assert_eq!(use_target("USE cp.`default`").as_deref(), Some("cp.default"));
        assert_eq!(changed_schema("Default schema changed to [dfs.tmp]").as_deref(), Some("dfs.tmp"));
        assert_eq!(changed_schema("ok"), None);
        let long = format!("SELECT {} FROM t", "x, ".repeat(60));
        assert!(same_query(&format!("{}...", &long[..150]), &long) && same_query("select 1", " select 1") && !same_query("select 2", "select 1"));
        assert!(is_read(" with x as (select 1) select * from x") && !is_read("create table t as select 1"));
    }

    #[test]
    fn body_carries_state() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let s = DrillSession {
            conn: Arc::new(Conn { http: reqwest::Client::new(), base: String::new(), user: None, password: None, cookie: Mutex::new(None) }),
            schema: Some("dfs.tmp".into()),
            options: [("store.format".to_string(), "json".to_string())].into(),
            cancel: Arc::new(Cancel::default()),
            rt: rt.handle().clone(),
            profiler: None,
        };
        let b = s.body("select 1");
        assert_eq!(b["defaultSchema"], json!("dfs.tmp"));
        assert_eq!(b["options"]["store.format"], json!("json"));
        assert_eq!(b["options"][VERBOSE_ERRORS], json!("true"));
        let o = ObjectRef { kind: FILE.into(), schema: Some("dfs.tmp".into()), name: "a b.csv".into() };
        assert_eq!(s.browse_query(&o, 5), "SELECT *\nFROM `dfs.tmp`.`a b.csv`\nLIMIT 5");
    }
}

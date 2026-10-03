//! ClickHouse over its HTTP interface (8123, or 8443 with TLS), and Timeplus
//! Proton, a ClickHouse fork with the same interface (its batch endpoint is
//! also 8123; 3218 runs streaming queries).
//!
//! One statement per request: scripts are split and each statement gets its
//! own `query_id`, so `interrupter` can `KILL QUERY` it from another request.
//! Results come as `JSONCompactEachRowWithNamesAndTypes` and are read line by
//! line. A `session_id` keeps `SET`s and temporary tables between runs.

mod backup;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod schema;
mod security;
mod sync;
mod transfer;

use dbine_driver::sql::{
    quote_ident, select_top, split_script, strip_comments, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode, StatementKind,
};
use dbine_driver::{
    json_i64, json_u64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, Language, ObjectKindInfo, ObjectRef, QueryOutcome,
    ResultColumn, Result, RowChange, ScriptError, Session, TableSchema,
};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";
/// Asked for on every request unless the user's profile has `readonly = 1`,
/// which refuses any setting that differs from the profile's. The JSON
/// number quoting (`output_format_json_quote_*`) is left to the server, so
/// every user sees values the same way; `split_row` reads numbers from their
/// text, quoted or not, without losing digits.
const OUTPUT_SETTINGS: [(&str, &str); 1] = [("http_write_exception_in_output_format", "0")];
/// `READONLY`: "Cannot modify '…' setting in readonly mode".
const READONLY: u32 = 164;
const DICTIONARY: &str = "dictionary";
const SESSION_IS_LOCKED: u32 = 373;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    ClickHouse,
    Timeplus,
}

impl Flavor {
    /// Response headers are `X-ClickHouse-*` or `X-Timeplus-*`.
    fn header(self, name: &str) -> String {
        match self {
            Flavor::ClickHouse => format!("x-clickhouse-{name}"),
            Flavor::Timeplus => format!("x-timeplus-{name}"),
        }
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(ClickHouseDriver { info: info(Flavor::ClickHouse), flavor: Flavor::ClickHouse }),
        Arc::new(ClickHouseDriver { info: info(Flavor::Timeplus), flavor: Flavor::Timeplus }),
    ]
}

fn info(flavor: Flavor) -> DriverInfo {
    let mut fields = Field::server_set();
    fields[1] = Field::port().placeholder("8123").help("HTTP: 8123; con TLS, 8443.");
    fields[3] = Field::username().placeholder("default");
    let (id, name, family, object_kinds) = match flavor {
        Flavor::ClickHouse => (
            "clickhouse",
            "ClickHouse",
            Family::Analytical,
            vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::materialized_views(),
                ObjectKindInfo::new(DICTIONARY, "Diccionarios", true, true, true),
                ObjectKindInfo::functions(),
            ],
        ),
        Flavor::Timeplus => {
            fields[1] = Field::port().placeholder("8123").help("Consultas por lotes: 8123. El puerto 3218 corre consultas de streaming.");
            (
                "timeplus",
                "Timeplus Proton",
                Family::Streaming,
                vec![
                    ObjectKindInfo::new(kinds::STREAM, "Streams", true, true, true),
                    ObjectKindInfo::tables(),
                    ObjectKindInfo::views(),
                    ObjectKindInfo::materialized_views(),
                    ObjectKindInfo::new(DICTIONARY, "Diccionarios", true, true, true),
                    ObjectKindInfo::functions(),
                ],
            )
        }
    };
    DriverInfo {
        id,
        name,
        family,
        language: Language::Sql,
        dialect: "clickhouse",
        default_port: 8123,
        fields,
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds,
    }
}

pub struct ClickHouseDriver {
    info: DriverInfo,
    flavor: Flavor,
}

#[async_trait]
impl Driver for ClickHouseDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One statement per request in the tab's `session_id`, which keeps
    /// `SET`s and temporary tables. Every request names its database, which
    /// overrides the session's: `USE` is followed by the session itself
    /// (see `execute`).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// clickhouse-client --multiquery stops at the first error (unless
    /// `--ignore-error`).
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: false, confirm_unsafe_dml: true }
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

    fn supports_profiler(&self) -> bool {
        true
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec(self.flavor))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        backup::spec(self.flavor)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        if self.flavor != Flavor::ClickHouse {
            return Err(Error::Unsupported("Timeplus Proton no tiene backups propios que se puedan restaurar".into()));
        }
        backup::script(action)
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(schema::designer(self.flavor))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        schema::templates(self.flavor)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(schema::table_ddl(self.flavor, table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// The sorting key, skip indexes and projections with their sizes; no
    /// usage counters (see [`index_usage`]).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(self.flavor, changes)
    }

    /// `INSERT … FORMAT RowBinary` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// RowBinary piped from one server to another of the same engine.
    fn supports_native_copy(&self, target: &str) -> bool {
        target == self.info.id
    }

    async fn copy_native(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        spec: &dbine_driver::CopySpec,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::copy_native(source, target, spec, progress).await
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(schema::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(schema::update_script(self.flavor, target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        Ok(schema::delete_script(self.flavor, target.schema(), &target.name, keys))
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        schema::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let port = cfg.port_or(if cfg.encrypt { 8443 } else { 8123 });
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let database = database
            .filter(|d| !d.is_empty())
            .or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()))
            .unwrap_or("default")
            .to_string();
        let mut s = ClickHouseSession {
            conn: Arc::new(Conn {
                http,
                url: format!("{scheme}://{host}:{port}/"),
                user: cfg.username.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| "default".into()),
                password: cfg.password.clone().unwrap_or_default(),
                locked_settings: AtomicBool::new(false),
                server_readonly: AtomicBool::new(false),
            }),
            flavor: self.flavor,
            database,
            read_only: cfg.read_only,
            session_id: uuid::Uuid::new_v4().to_string(),
            in_flight: Arc::new(Mutex::new(None)),
            rt: tokio::runtime::Handle::current(),
            profiler: None,
        };
        tokio::time::timeout(Duration::from_secs(20), s.server_version())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(s))
    }
}

struct Conn {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
    /// The server refused `OUTPUT_SETTINGS` (a `readonly = 1` profile):
    /// found on the first request and remembered for the connection.
    locked_settings: AtomicBool,
    /// The server refused `readonly = 1` from DBine's read-only mode and the
    /// profile's own `readonly` (2) was checked to refuse every write.
    server_readonly: AtomicBool,
}

impl Conn {
    fn post(&self) -> reqwest::RequestBuilder {
        self.http.post(&self.url).basic_auth(&self.user, Some(&self.password))
    }
}

pub struct ClickHouseSession {
    conn: Arc<Conn>,
    flavor: Flavor,
    database: String,
    read_only: bool,
    session_id: String,
    /// `query_id` of the statement running now.
    in_flight: Arc<Mutex<Option<String>>>,
    rt: tokio::runtime::Handle,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

/// What one statement returned.
enum Body {
    /// No result set (DDL, INSERT…): rows written.
    Written(u64),
    Rows(reqwest::Response),
    /// The statement chose its own `FORMAT`: shown as text lines.
    Text(reqwest::Response),
}

impl ClickHouseSession {
    /// Send one statement. `params` are ClickHouse query parameters
    /// (`{name:String}` placeholders), so catalog lookups never splice
    /// user text into SQL.
    async fn send(&self, sql: &str, params: &[(&str, &str)], in_session: bool) -> Result<Body> {
        let query_id = uuid::Uuid::new_v4().to_string();
        let mut q: Vec<(String, String)> = vec![
            ("database".into(), self.database.clone()),
            ("query_id".into(), query_id.clone()),
            ("default_format".into(), FORMAT.into()),
        ];
        if in_session {
            q.push(("session_id".into(), self.session_id.clone()));
            q.push(("session_timeout".into(), "3600".into()));
        }
        q.extend(params.iter().map(|(k, v)| (format!("param_{k}"), v.to_string())));
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()) = Some(query_id);
        // Proton spells types in lower case.
        let sql = match self.flavor {
            Flavor::Timeplus if !params.is_empty() => sql.replace(":String}", ":string}"),
            _ => sql.to_string(),
        };
        // A stopped or cancelled query keeps the session locked until the
        // server notices; wait for it a little.
        let mut attempts = 0;
        let resp = loop {
            let locked = self.conn.locked_settings.load(Ordering::Relaxed);
            let mut settings: Vec<(&str, &str)> = if locked { Vec::new() } else { OUTPUT_SETTINGS.to_vec() };
            let readonly = self.sends_readonly();
            if readonly {
                settings.push(("readonly", "1"));
            }
            let resp = self.conn.post().query(&q).query(&settings).body(sql.clone()).send().await.map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    Error::Connect(e.to_string())
                } else {
                    Error::Query(e.to_string())
                }
            })?;
            if resp.status().is_success() {
                break resp;
            }
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            match refused_setting(&text) {
                Some(Refused::Output) if !locked => {
                    tracing::debug!("clickhouse: readonly profile, sending no output settings");
                    self.conn.locked_settings.store(true, Ordering::Relaxed);
                    continue;
                }
                // The refusal may come from the user's own statement
                // (`SETTINGS readonly = 0`): stop sending `readonly` only
                // when the profile itself is read-only.
                Some(Refused::ReadOnly) if readonly && self.profile_readonly().await > 0 => {
                    tracing::debug!("clickhouse: readonly profile, not sending readonly");
                    self.conn.server_readonly.store(true, Ordering::Relaxed);
                    continue;
                }
                _ => {}
            }
            if in_session && exception_code(&text) == Some(SESSION_IS_LOCKED) && attempts < 25 {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            return Err(server_error(status.as_u16(), text.trim()));
        };
        let format = resp.headers().get(self.flavor.header("format")).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(match format.as_deref() {
            Some(FORMAT) => Body::Rows(resp),
            Some(_) => Body::Text(resp),
            None => {
                let written = resp
                    .headers()
                    .get(self.flavor.header("summary"))
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| v.get("written_rows")?.as_str()?.parse().ok())
                    .unwrap_or(0);
                // Drain (an error can still come in the body).
                let text = resp.text().await.map_err(Error::query)?;
                if let Some(e) = body_exception(&text) {
                    return Err(e);
                }
                Body::Written(written)
            }
        })
    }

    /// The user's profile's own `readonly`, asked without any setting (0
    /// when it can't be read, so DBine keeps sending its own).
    async fn profile_readonly(&self) -> i64 {
        let resp = match self.conn.post().body("SELECT getSetting('readonly')").send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return 0,
        };
        resp.text().await.ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
    }

    /// Whether requests carry `readonly = 1` (DBine's read-only mode, unless
    /// the user's profile is already read-only and refuses it).
    fn sends_readonly(&self) -> bool {
        self.read_only && !self.conn.server_readonly.load(Ordering::Relaxed)
    }

    /// Three catalog queries: tables (with engine clauses, and the CREATE
    /// statement for CHECK / ASSUME constraints and projections), columns
    /// and data-skipping indexes; `only`: just that table.
    pub(crate) async fn catalog(&self, only: Option<&str>) -> Result<Vec<TableSchema>> {
        let db = self.database.clone();
        let mut params: Vec<(&str, &str)> = vec![("db", &db)];
        let (only_table, only_column) = match only {
            Some(t) => {
                params.push(("t", t));
                (" AND name = {t:String}", " AND table = {t:String}")
            }
            None => ("", ""),
        };
        let engines = match self.flavor {
            Flavor::ClickHouse => "engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView', 'Dictionary')",
            Flavor::Timeplus => "engine IN ('Stream', 'MergeTree')",
        };
        let tables = self
            .rows(
                &format!(
                    "SELECT name, engine, engine_full, comment, sorting_key, primary_key, partition_key, sampling_key,
                            create_table_query
                     FROM system.tables
                     WHERE database = {{db:String}} AND NOT is_temporary AND name NOT LIKE '.inner%' AND {engines}{only_table}
                     ORDER BY name"
                ),
                &params,
            )
            .await?;
        let columns = self
            .rows(
                &format!(
                    "SELECT table, name, type, default_kind, default_expression, comment, compression_codec
                     FROM system.columns WHERE database = {{db:String}}{only_column} ORDER BY table, position"
                ),
                &params,
            )
            .await?;
        // Timeplus has no `type_full` (and its `type` drops the arguments).
        let index_type = match self.flavor {
            Flavor::ClickHouse => "type_full",
            Flavor::Timeplus => "type",
        };
        let indexes = self
            .rows(
                &format!(
                    "SELECT table, name, {index_type}, expr, granularity
                     FROM system.data_skipping_indices WHERE database = {{db:String}}{only_column} ORDER BY table, name"
                ),
                &params,
            )
            .await
            .unwrap_or_default();
        Ok(schema::assemble(self.flavor, &db, schema::Catalog { tables, columns, indexes }))
    }

    /// Run a catalog query and collect its rows (at most 100 000).
    async fn rows(&self, sql: &str, params: &[(&str, &str)]) -> Result<Vec<Vec<Value>>> {
        let mut out = QueryOutcome::default();
        if let Body::Rows(resp) = self.send(sql, params, false).await? {
            read_rows(resp, &mut out, 100_000, false).await?;
        }
        self.done();
        Ok(out.results.pop().map(|r| r.rows).unwrap_or_default())
    }

    fn done(&self) {
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// clickhouse-client's lexer: backslash escapes in strings, heredocs
/// (`$tag$ … $tag$`), `` `name` ``, nested `/* /* */ */` and `#` line
/// comments; no procedural bodies.
fn dialect() -> ScriptDialect {
    ScriptDialect {
        backslash_escapes: true,
        dollar_quotes: true,
        nested_comments: true,
        hash_comments: true,
        compound_blocks: false,
        ..ScriptDialect::generic()
    }
}

/// The database of a `USE db` statement (`` `db` `` and `"db"` unquoted).
fn use_target(stmt: &str) -> Option<String> {
    let d = dialect();
    let s = strip_comments(stmt, &d, false);
    let s = s.trim().trim_end_matches(';').trim();
    let (kw, rest) = s.split_at(s.find(char::is_whitespace)?);
    if !kw.eq_ignore_ascii_case("use") {
        return None;
    }
    let name = rest.trim();
    let unquoted = match name.chars().next()? {
        q @ ('`' | '"') if name.len() >= 2 && name.ends_with(q) => {
            let inner = &name[1..name.len() - 1];
            let mut out = String::new();
            let mut chars = inner.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => out.extend(chars.next()),
                    c if c == q && chars.peek() == Some(&q) => {
                        chars.next();
                        out.push(q);
                    }
                    c => out.push(c),
                }
            }
            out
        }
        _ if name.chars().all(|c| c.is_alphanumeric() || c == '_') => name.to_string(),
        _ => return None,
    };
    (!unquoted.is_empty()).then_some(unquoted)
}

/// The statements of a script, with their byte offsets.
fn statements(sql: &str) -> Vec<(String, usize)> {
    split_script(sql, &dialect()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand).map(|s| (s.text, s.start)).collect()
}

/// A statement of `script` (at byte `start`) failed: ClickHouse's error
/// code, and where (`failed at position N`, 1-based, in the statement).
fn stmt_err(e: Error, script: &str, start: usize) -> Error {
    let Error::Query(msg) = e else { return e };
    let Some(code) = exception_code(&msg) else { return Error::Query(msg) };
    let mut se = ScriptError::new(msg.clone()).with_code(code.to_string());
    let position = msg.split("failed at position ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse::<usize>().ok());
    let start = start.min(script.len());
    if let Some(p) = position.filter(|p| *p >= 1) {
        let mut at = (start + p - 1).min(script.len());
        while !script.is_char_boundary(at) {
            at -= 1;
        }
        se = se.at_offset(at).at_line(script[..at].matches('\n').count() as u32 + 1);
    } else {
        se = se.at_line(script[..start].matches('\n').count() as u32 + 1);
    }
    se.into()
}

/// `Code: 516. DB::Exception: …` → the right error kind.
fn server_error(status: u16, text: &str) -> Error {
    let code = exception_code(text);
    let msg = if text.is_empty() { format!("HTTP {status}") } else { text.to_string() };
    match (status, code) {
        (_, Some(394)) => Error::Cancelled,
        (401 | 403, _) | (_, Some(192 | 193 | 194 | 516)) => Error::AuthFailed(msg),
        _ => Error::Query(msg),
    }
}

/// A setting the user's read-only profile refused.
#[derive(Debug, PartialEq)]
enum Refused {
    /// One of `OUTPUT_SETTINGS` (`readonly = 1`).
    Output,
    /// `readonly` itself (the profile has `readonly = 2`).
    ReadOnly,
}

fn refused_setting(text: &str) -> Option<Refused> {
    if exception_code(text) != Some(READONLY) {
        return None;
    }
    if OUTPUT_SETTINGS.iter().any(|(k, _)| text.contains(&format!("'{k}'"))) {
        Some(Refused::Output)
    } else if text.contains("'readonly'") {
        Some(Refused::ReadOnly)
    } else {
        None
    }
}

fn exception_code(text: &str) -> Option<u32> {
    let rest = &text[text.find("Code: ")? + 6..];
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// An exception the server wrote into a 200 response after it had started
/// streaming: a `__exception__` block (recent versions) or a bare
/// `Code: N. DB::Exception` line (older ones).
fn body_exception(text: &str) -> Option<Error> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.contains("__exception__") || (t.contains("Code: ") && t.contains("Exception")) {
        let msg = t
            .lines()
            .find(|l| l.contains("Code: "))
            .map(str::trim)
            .unwrap_or(t)
            .to_string();
        return Some(server_error(200, &msg));
    }
    None
}

/// Lines of a streamed body, read chunk by chunk.
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
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).to_string();
                return Ok(Some(line));
            }
            match self.resp.chunk().await.map_err(|e| Error::Query(e.to_string()))? {
                Some(c) => self.buf.extend_from_slice(&c),
                None => self.eof = true,
            }
        }
    }

    async fn rest(mut self) -> String {
        let mut s = String::from_utf8_lossy(&self.buf).to_string();
        while let Ok(Some(c)) = self.resp.chunk().await {
            s.push_str(&String::from_utf8_lossy(&c));
        }
        s
    }
}

/// Read a `JSONCompactEachRowWithNamesAndTypes` body into a new result set.
/// `stop_at_limit` stops reading once past `max_rows` (streaming queries
/// never end on their own); otherwise the rest is counted, not kept.
/// Returns whether it stopped early.
async fn read_rows(resp: reqwest::Response, out: &mut QueryOutcome, max_rows: usize, stop_at_limit: bool) -> Result<bool> {
    let mut lines = Lines::new(resp);
    let mut header: Vec<Vec<String>> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    while let Some(line) = lines.next().await? {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = if line.trim() == "__exception__" { None } else { split_row(&line) };
        let Some(cells) = parsed else {
            let rest = format!("{line}\n{}", lines.rest().await);
            return Err(body_exception(&rest).unwrap_or_else(|| Error::Query(format!("respuesta inesperada: {}", rest.trim()))));
        };
        if header.len() < 2 {
            header.push(
                cells
                    .iter()
                    .map(|raw| match serde_json::from_str(raw) {
                        Ok(Value::String(s)) => s,
                        _ => String::new(),
                    })
                    .collect(),
            );
            if header.len() == 2 {
                types = header[1].clone();
                out.begin_result(
                    header[0].iter().zip(&types).map(|(n, t)| ResultColumn { name: n.clone(), type_name: t.clone() }).collect(),
                );
            }
            continue;
        }
        let row = cells.into_iter().enumerate().map(|(i, raw)| raw_cell(raw, types.get(i).map_or("", String::as_str))).collect();
        out.push_row(row, max_rows);
        if stop_at_limit && out.results.last().is_some_and(|r| r.truncated) {
            return Ok(true);
        }
    }
    if header.len() < 2 {
        out.begin_result(Vec::new());
    }
    Ok(false)
}

/// A text body (the statement picked its own `FORMAT`): one row per line.
async fn read_text(resp: reqwest::Response, out: &mut QueryOutcome, max_rows: usize) -> Result<()> {
    let mut lines = Lines::new(resp);
    out.begin_result(vec![ResultColumn { name: "result".into(), type_name: String::new() }]);
    while let Some(line) = lines.next().await? {
        if line.trim() == "__exception__" {
            let rest = lines.rest().await;
            return Err(body_exception(&format!("__exception__\n{rest}")).unwrap_or_else(|| Error::Query(rest)));
        }
        out.push_row(vec![line.into()], max_rows);
    }
    Ok(())
}

/// `Nullable(LowCardinality(Int64))` → `int64`.
fn base_type(t: &str) -> String {
    let mut t = t.trim();
    loop {
        let lower = t.to_ascii_lowercase();
        let inner = ["nullable(", "lowcardinality("].iter().find_map(|w| lower.starts_with(w).then_some(w.len()));
        match inner {
            Some(n) if t.ends_with(')') => t = &t[n..t.len() - 1],
            _ => return lower,
        }
    }
}

/// `Nullable(T)` or `LowCardinality(Nullable(T))`.
fn is_nullable(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    let t = t.strip_prefix("lowcardinality(").unwrap_or(&t);
    t.starts_with("nullable(")
}

/// The elements of a JSON array line, as raw text. Numbers are read from
/// their text: unless the server quotes them, 64-bit (and wider) integers and
/// decimals come unquoted, and serde_json would read them as f64.
fn split_row(line: &str) -> Option<Vec<&str>> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc, mut start) = (0usize, false, false, 0);
    for (i, b) in inner.bytes().enumerate() {
        if in_str {
            match b {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => {
                out.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if in_str || depth != 0 || (last.is_empty() && !out.is_empty()) || out.iter().any(|e| e.is_empty()) {
        return None;
    }
    if !last.is_empty() {
        out.push(last);
    }
    Some(out)
}

/// One raw JSON cell (see `split_row`) as the UI wants it.
fn raw_cell(raw: &str, ty: &str) -> Value {
    if raw.starts_with(|c: char| c == '-' || c.is_ascii_digit()) {
        let t = base_type(ty);
        if t.starts_with("float") || t.starts_with("bfloat") {
            return serde_json::from_str(raw).unwrap_or_else(|_| raw.into());
        }
        // Decimals stay text, as with `output_format_json_quote_decimals`.
        if !t.starts_with("decimal") {
            if let Ok(i) = raw.parse::<i64>() {
                return json_i64(i);
            }
            if let Ok(u) = raw.parse::<u64>() {
                return json_u64(u);
            }
        }
        return raw.into();
    }
    match serde_json::from_str::<Value>(raw) {
        // Arrays, maps and tuples are shown as text: the server's own when
        // a number in it would lose digits as f64.
        Ok(Value::Array(_) | Value::Object(_)) if !numbers_exact(raw) => raw.into(),
        Ok(v) => cell(v, ty),
        Err(_) => raw.into(),
    }
}

/// Every number in `raw` (outside strings) is an integer that fits 64 bits.
fn numbers_exact(raw: &str) -> bool {
    let (mut in_str, mut esc, mut token) = (false, false, String::new());
    for c in raw.chars().chain([' ']) {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        if c.is_ascii_digit() || c == '-' || (!token.is_empty() && matches!(c, '+' | '.' | 'e' | 'E')) {
            token.push(c);
            continue;
        }
        if !token.is_empty() {
            if token.parse::<i64>().is_err() && token.parse::<u64>().is_err() {
                return false;
            }
            token.clear();
        }
        in_str = c == '"';
    }
    true
}

/// A JSON cell as the UI wants it, given its ClickHouse type.
fn cell(v: Value, ty: &str) -> Value {
    match v {
        Value::String(s) => {
            let t = base_type(ty);
            if t.starts_with("int") || t.starts_with("uint") {
                if let Ok(i) = s.parse::<i64>() {
                    return json_i64(i);
                }
                if let Ok(u) = s.parse::<u64>() {
                    return json_u64(u);
                }
            }
            Value::String(s)
        }
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v,
    }
}

fn kind_of(engine: &str, flavor: Flavor) -> &'static str {
    match engine {
        "View" | "LiveView" | "WindowView" => kinds::VIEW,
        "MaterializedView" => kinds::MATERIALIZED_VIEW,
        "Dictionary" => DICTIONARY,
        "Stream" | "ExternalStream" | "Random" if flavor == Flavor::Timeplus => kinds::STREAM,
        _ => kinds::TABLE,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

#[async_trait]
impl Session for ClickHouseSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.rows("SELECT version()", &[]).await?;
        let v = rows.first().and_then(|r| r.first()).map(text).unwrap_or_default();
        Ok(match self.flavor {
            Flavor::ClickHouse => format!("ClickHouse {v}"),
            Flavor::Timeplus => format!("Timeplus Proton {v}"),
        })
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    /// The queries running now (`system.processes`): ClickHouse has no
    /// sessions to list.
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

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self
            .rows(
                "SELECT name FROM system.databases
                 WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| r.first()).map(text).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let db = self.database.clone();
        let rows = self
            .rows(
                "SELECT name, engine FROM system.tables
                 WHERE database = {db:String} AND NOT is_temporary AND name NOT LIKE '.inner%'
                 ORDER BY name",
                &[("db", &db)],
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .iter()
            .map(|r| DbObject {
                kind: kind_of(&text(&r[1]), self.flavor).to_string(),
                schema: None,
                name: text(&r[0]),
                parent: None,
            })
            .collect();
        // SQL user-defined functions are global, not per database.
        if let Ok(funcs) = self.rows("SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name", &[]).await {
            out.extend(funcs.iter().map(|r| DbObject {
                kind: kinds::FUNCTION.into(),
                schema: None,
                name: text(&r[0]),
                parent: None,
            }));
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let db = obj.schema().unwrap_or(&self.database).to_string();
        let rows = self
            .rows(
                "SELECT name, type, default_kind, default_expression, is_in_primary_key
                 FROM system.columns WHERE database = {db:String} AND table = {t:String} ORDER BY position",
                &[("db", &db), ("t", &obj.name)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let data_type = text(&r[1]);
                let (kind, expr) = (text(&r[2]), text(&r[3]));
                ColumnInfo {
                    name: text(&r[0]),
                    nullable: is_nullable(&data_type),
                    data_type,
                    primary_key: matches!(r[4], Value::Number(ref n) if n.as_u64() == Some(1)) || text(&r[4]) == "1",
                    auto_increment: false,
                    default_value: (!expr.is_empty()).then(|| if kind == "DEFAULT" || kind.is_empty() { expr } else { format!("{kind} {expr}") }),
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if obj.kind == kinds::FUNCTION {
            let rows = self
                .rows("SELECT create_query FROM system.functions WHERE name = {n:String}", &[("n", &obj.name)])
                .await?;
            return Ok(rows.first().and_then(|r| r.first()).map(text));
        }
        let db = obj.schema().unwrap_or(&self.database).to_string();
        let rows = self
            .rows(
                "SELECT create_table_query FROM system.tables WHERE database = {db:String} AND name = {t:String}",
                &[("db", &db), ("t", &obj.name)],
            )
            .await?;
        Ok(rows.first().and_then(|r| r.first()).map(text).filter(|s| !s.is_empty()))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        if self.flavor == Flavor::Timeplus && obj.kind == kinds::STREAM {
            // table() reads what the stream holds instead of waiting for new events.
            return format!("SELECT *\nFROM table({})\nLIMIT {limit}", quote_ident(Quote::Backtick, &obj.name));
        }
        select_top(Quote::Backtick, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let res = async {
            for (stmt, start) in statements(text) {
                let one = async {
                    match self.send(&stmt, &[], true).await? {
                        Body::Written(n) => out.push_affected(n),
                        Body::Text(resp) => read_text(resp, out, max_rows).await?,
                        Body::Rows(resp) => {
                            let stop = self.flavor == Flavor::Timeplus;
                            if read_rows(resp, out, max_rows, stop).await? {
                                // A streaming query: stop it on the server.
                                if let Some(f) = self.interrupter() {
                                    f();
                                }
                                out.info(format!("Se detuvo la consulta al llegar a {max_rows} filas."));
                            }
                        }
                    }
                    Ok::<(), Error>(())
                };
                one.await.map_err(|e| stmt_err(e, text, start))?;
                self.done();
                // Requests carry `database=`, which would undo the USE on
                // the next one: the session follows it instead, and so
                // does the tab.
                if let Some(db) = use_target(&stmt) {
                    self.database = db.clone();
                    out.database = Some(db);
                }
            }
            Ok(())
        }
        .await;
        self.done();
        res
    }

    /// `EXPLAIN json = 1, indexes = 1` per SELECT; ClickHouse explains
    /// nothing else as JSON, so other statements get no plan. It has no
    /// analyzing EXPLAIN either: with `analyze` the script runs as with
    /// `execute` and the plans are the estimated ones.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if analyze {
            out.messages.push("ClickHouse no da cifras reales por operador: se muestran los planes estimados.".into());
        }
        let res = async {
            let d = dialect();
            for stmt in statements(sql).into_iter().map(|(s, _)| strip_comments(&s, &d, false).trim().to_string()).filter(|s| !s.is_empty()) {
                if plan::explainable(&stmt) {
                    let q = format!("EXPLAIN json = 1, indexes = 1, description = 1 {stmt}");
                    let raw = match self.send(&q, &[], true).await? {
                        Body::Rows(resp) => {
                            let mut local = QueryOutcome::default();
                            read_rows(resp, &mut local, 1, false).await?;
                            local.results.pop().and_then(|r| r.rows.into_iter().next()).and_then(|r| r.into_iter().next())
                        }
                        _ => None,
                    };
                    self.done();
                    let raw = raw.map(|v| text(&v)).ok_or_else(|| Error::Query("EXPLAIN no devolvió un plan".into()))?;
                    out.plans.push(plan::plan_json(&stmt, &raw).map_err(Error::Query)?);
                } else if !analyze {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                }
                if analyze {
                    self.execute(&stmt, max_rows, out).await?;
                }
            }
            Ok(())
        }
        .await;
        self.done();
        res
    }

    /// See [`ClickHouseSession::catalog`].
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        self.catalog(None).await
    }

    /// The table's skip indexes, projections and sorting key (see [`index_usage`]).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        index_usage::report(self, table).await.map(Some)
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        self.backup_history(database).await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        let sql = format!("CREATE DATABASE {}", quote_ident(Quote::Backtick, name.trim()));
        self.send(&sql, &[], false).await?;
        self.done();
        Ok(())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if name == self.database {
            return Err(Error::Query("no se puede borrar la base de esta sesión".into()));
        }
        let sql = format!("DROP DATABASE {}", quote_ident(Quote::Backtick, name));
        self.send(&sql, &[], false).await?;
        self.done();
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let conn = self.conn.clone();
        let in_flight = self.in_flight.clone();
        let rt = self.rt.clone();
        Some(Arc::new(move || {
            let Some(id) = in_flight.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                // The id is a UUID we generated: safe to inline.
                let sql = format!("KILL QUERY WHERE query_id = '{id}' ASYNC");
                if let Err(e) = conn.post().body(sql).send().await {
                    tracing::debug!("clickhouse kill query failed: {e}");
                }
            });
        }))
    }

    /// `CHECK GRANT` per action (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn use_statements_name_their_database() {
        assert_eq!(use_target("USE wv_db2").as_deref(), Some("wv_db2"));
        assert_eq!(use_target("-- go\nuse `my db`;").as_deref(), Some("my db"));
        assert_eq!(use_target("USE \"a\"\"b\"").as_deref(), Some("a\"b"));
        assert_eq!(use_target("USE `a\\`b`").as_deref(), Some("a`b"));
        assert_eq!(use_target("SELECT 1"), None);
        assert_eq!(use_target("user_function()"), None);
        assert_eq!(use_target("/* a /* b */ c */ USE x # note").as_deref(), Some("x"));
    }

    /// clickhouse-client nests block comments and takes `#` as a line
    /// comment: a `;` inside either doesn't split.
    #[test]
    fn nested_and_hash_comments_hold_semicolons() {
        let d = dialect();
        let units = split_script("/* outer /* nested ; */ c ; */ SELECT 1;\nSELECT 1 # trailing ; comment\n;SELECT 2", &d);
        let texts: Vec<String> = units.iter().map(|u| strip_comments(&u.text, &d, false).trim().to_string()).collect();
        assert_eq!(texts, ["SELECT 1", "SELECT 1", "SELECT 2"]);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn cells_follow_their_types() {
        assert_eq!(cell(json!("42"), "Nullable(Int64)"), json!(42));
        assert_eq!(cell(json!("18446744073709551615"), "UInt64"), json!("18446744073709551615"));
        assert_eq!(cell(json!("1.50"), "Decimal(10, 2)"), json!("1.50"));
        assert_eq!(cell(json!([1, 2]), "Array(UInt8)"), json!("[1,2]"));
        assert_eq!(cell(json!({"k": 1}), "Map(String, UInt8)"), json!("{\"k\":1}"));
        assert_eq!(cell(json!("2024-01-31 13:45:00"), "DateTime"), json!("2024-01-31 13:45:00"));
        assert_eq!(cell(json!("7"), "int64"), json!(7));
    }

    #[test]
    fn unquoted_numbers_keep_their_digits() {
        let line = r#"["1", 9007199254740993, 12345678901234567890.123, 18446744073709551615, 170141183460469231731687303715884105727, [170141183460469231731687303715884105727], {"a\/b,]":1}, 1.5, null, true, "x\"],"]"#;
        let types = [
            "String", "Int64", "Decimal(38, 3)", "UInt64", "Int128", "Array(Int128)", "Map(String, UInt8)", "Float64",
            "Nullable(Int64)", "Bool", "String",
        ];
        let cells: Vec<Value> = split_row(line).unwrap().into_iter().zip(types).map(|(r, t)| raw_cell(r, t)).collect();
        assert_eq!(
            cells,
            vec![
                json!("1"),
                json!("9007199254740993"),
                json!("12345678901234567890.123"),
                json!("18446744073709551615"),
                json!("170141183460469231731687303715884105727"),
                json!("[170141183460469231731687303715884105727]"),
                json!("{\"a/b,]\":1}"),
                json!(1.5),
                Value::Null,
                json!(true),
                json!("x\"],"),
            ]
        );
        // Quoted (`output_format_json_quote_*` on) reads the same.
        let quoted = split_row(r#"["9007199254740993", "1.50", ["1", "2"], 42]"#).unwrap();
        let types = ["Int64", "Decimal(10, 2)", "Array(Int64)", "Int32"];
        let cells: Vec<Value> = quoted.into_iter().zip(types).map(|(r, t)| raw_cell(r, t)).collect();
        assert_eq!(cells, vec![json!("9007199254740993"), json!("1.50"), json!("[\"1\",\"2\"]"), json!(42)]);
        assert_eq!(split_row("[]"), Some(vec![]));
        for bad in ["__exception__", "Code: 395. DB::Exception: boom", "[1,]", "[1", r#"["a]"#, "{\"exception\": \"x\"}"] {
            assert_eq!(split_row(bad), None, "{bad}");
        }
    }

    #[test]
    fn readonly_refusals_are_recognized() {
        let refused = |s: &str| refused_setting(&format!("Code: 164. DB::Exception: Cannot modify '{s}' setting in readonly mode. (READONLY)"));
        assert_eq!(refused("http_write_exception_in_output_format"), Some(Refused::Output));
        assert_eq!(refused("readonly"), Some(Refused::ReadOnly));
        assert_eq!(refused_setting("Code: 164. DB::Exception: ro_user: Cannot execute query in readonly mode. (READONLY)"), None);
    }

    #[test]
    fn base_type_unwraps_wrappers() {
        assert_eq!(base_type("Nullable(LowCardinality(String))"), "string");
        assert_eq!(base_type("Array(Nullable(Int8))"), "array(nullable(int8))");
    }

    #[test]
    fn scripts_split_like_clickhouse_client() {
        let s = statements("SELECT 'it\\'s; ok';\nSELECT $h$a;b$h$; SELECT `a;b` FROM t;\n-- only a comment;\n");
        assert_eq!(s.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["SELECT 'it\\'s; ok'", "SELECT $h$a;b$h$", "SELECT `a;b` FROM t"]);
        // No procedural bodies: `begin` is just a word.
        assert_eq!(statements("create table event (id Int8, begin Int8) engine = Memory; insert into event values (1, 2)").len(), 2);
    }

    #[test]
    fn errors_carry_code_and_position() {
        let script = "SELECT 1;\nSELEC 2";
        let msg = "Code: 62. DB::Exception: Syntax error: failed at position 1 ('SELEC') (line 1, col 1): SELEC 2. Expected one of: … (SYNTAX_ERROR) (version 24.8.4.13 (official build))";
        let e = stmt_err(Error::Query(msg.into()), script, 10).to_script_error();
        assert_eq!((e.code.as_deref(), e.offset, e.line), (Some("62"), Some(10), Some(2)));
        let e = stmt_err(Error::Query("Code: 60. DB::Exception: Unknown table expression identifier 'nope'. (UNKNOWN_TABLE)".into()), script, 10);
        assert_eq!(e.to_script_error().line, Some(2));
        assert!(matches!(stmt_err(Error::Cancelled, script, 0), Error::Cancelled));
    }

    #[test]
    fn exceptions_are_classified() {
        assert!(matches!(server_error(500, "Code: 60. DB::Exception: Table x doesn't exist"), Error::Query(_)));
        assert!(matches!(server_error(403, "Code: 516. DB::Exception: dbine: Authentication failed"), Error::AuthFailed(_)));
        assert!(matches!(server_error(500, "Code: 394. DB::Exception: Query was cancelled"), Error::Cancelled));
        let e = body_exception("\r\n__exception__\r\nabc\r\nCode: 395. DB::Exception: boom\n288 abc\r\n__exception__\r\n");
        assert!(matches!(e, Some(Error::Query(m)) if m.starts_with("Code: 395")));
        assert!(body_exception("").is_none());
    }

    #[test]
    fn engines_map_to_kinds() {
        assert_eq!(kind_of("MergeTree", Flavor::ClickHouse), kinds::TABLE);
        assert_eq!(kind_of("MaterializedView", Flavor::ClickHouse), kinds::MATERIALIZED_VIEW);
        assert_eq!(kind_of("Dictionary", Flavor::ClickHouse), DICTIONARY);
        assert_eq!(kind_of("Stream", Flavor::Timeplus), kinds::STREAM);
    }
}

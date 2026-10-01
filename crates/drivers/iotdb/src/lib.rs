//! Apache IoTDB through its REST API v2 (`/rest/v2/query` for reads,
//! `/rest/v2/nonQuery` for the rest), with basic auth. Databases are the
//! storage groups (`root.x`), objects their devices, and a device's
//! columns its time series. Answers come column-major; they're turned
//! into rows here.

use dbine_driver::{
    async_trait, json_f64, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig,
    CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, KeyDef,
    Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, RowChange, Session,
    TableSchema,
};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Object kind: a device (the level that holds time series).
const DEVICE: &str = "device";

/// Data types of IoTDB 1.x (STRING, BLOB, DATE and TIMESTAMP need 1.3.3+).
const DATA_TYPES: &[&str] = &["BOOLEAN", "INT32", "INT64", "FLOAT", "DOUBLE", "TEXT", "STRING", "BLOB", "DATE", "TIMESTAMP"];
const ENCODINGS: &[&str] =
    &["PLAIN", "RLE", "TS_2DIFF", "GORILLA", "DICTIONARY", "ZIGZAG", "CHIMP", "SPRINTZ", "RLBE"];
const COMPRESSIONS: &[&str] = &["UNCOMPRESSED", "SNAPPY", "LZ4", "GZIP", "ZSTD", "LZMA2"];

/// IoTDB's answer when a result has more rows than `row_limit`.
const ROW_LIMIT_EXCEEDED: i64 = 708;

mod monitor;
mod permissions;
mod profiler;
mod script;
mod security;
mod sync;
mod transfer;

/// Apache IoTDB and TimechoDB (Timecho's commercial IoTDB: same REST API,
/// SQL and system views).
pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(IotDbDriver { info: info("iotdb", "Apache IoTDB"), product: "Apache IoTDB" }),
        Arc::new(IotDbDriver { info: info("timechodb", "TimechoDB"), product: "TimechoDB" }),
    ]
}

fn info(id: &'static str, name: &'static str) -> DriverInfo {
    DriverInfo {
        id,
        name,
        family: Family::TimeSeries,
        language: Language::Sql,
        dialect: "iotdb",
        default_port: 18080,
        fields: vec![
            Field::host().help("Servidor o URL del servicio REST (enable_rest_service=true)."),
            Field::port().placeholder("18080"),
            Field::new("database", "Base de datos", FieldKind::Text).placeholder("root.…"),
            Field::username().placeholder("root"),
            Field::password(),
            Field::new("metrics_url", "URL de métricas (Prometheus)", FieldKind::Text)
                .placeholder("http://servidor:9092/metrics")
                .help("Opcional, para el monitor: el endpoint del DataNode (dn_metric_reporter_list=PROMETHEUS). Vacío = se prueba el puerto 9092 del mismo servidor.")
                .advanced(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(DEVICE, "Dispositivos", true, true, false)],
    }
}

struct IotDbDriver {
    info: DriverInfo,
    product: &'static str,
}

#[async_trait]
impl Driver for IotDbDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// One request per statement, as the CLI's `-e`/file runs do: the REST
    /// API is stateless, so nothing is lost between statements.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// REST `insertTablet` in columnar tablets (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: false, monitor: true, ..Default::default() }
    }

    /// Time series of a device: data type, encoding and compression per
    /// measurement, aligned or not. No keys, defaults or indexes in IoTDB.
    fn designer(&self) -> Option<DesignerSpec> {
        let select = |key, label, values: &[&'static str]| {
            let mut opts = vec![("", "(por defecto)")];
            opts.extend(values.iter().map(|v| (*v, *v)));
            Field::new(key, label, FieldKind::Select(opts))
        };
        Some(DesignerSpec {
            kind: DEVICE,
            label: "Nuevo dispositivo",
            primary_key: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            indexes: false,
            foreign_keys: false,
            column_options: vec![
                select("encoding", "Codificación", ENCODINGS),
                select("compression", "Compresión", COMPRESSIONS),
            ],
            table_options: vec![Field::new("aligned", "Alineado", FieldKind::Bool)
                .help("Las series de un dispositivo alineado comparten la columna de tiempo (CREATE ALIGNED TIMESERIES).")],
            ..DesignerSpec::sql_table(DATA_TYPES.to_vec())
        })
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        vec![
            CreateTemplate {
                kind: "device_template",
                label: "Nueva plantilla de dispositivo",
                template: "CREATE DEVICE TEMPLATE {name} ALIGNED (\n    temperature FLOAT encoding=GORILLA compressor=LZ4,\n    \
                           status BOOLEAN encoding=PLAIN\n);\nSET DEVICE TEMPLATE {name} TO root.db.planta;"
                    .into(),
            },
            CreateTemplate {
                kind: "continuous_query",
                label: "Nueva consulta continua",
                template: "CREATE CONTINUOUS QUERY {name}\nRESAMPLE EVERY 1h\nBEGIN\n    SELECT avg(temperature)\n    \
                           INTO root.db.planta_1h(temperature_avg)\n    FROM root.db.planta\n    GROUP BY(1h)\nEND"
                    .into(),
            },
            CreateTemplate {
                kind: kinds::TRIGGER,
                label: "Nuevo trigger",
                template: "CREATE STATELESS TRIGGER {name}\nAFTER INSERT\nON root.db.**\nAS 'org.example.MiTrigger'\n\
                           USING URI 'https://example.com/triggers.jar'\nWITH (\"umbral\" = \"100\")"
                    .into(),
            },
            CreateTemplate {
                kind: kinds::FUNCTION,
                label: "Nueva función (UDF)",
                template: "CREATE FUNCTION {name} AS 'org.example.MiUdf'\nUSING URI 'https://example.com/udf.jar'".into(),
            },
        ]
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        table_ddl(table, parts)
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<J>]) -> Result<String> {
        insert_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, J)>]) -> Result<String> {
        delete_script(target, keys)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let db = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let username = cfg.username.as_deref().filter(|u| !u.is_empty()).unwrap_or("root").to_string();
        let mut s = IotDbSession {
            http,
            base: base_url(cfg),
            username,
            password: cfg.password_or_empty().to_string(),
            db: db.map(str::to_string),
            read_only: cfg.read_only,
            precision: Precision::Ms,
            product: self.product,
            metrics: metrics_url(cfg),
            profiler: None,
        };
        let vars = s.query("SHOW VARIABLES", 1000).await.map_err(|e| match e {
            Error::Query(m) => Error::Connect(m),
            e => e,
        })?;
        if let Some(p) = vars.rows.iter().find(|r| r.first().and_then(|v| v.as_str()) == Some("TimestampPrecision")) {
            s.precision = match p.get(1).and_then(|v| v.as_str()) {
                Some("us") => Precision::Us,
                Some("ns") => Precision::Ns,
                _ => Precision::Ms,
            };
        }
        Ok(Box::new(s))
    }
}

fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.to_string();
    }
    format!("{}://{host}:{}", if cfg.encrypt { "https" } else { "http" }, cfg.port_or(18080))
}

/// The DataNode's Prometheus endpoint: the one in the form, or port 9092
/// of the REST host (tried once).
fn metrics_url(cfg: &ConnectionConfig) -> MetricsEndpoint {
    if let Some(u) = cfg.option("metrics_url") {
        let u = u.trim().trim_end_matches('/');
        let u = if u.starts_with("http://") || u.starts_with("https://") { u.to_string() } else { format!("http://{u}") };
        let u = if u.split("://").nth(1).is_some_and(|rest| rest.contains('/')) { u } else { format!("{u}/metrics") };
        return MetricsEndpoint { url: u, explicit: true, dead: false };
    }
    let base = base_url(cfg);
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base.as_str()));
    let host = rest.split('/').next().unwrap_or(rest);
    let host = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    MetricsEndpoint { url: format!("{scheme}://{host}:9092/metrics"), explicit: false, dead: false }
}

struct MetricsEndpoint {
    url: String,
    /// Typed in the form (keep trying) or guessed (give up after a failure).
    explicit: bool,
    dead: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Precision {
    Ms,
    Us,
    Ns,
}

struct IotDbSession {
    http: reqwest::Client,
    base: String,
    username: String,
    password: String,
    db: Option<String>,
    read_only: bool,
    precision: Precision,
    product: &'static str,
    metrics: MetricsEndpoint,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

/// A statement's answer as a table.
#[derive(Debug, Default)]
struct Table {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<J>>,
}

/// Statements that go to `/query` (the rest go to `/nonQuery`).
fn is_query(stmt: &str) -> bool {
    matches!(first_word(stmt).as_str(), "select" | "show" | "list" | "count" | "explain")
}

fn first_word(stmt: &str) -> String {
    stmt.split(|c: char| !c.is_ascii_alphabetic()).find(|w| !w.is_empty()).unwrap_or("").to_ascii_lowercase()
}

fn words(stmt: &str) -> Vec<String> {
    stmt.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `SELECT … INTO` writes, even though it goes to `/query`.
fn first_write(statements: &[String]) -> Option<String> {
    statements.iter().find_map(|s| {
        if !is_query(s) {
            return Some(first_word(s).to_uppercase());
        }
        (first_word(s) == "select" && words(s).iter().any(|w| w == "into")).then(|| "SELECT … INTO".to_string())
    })
}

/// A `SELECT` capped at `n` rows: `LIMIT n` before `ALIGN BY` (IoTDB's
/// clause order), or `None` when it already has a limit.
fn with_limit(stmt: &str, n: usize) -> Option<String> {
    let w = words(stmt);
    if first_word(stmt) != "select" || w.iter().any(|x| x == "limit") {
        return None;
    }
    let lower = stmt.to_ascii_lowercase();
    match lower.rfind("align by") {
        Some(i) => Some(format!("{} LIMIT {n} {}", stmt[..i].trim_end(), &stmt[i..])),
        None => Some(format!("{} LIMIT {n}", stmt.trim_end())),
    }
}

fn cell(v: &J, data_type: &str) -> J {
    match v {
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                json_i64(i)
            } else if let Some(u) = n.as_u64() {
                json_u64(u)
            } else {
                json_f64(n.as_f64().unwrap_or_default())
            }
        }
        J::String(s) if data_type == "BOOLEAN" => J::Bool(s == "true"),
        J::Array(_) | J::Object(_) => J::String(v.to_string()),
        other => other.clone(),
    }
}

fn time_text(t: i64, p: Precision) -> J {
    let (secs, nanos) = match p {
        Precision::Ms => (t.div_euclid(1000), t.rem_euclid(1000) * 1_000_000),
        Precision::Us => (t.div_euclid(1_000_000), t.rem_euclid(1_000_000) * 1000),
        Precision::Ns => (t.div_euclid(1_000_000_000), t.rem_euclid(1_000_000_000)),
    };
    chrono::DateTime::from_timestamp(secs, nanos as u32)
        .map_or_else(|| json_i64(t), |d| J::String(d.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
}

/// The REST answer (column-major) as rows.
fn to_table(v: &J, precision: Precision) -> Table {
    let strings = |k: &str| -> Option<Vec<String>> {
        v.get(k)?.as_array().map(|a| a.iter().map(|s| s.as_str().unwrap_or_default().to_string()).collect())
    };
    let values: Vec<Vec<J>> = v
        .get("values")
        .and_then(|v| v.as_array())
        .map(|cols| cols.iter().map(|c| c.as_array().cloned().unwrap_or_default()).collect())
        .unwrap_or_default();
    let types = strings("data_types").unwrap_or_default();
    let timestamps: Option<Vec<i64>> =
        v.get("timestamps").and_then(|t| t.as_array()).map(|t| t.iter().filter_map(|x| x.as_i64()).collect());
    let names = strings("column_names").or_else(|| strings("expressions")).unwrap_or_default();

    let mut t = Table::default();
    if timestamps.is_some() {
        t.columns.push(ResultColumn { name: "Time".into(), type_name: "TIMESTAMP".into() });
    }
    for (i, n) in names.iter().enumerate() {
        t.columns.push(ResultColumn { name: n.clone(), type_name: types.get(i).cloned().unwrap_or_default() });
    }
    let n_rows = timestamps.as_ref().map(Vec::len).unwrap_or_else(|| values.first().map_or(0, Vec::len));
    for r in 0..n_rows {
        let mut row = Vec::with_capacity(t.columns.len());
        if let Some(ts) = &timestamps {
            row.push(time_text(ts[r], precision));
        }
        for (c, col) in values.iter().enumerate() {
            let ty = types.get(c).map(String::as_str).unwrap_or("");
            row.push(col.get(r).map_or(J::Null, |x| cell(x, ty)));
        }
        t.rows.push(row);
    }
    t
}

/// `{code, message}` when the server refused the statement.
fn status(v: &J) -> Option<(i64, String)> {
    let code = v.get("code")?.as_i64()?;
    (code != 200).then(|| (code, v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string()))
}

impl IotDbSession {
    async fn post(&self, path: &str, body: J) -> Result<J> {
        let resp = self
            .http
            .post(format!("{}/rest/v2/{path}", self.base))
            .basic_auth(&self.username, Some(&self.password))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Connect(format!("no se pudo conectar con IoTDB: {e}")))?;
        let http_status = resp.status();
        let text = resp.text().await.map_err(Error::connect)?;
        let v: J = serde_json::from_str(&text).unwrap_or(J::Null);
        if http_status.as_u16() == 401 || http_status.as_u16() == 403 {
            let msg = status(&v).map(|(_, m)| m).unwrap_or_else(|| text.trim().to_string());
            return Err(Error::AuthFailed(msg));
        }
        if v.is_null() {
            return Err(Error::Query(format!("HTTP {http_status}: {}", text.trim())));
        }
        Ok(v)
    }

    /// A read, at most `max_rows` rows.
    async fn query(&self, sql: &str, max_rows: usize) -> Result<Table> {
        // One row past the limit tells it was cut; the server refuses a
        // result that reaches `row_limit`, hence one more.
        let limit = max_rows.saturating_add(1);
        let row_limit = limit.saturating_add(1);
        let v = self.post("query", json!({ "sql": sql, "row_limit": row_limit })).await?;
        match status(&v) {
            None => Ok(to_table(&v, self.precision)),
            Some((ROW_LIMIT_EXCEEDED, m)) => {
                // The server won't hand out part of a result: ask for fewer rows.
                let Some(capped) = with_limit(sql, limit) else { return Err(Error::Query(m)) };
                let v = self.post("query", json!({ "sql": capped, "row_limit": row_limit })).await?;
                match status(&v) {
                    None => Ok(to_table(&v, self.precision)),
                    Some((_, m)) => Err(Error::Query(m)),
                }
            }
            Some((_, m)) => Err(Error::Query(m)),
        }
    }

    async fn non_query(&self, sql: &str) -> Result<()> {
        let v = self.post("nonQuery", json!({ "sql": sql })).await?;
        match status(&v) {
            Some((_, m)) => Err(Error::Query(m)),
            None => Ok(()),
        }
    }

    fn device_path(&self, obj: &ObjectRef) -> String {
        match (&self.db, obj.name.starts_with("root.")) {
            (Some(db), false) => format!("{db}.{}", obj.name),
            _ => obj.name.clone(),
        }
    }
}

/// Row cap for the monitor's metadata views.
fn monitor_rows() -> usize {
    200
}

fn texts(t: &Table, col: usize) -> Vec<String> {
    t.rows.iter().filter_map(|r| r.get(col)?.as_str().map(str::to_string)).collect()
}

/// A path node as IoTDB takes it: backquoted unless it's a plain name.
fn node(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().all(|c| c.is_ascii_digit())
        && !matches!(name.to_ascii_lowercase().as_str(), "time" | "timestamp" | "root");
    let quoted = name.len() > 1 && name.starts_with('`') && name.ends_with('`');
    if plain || quoted {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

/// A node without its backquotes.
fn unquote(node: &str) -> String {
    match node.strip_prefix('`').and_then(|n| n.strip_suffix('`')) {
        Some(n) => n.replace("``", "`"),
        None => node.to_string(),
    }
}

/// Nodes of a path, splitting at dots outside backquotes.
fn split_path(path: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quoted = false;
    for c in path.chars() {
        match c {
            '`' => quoted = !quoted,
            '.' if !quoted => {
                out.push(String::new());
                continue;
            }
            _ => {}
        }
        out.last_mut().unwrap().push(c);
    }
    out
}

/// A database path: `root.x` as given, a bare name under `root`.
fn database_path(name: &str) -> String {
    let name = name.trim();
    if name == "root" || name.starts_with("root.") {
        return name.to_string();
    }
    let nodes: Vec<String> = split_path(name).iter().map(|n| node(n)).collect();
    format!("root.{}", nodes.join("."))
}

/// Full path of a device: `name` under the database in `schema`, or `name`
/// itself when it already starts at `root`.
fn full_device(schema: Option<&str>, name: &str) -> Result<String> {
    if name.starts_with("root.") {
        return Ok(name.to_string());
    }
    match schema.filter(|s| !s.is_empty()) {
        Some(db) => Ok(format!("{}.{name}", database_path(db))),
        None => Err(Error::Query(format!(
            "Falta la base de datos del dispositivo «{name}»: indicá la ruta completa (root.…)."
        ))),
    }
}

fn is_time(col: &str) -> bool {
    col.eq_ignore_ascii_case("time") || col.eq_ignore_ascii_case("timestamp")
}

fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let device = full_device(t.schema.as_deref(), &t.name)?;
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        // IoTDB has no IF EXISTS here: a missing device fails.
        out.push(format!("DELETE TIMESERIES {device}.**;"));
    }
    if parts.create {
        let cols: Vec<&ColumnDef> = t.columns.iter().filter(|c| !is_time(&c.name)).collect();
        if cols.is_empty() {
            return Err(Error::Query("Un dispositivo necesita al menos una serie (medición).".into()));
        }
        let opt = |c: &ColumnDef, k: &str| c.options.get(k).map(|v| v.trim().to_ascii_uppercase()).filter(|v| !v.is_empty());
        let aligned = t.options.get("aligned").is_some_and(|v| v == "true" || v == "1");
        if aligned {
            let defs: Vec<String> = cols
                .iter()
                .map(|c| {
                    let mut d = format!("    {} {}", node(&c.name), c.data_type.trim().to_ascii_uppercase());
                    if let Some(e) = opt(c, "encoding") {
                        d.push_str(&format!(" encoding={e}"));
                    }
                    if let Some(z) = opt(c, "compression") {
                        d.push_str(&format!(" compressor={z}"));
                    }
                    d
                })
                .collect();
            out.push(format!("CREATE ALIGNED TIMESERIES {device}(\n{}\n);", defs.join(",\n")));
        } else {
            for c in cols {
                let mut d = format!("CREATE TIMESERIES {device}.{} WITH DATATYPE={}", node(&c.name), c.data_type.trim().to_ascii_uppercase());
                if let Some(e) = opt(c, "encoding") {
                    d.push_str(&format!(", ENCODING={e}"));
                }
                if let Some(z) = opt(c, "compression") {
                    d.push_str(&format!(", COMPRESSOR={z}"));
                }
                d.push(';');
                out.push(d);
            }
        }
    }
    Ok(out.join("\n"))
}

/// A value as an IoTDB literal. Times shown as `2024-01-31 13:45:00.5`
/// (UTC, see [`time_text`]) go back as ISO-8601 with the offset.
fn literal(v: &J, time: bool) -> String {
    match v {
        J::Null => "null".into(),
        J::Bool(b) => b.to_string(),
        J::Number(n) => n.to_string(),
        J::String(s) if time => chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S%.f")
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string())
            .unwrap_or_else(|_| format!("'{}'", s.replace('\'', "''"))),
        J::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<J>]) -> Result<String> {
    let device = full_device(target.schema(), &target.name)?;
    let Some(ti) = columns.iter().position(|c| is_time(c)) else {
        return Err(Error::Unsupported("IoTDB necesita la columna Time (marca de tiempo) para insertar filas.".into()));
    };
    let prefix = format!("{device}.");
    let mut header = vec!["timestamp".to_string()];
    let others: Vec<usize> = (0..columns.len()).filter(|&i| i != ti).collect();
    header.extend(others.iter().map(|&i| node(columns[i].strip_prefix(&prefix).unwrap_or(&columns[i]))));
    let head = format!("INSERT INTO {device}({}) VALUES", header.join(", "));
    let mut out = Vec::new();
    for chunk in rows.chunks(100) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| {
                let cell = |i: usize| r.get(i).unwrap_or(&J::Null);
                let mut v = vec![literal(cell(ti), true)];
                v.extend(others.iter().map(|&i| literal(cell(i), false)));
                format!("({})", v.join(", "))
            })
            .collect();
        out.push(format!("{head}\n    {};", values.join(",\n    ")));
    }
    Ok(out.join("\n"))
}

/// Edited rows: IoTDB has no UPDATE, but writing a point at an existing
/// timestamp overwrites it, so each row is an `INSERT` at its `Time`. A
/// null can't be written (IoTDB skips it), so clearing a value is a
/// `DELETE` of that point.
fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let device = full_device(target.schema(), &target.name)?;
    let prefix = format!("{device}.");
    let mut out = Vec::new();
    for c in changes.iter().filter(|c| !c.set.is_empty()) {
        let Some(time) = c.key.iter().find(|(k, _)| is_time(k)).map(|(_, v)| v).filter(|v| !v.is_null()) else {
            return Err(Error::Unsupported(
                "IoTDB modifica un punto reescribiendo su marca de tiempo: la clave de la fila tiene que incluir la columna Time".into(),
            ));
        };
        if c.set.iter().any(|(k, _)| is_time(k)) {
            return Err(Error::Unsupported("en IoTDB no se puede cambiar la marca de tiempo de un punto: crearía otro".into()));
        }
        let ts = literal(time, true);
        let path = |k: &str| node(k.strip_prefix(&prefix).unwrap_or(k));
        let (nulls, values): (Vec<_>, Vec<_>) = c.set.iter().partition(|(_, v)| v.is_null());
        if !values.is_empty() {
            let mut header = vec!["timestamp".to_string()];
            header.extend(values.iter().map(|(k, _)| path(k)));
            let mut v = vec![ts.clone()];
            v.extend(values.iter().map(|(_, v)| literal(v, false)));
            out.push(format!("INSERT INTO {device}({}) VALUES ({});", header.join(", "), v.join(", ")));
        }
        for (k, _) in nulls {
            out.push(format!("DELETE FROM {device}.{} WHERE time = {ts};", path(k)));
        }
    }
    Ok(out.join("\n"))
}

/// Deleted rows: a row is the device's point at a timestamp, so each one
/// is `DELETE FROM <device>.* WHERE time = <Time>` (every measurement of
/// the device at that instant). Without a non-null `Time` in the key the
/// point can't be singled out.
fn delete_script(target: &ObjectRef, keys: &[Vec<(String, J)>]) -> Result<String> {
    let device = full_device(target.schema(), &target.name)?;
    let mut out = Vec::new();
    for key in keys {
        let Some(time) = key.iter().find(|(k, _)| is_time(k)).map(|(_, v)| v).filter(|v| !v.is_null()) else {
            return Err(Error::Unsupported(
                "IoTDB borra un punto por su marca de tiempo: la clave de la fila tiene que incluir la columna Time".into(),
            ));
        };
        out.push(format!("DELETE FROM {device}.* WHERE time = {};", literal(time, true)));
    }
    Ok(out.join("\n"))
}

/// The browse query (`SELECT * FROM <device> ORDER BY time DESC LIMIT n`)
/// restricted by the grid's column filters. Columns come back as full
/// paths (`root.sg.d1.s1`): the WHERE names the measurement under the
/// device. `Time` only compares; LIKE relies on the default `\` escape.
fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    use dbine_driver::sql::Quote;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let device = browse
        .find("FROM ")
        .map(|i| browse[i + 5..].lines().next().unwrap_or("").trim().to_string())
        .unwrap_or_default();
    let prefix = format!("{device}.");
    let data = |v: &J| literal(v, false);
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &data, like: "LIKE", true_literal: "true", false_literal: "false" };
    let mut parts = Vec::new();
    for f in filters {
        if is_time(&f.column) {
            let op = match f.op {
                FilterOp::Eq => "=",
                FilterOp::Ne => "!=",
                FilterOp::Gt => ">",
                FilterOp::Ge => ">=",
                FilterOp::Lt => "<",
                FilterOp::Le => "<=",
                _ => return Err(Error::Unsupported("en IoTDB la columna Time solo se filtra por comparación".into())),
            };
            let v = f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)))?;
            parts.push(format!("time {op} {}", literal(v, true)));
            continue;
        }
        let mut one = f.clone();
        one.column = unquote(f.column.strip_prefix(&prefix).unwrap_or(&f.column));
        let c = sql_condition(std::slice::from_ref(&one), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => s.to_string(),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

#[async_trait]
impl Session for IotDbSession {
    async fn server_version(&mut self) -> Result<String> {
        let t = self.query("SHOW VERSION", 10).await?;
        Ok(format!("{} {}", self.product, texts(&t, 0).first().cloned().unwrap_or_default()).trim().to_string())
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let answer = |t: Table| monitor::Answer { columns: t.columns.into_iter().map(|c| c.name).collect(), rows: t.rows };
        let mut i = monitor::Inputs::default();
        // The first one proves the connection; the rest may be refused.
        i.version = Some(answer(self.query("SHOW VERSION", 10).await?));
        i.variables = self.query("SHOW VARIABLES", 1000).await.ok().map(answer);
        i.cluster = self.query("SHOW CLUSTER", monitor_rows()).await.ok().map(answer);
        i.regions = self.query("SHOW REGIONS", monitor_rows()).await.ok().map(answer);
        i.queries = self.query("SHOW QUERIES", monitor_rows()).await.ok().map(answer);
        i.databases = self.query("SHOW DATABASES DETAILS", monitor_rows()).await.ok().map(answer);
        i.series = self.query("COUNT TIMESERIES root.** GROUP BY LEVEL = 1", monitor_rows()).await.ok().map(answer);
        i.devices = self
            .query("COUNT DEVICES root.**", 1)
            .await
            .ok()
            .and_then(|t| t.rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()));
        if !self.metrics.dead {
            let resp = self.http.get(&self.metrics.url).timeout(Duration::from_secs(3)).send().await;
            match resp {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(body) => i.prom = Some(monitor::Prom::parse(&body)),
                    Err(e) => i.prom_note = Some(format!("No se pudo leer {}: {e}", self.metrics.url)),
                },
                other => {
                    let why = match other {
                        Ok(r) => format!("HTTP {}", r.status()),
                        Err(e) => e.to_string(),
                    };
                    i.prom_note = Some(format!("No se pudo leer el endpoint de métricas {} ({why}).", self.metrics.url));
                    self.metrics.dead = !self.metrics.explicit;
                }
            }
        }
        if i.prom.is_none() && !self.metrics.explicit {
            i.prom_note = Some(format!(
                "CPU, memoria, conexiones, disco y red salen del endpoint Prometheus del DataNode, que no respondió en {}: \
                 activalo con dn_metric_reporter_list=PROMETHEUS o indicá su URL en la conexión.",
                self.metrics.url
            ));
        }
        Ok(monitor::snapshot(self.product, &i))
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
        self.profiler = None;
        Ok(())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let t = self.query("SHOW DATABASES", 100_000).await?;
        let mut v: Vec<String> = texts(&t, 0).into_iter().filter(|d| !d.starts_with("root.__")).collect();
        v.sort();
        Ok(v)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Some(db) = self.db.clone() else { return Ok(Vec::new()) };
        let t = self.query(&format!("SHOW DEVICES {db}.**"), 100_000).await?;
        let prefix = format!("{db}.");
        let mut v: Vec<DbObject> = texts(&t, 0)
            .into_iter()
            .map(|d| DbObject {
                kind: DEVICE.into(),
                schema: None,
                name: d.strip_prefix(&prefix).map(str::to_string).unwrap_or(d),
                parent: None,
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let device = self.device_path(obj);
        let t = self.query(&format!("SHOW TIMESERIES {device}.*"), 100_000).await?;
        let name_col = t.columns.iter().position(|c| c.name == "Timeseries").unwrap_or(0);
        let type_col = t.columns.iter().position(|c| c.name == "DataType").unwrap_or(3);
        let prefix = format!("{device}.");
        let mut out = vec![ColumnInfo {
            name: "Time".into(),
            data_type: "TIMESTAMP".into(),
            nullable: false,
            primary_key: true,
            auto_increment: false,
            default_value: None,
        }];
        for r in &t.rows {
            let full = r.get(name_col).and_then(|v| v.as_str()).unwrap_or_default();
            out.push(ColumnInfo {
                name: full.strip_prefix(&prefix).unwrap_or(full).to_string(),
                data_type: r.get(type_col).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                nullable: true,
                primary_key: false,
                auto_increment: false,
                default_value: None,
            });
        }
        Ok(out)
    }

    /// Devices with their series, from two statements (devices for the
    /// aligned flag, series for types, encodings and compressions).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let Some(db) = self.db.clone() else { return Ok(Vec::new()) };
        let devices = self.query(&format!("SHOW DEVICES {db}.**"), 10_000_000).await?;
        let series = self.query(&format!("SHOW TIMESERIES {db}.**"), 10_000_000).await?;
        let pos = |t: &Table, n: &str| t.columns.iter().position(|c| c.name == n);
        let s = |r: &[J], i: Option<usize>| i.and_then(|i| r.get(i)?.as_str()).unwrap_or_default().to_string();
        let prefix = format!("{db}.");
        let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
        let (dev, aligned) = (pos(&devices, "Device").or(Some(0)), pos(&devices, "IsAligned"));
        for r in &devices.rows {
            let path = s(r, dev);
            let name = path.strip_prefix(&prefix).unwrap_or(&path).to_string();
            let mut options = BTreeMap::new();
            if s(r, aligned) == "true" {
                options.insert("aligned".to_string(), "true".to_string());
            }
            let time = ColumnDef { name: "Time".into(), data_type: "TIMESTAMP".into(), nullable: false, ..Default::default() };
            tables.insert(
                name.clone(),
                TableSchema {
                    kind: DEVICE.into(),
                    schema: Some(db.clone()),
                    name,
                    columns: vec![time],
                    primary_key: Some(KeyDef { name: None, columns: vec!["Time".into()] }),
                    options,
                    ..Default::default()
                },
            );
        }
        let [ts, ty, enc, comp, view] = ["Timeseries", "DataType", "Encoding", "Compression", "ViewType"].map(|n| pos(&series, n));
        for r in &series.rows {
            if s(r, view) == "VIEW" {
                continue;
            }
            let mut nodes = split_path(&s(r, ts.or(Some(0))));
            let measurement = unquote(&nodes.pop().unwrap_or_default());
            let path = nodes.join(".");
            let Some(t) = tables.get_mut(path.strip_prefix(&prefix).unwrap_or(&path)) else { continue };
            let mut options = BTreeMap::new();
            for (k, i) in [("encoding", enc), ("compression", comp)] {
                let v = s(r, i);
                if !v.is_empty() {
                    options.insert(k.to_string(), v);
                }
            }
            t.columns.push(ColumnDef { name: measurement, data_type: s(r, ty), options, ..Default::default() });
        }
        // Devices whose series are all views (or only a template) aren't tables.
        Ok(tables.into_values().filter(|t| t.columns.len() > 1).collect())
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        self.non_query(&format!("CREATE DATABASE {}", database_path(name))).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()));
        }
        let path = database_path(name);
        if self.db.as_deref().map(database_path).as_deref() == Some(path.as_str()) {
            return Err(Error::Query(format!("No se puede borrar {path}: es la base de datos de esta conexión.")));
        }
        self.non_query(&format!("DELETE DATABASE {path}")).await
    }

    async fn definition(&mut self, _obj: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
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

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT *\nFROM {}\nORDER BY time DESC\nLIMIT {limit}", self.device_path(obj))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let statements = dbine_driver::sql::split_statements(text);
        if self.read_only {
            if let Some(kw) = first_write(&statements) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, SHOW, LIST, COUNT)."
                )));
            }
        }
        for unit in dbine_driver::sql::split_script(text, &dbine_driver::ScriptDialect::generic()) {
            let stmt = dbine_driver::sql::strip_comments(&unit.text, &dbine_driver::ScriptDialect::generic(), false);
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            let r = if is_query(stmt) {
                self.query(stmt, max_rows).await.map(|t| {
                    out.begin_result(t.columns);
                    for row in t.rows {
                        out.push_row(row, max_rows);
                    }
                })
            } else {
                self.non_query(stmt).await.map(|_| out.push_affected(0))
            };
            match r {
                Ok(()) => {}
                // Positions count in the text sent, which has no comments:
                // placed only when the statement had none.
                Err(Error::Query(m)) if stmt == unit.text => return Err(script::shift(script::error(&m, stmt), &unit)),
                Err(Error::Query(m)) => return Err(script::shift(dbine_driver::ScriptError::new(m).into(), &unit)),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The user's own privileges (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_browse_names_measurements() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<J>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT *\nFROM root.sg.d1\nORDER BY time DESC\nLIMIT 200",
                &[
                    f("Time", FilterOp::Ge, vec![json!("2024-01-31 13:45:00.5")]),
                    f("root.sg.d1.status", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("root.sg.d1.temp", FilterOp::Gt, vec![json!(20.5)]),
                    f("root.sg.d1.note", FilterOp::Contains, vec![json!("a_b")]),
                    f("root.sg.d1.on", FilterOp::IsNull, vec![]),
                    f("root.sg.d1.code", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM root.sg.d1\nWHERE time >= 2024-01-31T13:45:00.500+00:00\n  AND `status` = 'O''Brien'\n  AND `temp` > 20.5\n  AND `note` LIKE '%a\\_b%'\n  AND `on` IS NULL\n  AND `code` IN (1, 2)\nORDER BY time DESC\nLIMIT 200"
        );
        assert!(filtered_browse("SELECT *\nFROM root.sg.d1\nLIMIT 5", &[f("Time", FilterOp::IsNull, vec![])]).is_err());
    }

    #[test]
    fn series_answers_get_a_time_column() {
        let v = json!({"expressions":["root.sg.d1.temp","root.sg.d1.on"],"column_names":null,
            "data_types":["DOUBLE","BOOLEAN"],"timestamps":[1706708700000i64, 1706708700500i64],
            "values":[[21.5, null],[true, false]]});
        let t = to_table(&v, Precision::Ms);
        let names: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Time", "root.sg.d1.temp", "root.sg.d1.on"]);
        assert_eq!(t.rows[0], vec![json!("2024-01-31 13:45:00"), json!(21.5), json!(true)]);
        assert_eq!(t.rows[1], vec![json!("2024-01-31 13:45:00.500"), J::Null, json!(false)]);
    }

    #[test]
    fn metadata_answers_are_column_major() {
        let v = json!({"expressions":null,"column_names":["Device","IsAligned"],"data_types":null,"timestamps":null,
            "values":[["root.sg.d1","root.sg.d2"],["false","true"]]});
        let t = to_table(&v, Precision::Ms);
        assert_eq!(t.rows, vec![vec![json!("root.sg.d1"), json!("false")], vec![json!("root.sg.d2"), json!("true")]]);
        // Aggregates: expressions without timestamps.
        let v = json!({"expressions":["count(root.sg.d1.t)"],"data_types":["INT64"],"timestamps":null,"values":[[3]]});
        assert_eq!(to_table(&v, Precision::Ms).rows, vec![vec![json!(3)]]);
    }

    #[test]
    fn precisions() {
        assert_eq!(time_text(1_706_708_700_000_001, Precision::Us), json!("2024-01-31 13:45:00.000001"));
        assert_eq!(time_text(1_706_708_700_000_000_000, Precision::Ns), json!("2024-01-31 13:45:00"));
    }

    #[test]
    fn routing_and_read_only() {
        assert!(is_query("  select * from root.sg.d1"));
        assert!(is_query("SHOW DATABASES") && is_query("count timeseries root.**") && is_query("LIST USER"));
        assert!(!is_query("INSERT INTO root.sg.d1(timestamp, t) VALUES (1, 2)"));
        let s = |x: &str| dbine_driver::sql::split_statements(x);
        assert_eq!(first_write(&s("select * from root.a; show devices")), None);
        assert_eq!(first_write(&s("show databases; delete database root.a")).as_deref(), Some("DELETE"));
        assert_eq!(first_write(&s("select s1 into root.b(s1) from root.a")).as_deref(), Some("SELECT … INTO"));
    }

    #[test]
    fn limits_go_before_align_by() {
        assert_eq!(with_limit("SELECT * FROM root.a", 5).unwrap(), "SELECT * FROM root.a LIMIT 5");
        assert_eq!(with_limit("select * from root.a.** align by device", 5).unwrap(), "select * from root.a.** LIMIT 5 align by device");
        assert!(with_limit("SELECT * FROM root.a LIMIT 3", 5).is_none());
        assert!(with_limit("SHOW TIMESERIES", 5).is_none());
    }

    #[test]
    fn urls() {
        let cfg = ConnectionConfig { host: "iot".into(), ..Default::default() };
        assert_eq!(base_url(&cfg), "http://iot:18080");
        let cfg = ConnectionConfig { host: "https://iot.example.com/".into(), ..Default::default() };
        assert_eq!(base_url(&cfg), "https://iot.example.com");
    }

    #[test]
    fn metrics_endpoints() {
        let mut cfg = ConnectionConfig { host: "iot".into(), port: 18080, ..Default::default() };
        let m = metrics_url(&cfg);
        assert_eq!((m.url.as_str(), m.explicit), ("http://iot:9092/metrics", false));
        cfg.host = "https://iot.example.com:18443/".into();
        assert_eq!(metrics_url(&cfg).url, "https://iot.example.com:9092/metrics");
        cfg.options.insert("metrics_url".into(), "dn1:9093".into());
        let m = metrics_url(&cfg);
        assert_eq!((m.url.as_str(), m.explicit), ("http://dn1:9093/metrics", true));
        cfg.options.insert("metrics_url".into(), "https://dn1/prom".into());
        assert_eq!(metrics_url(&cfg).url, "https://dn1/prom");
    }

    #[test]
    fn variants() {
        let ids: Vec<&str> = drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["iotdb", "timechodb"]);
        assert!(drivers().iter().all(|d| d.capabilities().monitor && d.designer().is_some()));
    }
    fn device(aligned: bool) -> TableSchema {
        let col = |n: &str, t: &str, enc: &str| ColumnDef {
            name: n.into(),
            data_type: t.into(),
            options: [("encoding".to_string(), enc.to_string()), ("compression".to_string(), "snappy".to_string())]
                .into_iter()
                .filter(|(_, v)| !v.is_empty())
                .collect(),
            ..Default::default()
        };
        TableSchema {
            kind: DEVICE.into(),
            schema: Some("root.sg".into()),
            name: "plant.d1".into(),
            columns: vec![col("Time", "TIMESTAMP", ""), col("temp", "double", "GORILLA"), col("my-x", "INT32", "")],
            options: if aligned { [("aligned".to_string(), "true".to_string())].into() } else { BTreeMap::new() },
            ..Default::default()
        }
    }

    #[test]
    fn ddl() {
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        assert_eq!(
            table_ddl(&device(false), all).unwrap(),
            "DELETE TIMESERIES root.sg.plant.d1.**;\n\
             CREATE TIMESERIES root.sg.plant.d1.temp WITH DATATYPE=DOUBLE, ENCODING=GORILLA, COMPRESSOR=SNAPPY;\n\
             CREATE TIMESERIES root.sg.plant.d1.`my-x` WITH DATATYPE=INT32, COMPRESSOR=SNAPPY;"
        );
        let create = DdlParts { create: true, ..Default::default() };
        assert_eq!(
            table_ddl(&device(true), create).unwrap(),
            "CREATE ALIGNED TIMESERIES root.sg.plant.d1(\n    temp DOUBLE encoding=GORILLA compressor=SNAPPY,\n    `my-x` INT32 compressor=SNAPPY\n);"
        );
        assert_eq!(table_ddl(&device(true), DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap(), "");
        let mut t = device(false);
        t.schema = None;
        assert!(table_ddl(&t, create).is_err());
        t.name = "root.x.d".into();
        assert!(table_ddl(&t, create).unwrap().starts_with("CREATE TIMESERIES root.x.d.temp "));
    }

    #[test]
    fn paths() {
        assert_eq!(database_path("root.a.b"), "root.a.b");
        assert_eq!(database_path("fábrica"), "root.`fábrica`");
        assert_eq!(database_path("a.b-c"), "root.a.`b-c`");
        assert_eq!(split_path("root.sg.`a.b`.s1"), ["root", "sg", "`a.b`", "s1"]);
        assert_eq!(unquote("`a``b`"), "a`b");
        assert_eq!(node("time"), "`time`");
        assert_eq!(node("123"), "`123`");
    }

    #[test]
    fn inserts() {
        let target = ObjectRef { kind: DEVICE.into(), schema: Some("root.sg".into()), name: "d1".into() };
        let cols = ["Time".to_string(), "root.sg.d1.t".to_string(), "s".to_string(), "on".to_string()];
        let rows = vec![
            vec![json!("2024-01-31 13:45:00.500"), json!(1.5), json!("it's"), json!(true)],
            vec![json!(1706708700000i64), J::Null, J::Null, json!(false)],
        ];
        assert_eq!(
            insert_script(&target, &cols, &rows).unwrap(),
            "INSERT INTO root.sg.d1(timestamp, t, s, on) VALUES\n    \
             (2024-01-31T13:45:00.500+00:00, 1.5, 'it''s', true),\n    (1706708700000, null, null, false);"
        );
        assert!(matches!(insert_script(&target, &cols[1..], &rows), Err(Error::Unsupported(_))));
        let many: Vec<Vec<J>> = (0..150).map(|i| vec![json!(i), json!(i)]).collect();
        assert_eq!(insert_script(&target, &cols[..2], &many).unwrap().matches("INSERT INTO").count(), 2);
    }

    #[test]
    fn updates_overwrite_points() {
        let target = ObjectRef { kind: DEVICE.into(), schema: Some("root.sg".into()), name: "d1".into() };
        let c = RowChange {
            key: vec![("Time".into(), json!("2024-01-31 13:45:00.500"))],
            set: vec![("root.sg.d1.s".into(), json!("it's")), ("t".into(), J::Null), ("on".into(), json!(true))], ..Default::default()
        };
        assert_eq!(
            update_script(&target, &[c, RowChange::default()]).unwrap(),
            "INSERT INTO root.sg.d1(timestamp, s, on) VALUES (2024-01-31T13:45:00.500+00:00, 'it''s', true);\n\
             DELETE FROM root.sg.d1.t WHERE time = 2024-01-31T13:45:00.500+00:00;"
        );
        let no_time = RowChange { key: vec![], set: vec![("t".into(), json!(1))], ..Default::default() };
        assert!(matches!(update_script(&target, &[no_time]), Err(Error::Unsupported(_))));
        let null_time = RowChange { key: vec![("Time".into(), J::Null)], set: vec![("t".into(), json!(1))], ..Default::default() };
        assert!(matches!(update_script(&target, &[null_time]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn deletes_points_by_time() {
        let target = ObjectRef { kind: DEVICE.into(), schema: Some("root.sg".into()), name: "d1".into() };
        let keys = vec![vec![("s".into(), json!("it's")), ("Time".into(), json!("2024-01-31 13:45:00.500"))], vec![("Time".into(), json!(1706708700000_i64))]];
        assert_eq!(
            delete_script(&target, &keys).unwrap(),
            "DELETE FROM root.sg.d1.* WHERE time = 2024-01-31T13:45:00.500+00:00;\n\
             DELETE FROM root.sg.d1.* WHERE time = 1706708700000;"
        );
        assert!(matches!(delete_script(&target, &[vec![("s".into(), json!(1))]]), Err(Error::Unsupported(_))));
        assert!(matches!(delete_script(&target, &[vec![("Time".into(), J::Null)]]), Err(Error::Unsupported(_))));
    }
}

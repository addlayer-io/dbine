//! TDengine 3.x through taosAdapter's REST API: `POST /rest/sql/{db}` with
//! the statement as the body and basic auth. One statement per request
//! (scripts are split); `USE db` is kept by the session and sent as the
//! path. Answers are `{code, column_meta, data}`; errors `{code, desc}`
//! with HTTP 200. Cancel drops the request and kills the statement on the
//! server (`KILL QUERY` of the matching `performance_schema.perf_queries`
//! row).
//!
//! The monitor reads the cluster views (`SHOW DNODES`, `SHOW MNODES`,
//! `perf_connections`, `perf_queries`, `ins_disk_usage`) and, when
//! taosKeeper feeds it, the `log` database (`taosd_dnodes_info`: CPU,
//! memory, disk and network of each dnode).

mod ddl;
mod permissions;
mod processes;
mod profiler;
mod script;
mod security;
mod sync;
mod transfer;

use dbine_driver::plan::tree_from_indented_text;
use dbine_driver::sql::split_statements;
use dbine_driver::sql::Quote;
use dbine_driver::{
    async_trait, json_bytes, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, Metric, MetricUnit,
    MonitorSnapshot, MonitorTable, ObjectKindInfo, ObjectRef, Plan, PlanNode, QueryOutcome, Result, ResultColumn, RowChange,
    Session, TableSchema,
};
use ddl::{lit, q, qualified, SUBTABLE, SUPERTABLE, TAG};
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Child tables the explorer lists per database at most.
const MAX_SUBTABLES: usize = 5000;
const SYSTEM_DBS: &[&str] = &["information_schema", "performance_schema"];

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(TdDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "tdengine",
        name: "TDengine",
        family: Family::TimeSeries,
        language: Language::Sql,
        dialect: "tdengine",
        default_port: 6041,
        fields: vec![
            Field::host(),
            Field::port().placeholder("6041").help("Puerto REST de taosAdapter."),
            Field::database(),
            Field { default: "root", ..Field::username() },
            Field::password().help("La predeterminada de TDengine es taosdata."),
            Field::new("timezone", "Zona horaria", FieldKind::Text)
                .placeholder("UTC")
                .help("Zona IANA en la que se muestran las fechas (p. ej. America/Argentina/Buenos_Aires).")
                .advanced(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(SUPERTABLE, "Supertablas", true, true, true),
            ObjectKindInfo::tables(),
            ObjectKindInfo::new(SUBTABLE, "Subtablas", true, true, true),
            ObjectKindInfo::views(),
            ObjectKindInfo::new(kinds::STREAM, "Streams", false, false, true),
            ObjectKindInfo::new(kinds::TOPIC, "Tópicos", false, false, true),
        ],
    }
}

pub struct TdDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for TdDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// One request per statement, as taosAdapter takes them; `USE` stays
    /// in the session (sent as the database of the next ones).
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Connections and their running queries from `performance_schema`,
    /// `KILL QUERY` and `KILL CONNECTION` (see [`processes`]).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: false,
            monitor: true,
            kill_session: true,
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Multi-row INSERTs up to the SQL length limit (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
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

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(ddl::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        ddl::update_script(target.schema(), &target.name, changes).map_err(Error::Unsupported)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(target.schema(), &target.name, target.kind == SUPERTABLE, keys).map_err(Error::Unsupported)
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
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let db = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
        let s = TdSession {
            conn: Arc::new(Conn {
                http,
                base: format!("{scheme}://{host}:{}", cfg.port_or(6041)),
                user: cfg.username.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| "root".into()),
                password: cfg.password.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "taosdata".into()),
                tz: cfg.option("timezone").map(str::to_string),
            }),
            db: db.map(str::to_string),
            cancel: Arc::new(Cancel::default()),
            profiler: None,
            rt: tokio::runtime::Handle::current(),
        };
        let check = async {
            match s.db.clone() {
                Some(d) => s.query(&format!("SHOW {}.VGROUPS", q(&d))).await.map(|_| ()),
                None => s.query("SELECT SERVER_VERSION()").await.map(|_| ()),
            }
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
    password: String,
    tz: Option<String>,
}

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
    /// The statement in flight, to find it in `perf_queries`.
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

/// One answer: columns (name, type) and rows.
struct Answer {
    columns: Vec<(String, String)>,
    data: Vec<Vec<Value>>,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

/// Error codes: 0x0357 (855) authentication failure, 0x0300s other
/// client-side ones; everything else the server's statement error.
fn td_error(code: i64, desc: String) -> Error {
    match code {
        0x0357 => Error::AuthFailed(desc),
        0x020B => Error::Cancelled,
        // TDengine documents its codes in hex (0x2600: syntax error).
        _ => dbine_driver::ScriptError::new(desc).with_code(format!("0x{:04X}", code)).into(),
    }
}

impl Conn {
    async fn sql(&self, db: Option<&str>, stmt: &str) -> Result<Answer> {
        let mut url = format!("{}/rest/sql", self.base);
        if let Some(d) = db.filter(|d| !d.is_empty()) {
            url.push('/');
            url.push_str(&encode(d));
        }
        let mut rb = self.http.post(url).basic_auth(&self.user, Some(&self.password)).body(stmt.to_string());
        if let Some(tz) = &self.tz {
            rb = rb.query(&[("tz", tz)]);
        }
        let resp = rb.send().await.map_err(http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(http_error)?;
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                Error::AuthFailed(text.trim().to_string())
            } else {
                Error::Query(format!("HTTP {status}: {}", text.trim()))
            }
        })?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            let desc = v.get("desc").and_then(Value::as_str).unwrap_or("error").to_string();
            return Err(td_error(code, desc));
        }
        let columns = v
            .get("column_meta")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| {
                let name = c.get(0).and_then(Value::as_str).unwrap_or("").to_string();
                let ty = c.get(1).and_then(Value::as_str).unwrap_or("").to_string();
                (name, ty)
            })
            .collect();
        let data = match v.get("data") {
            Some(Value::Array(rows)) => rows.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect(),
            _ => Vec::new(),
        };
        Ok(Answer { columns, data })
    }
}

/// Path segment encoding for a database name.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

pub struct TdSession {
    conn: Arc<Conn>,
    db: Option<String>,
    cancel: Arc<Cancel>,
    rt: tokio::runtime::Handle,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

/// A cell as the UI wants it, given its TDengine type.
fn cell(v: Value, ty: &str) -> Value {
    match v {
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => Value::Number(n),
        },
        Value::String(s) if ty == "TIMESTAMP" => Value::String(timestamp(&s)),
        Value::String(s) if ty == "VARBINARY" || ty == "GEOMETRY" => {
            match (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect::<Option<Vec<u8>>>() {
                Some(b) => json_bytes(&b),
                None => Value::String(s),
            }
        }
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v,
    }
}

/// `2024-01-31T13:45:00.123Z` → `2024-01-31 13:45:00.123`; an offset
/// other than UTC is kept (`… -03:00`).
fn timestamp(s: &str) -> String {
    let t = s.replacen('T', " ", 1);
    match t.strip_suffix('Z') {
        Some(u) => u.to_string(),
        None => match t.rfind(['+', '-']).filter(|&i| i > 18) {
            Some(i) => format!("{} {}", &t[..i], &t[i..]),
            None => t,
        },
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn first_word(stmt: &str) -> String {
    stmt.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

/// Statements that return rows (the rest answer `affected_rows`).
fn returns_rows(stmt: &str) -> bool {
    matches!(first_word(stmt).as_str(), "SELECT" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "WITH")
}

/// `USE db` → `db`.
fn use_target(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    if first_word(s) != "USE" {
        return None;
    }
    Some(s[3..].trim().trim_matches('`').to_string()).filter(|d| !d.is_empty())
}

impl TdSession {
    async fn query(&self, stmt: &str) -> Result<Answer> {
        self.cancel.run(self.conn.sql(self.db.as_deref(), stmt)).await
    }

    async fn strings(&self, stmt: &str) -> Result<Vec<Vec<String>>> {
        Ok(self.query(stmt).await?.data.iter().map(|r| r.iter().map(text).collect()).collect())
    }

    /// Named rows of a query: each row as (column → value).
    async fn records(&self, stmt: &str) -> Result<Vec<serde_json::Map<String, Value>>> {
        let a = self.query(stmt).await?;
        Ok(a.data
            .into_iter()
            .map(|r| a.columns.iter().map(|(n, _)| n.clone()).zip(r).collect::<serde_json::Map<_, _>>())
            .collect())
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(stmt.to_string());
        let r = self.query(stmt).await;
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let a = r?;
        if let Some(db) = use_target(stmt) {
            out.info(format!("Base de datos: {db}"));
            self.db = Some(db);
        }
        let affected = a.columns.len() == 1 && a.columns[0].0 == "affected_rows" && !returns_rows(stmt);
        if affected {
            out.push_affected(a.data.first().and_then(|r| r.first()).and_then(Value::as_u64).unwrap_or(0));
            return Ok(());
        }
        let types: Vec<String> = a.columns.iter().map(|c| c.1.clone()).collect();
        out.begin_result(a.columns.into_iter().map(|(name, type_name)| ResultColumn { name, type_name }).collect());
        for row in a.data {
            out.push_row(row.into_iter().enumerate().map(|(i, v)| cell(v, types.get(i).map_or("", String::as_str))).collect(), max_rows);
        }
        Ok(())
    }

    fn db(&self) -> Result<String> {
        self.db.clone().ok_or_else(|| Error::Query("Elegí una base de datos para ver sus objetos.".into()))
    }

    fn obj_db(&self, obj: &ObjectRef) -> Result<String> {
        obj.schema().map(str::to_string).map_or_else(|| self.db(), Ok)
    }

    async fn plan_text(&self, stmt: &str) -> Result<String> {
        Ok(self.strings(stmt).await?.into_iter().filter_map(|r| r.into_iter().next()).collect::<Vec<_>>().join("\n"))
    }

    /// `(schema, name, is_tag)` columns of a table.
    async fn describe(&self, db: &str, name: &str) -> Result<Vec<(ColumnInfo, bool)>> {
        let rows = self.strings(&format!("DESCRIBE {}", qualified(Some(db), name))).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .filter(|(_, r)| r.len() >= 4)
            .map(|(i, r)| {
                let ty = match r[1].as_str() {
                    t @ ("VARCHAR" | "BINARY" | "NCHAR" | "VARBINARY" | "GEOMETRY") => format!("{t}({})", r[2]),
                    t => t.to_string(),
                };
                let tag = r[3] == "TAG";
                let pk = i == 0 || r[3].contains("PRIMARY KEY");
                (ColumnInfo { name: r[0].clone(), data_type: ty, nullable: !pk && !tag, primary_key: pk, auto_increment: false, default_value: None }, tag)
            })
            .collect())
    }
}

/// A plan from TDengine's `EXPLAIN` text (`-> Operator (k=v …)` lines,
/// `Output: …` details). With ANALYZE each operator carries
/// `cost=first..last` (ms) and `rows=` actual rows; trailing summary lines
/// (`Planning Time`, `Execution Time`) go to the root's props.
fn plan_of(stmt: &str, text: &str, actual: bool) -> Plan {
    let (body, summary): (Vec<&str>, Vec<&str>) =
        text.lines().partition(|l| l.starts_with(' ') || l.trim_start().starts_with("->"));
    let mut root = tree_from_indented_text(&body.join("\n"));
    fn walk(n: &mut PlanNode, actual: bool) {
        if let Some(open) = n.op.find(" (") {
            let detail = n.op[open + 2..].trim_end_matches(')').trim().to_string();
            n.op = n.op[..open].trim().to_string();
            for kv in detail.split_whitespace() {
                let Some((k, v)) = kv.split_once('=') else { continue };
                match k {
                    "rows" if actual => n.actual_rows = v.parse().ok(),
                    "rows" => n.est_rows = v.parse().ok(),
                    "cost" if actual => n.actual_ms = v.split("..").nth(1).and_then(|x| x.parse().ok()),
                    _ => n.props.push((k.to_string(), v.to_string())),
                }
            }
            n.detail = detail;
        }
        if let Some(rest) = n.op.strip_prefix("Table Scan on ").or_else(|| n.op.strip_prefix("Tag Scan on ")) {
            n.object = Some(rest.split_whitespace().next().unwrap_or(rest).to_string());
        }
        for c in &mut n.children {
            walk(c, actual);
        }
    }
    walk(&mut root, actual);
    for s in summary {
        if let Some((k, v)) = s.split_once(':') {
            root.props.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Plan { statement: stmt.to_string(), root, actual, raw_format: "text".into(), raw: text.to_string() }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

#[async_trait]
impl Session for TdSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.strings("SELECT SERVER_VERSION()").await?;
        Ok(format!("TDengine {}", rows.first().and_then(|r| r.first()).cloned().unwrap_or_default()))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.strings("SHOW DATABASES").await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).filter(|d| !SYSTEM_DBS.contains(&d.as_str())).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let db = self.db()?;
        let obj = |kind: &str, name: String, parent: Option<String>| DbObject { kind: kind.into(), schema: Some(db.clone()), name, parent };
        let mut out = Vec::new();
        for r in self.strings(&format!("SELECT stable_name FROM information_schema.ins_stables WHERE db_name = {} ORDER BY 1", lit(&db))).await? {
            out.push(obj(SUPERTABLE, r[0].clone(), None));
        }
        for r in self
            .strings(&format!(
                "SELECT table_name FROM information_schema.ins_tables WHERE db_name = {} AND type = 'NORMAL_TABLE' ORDER BY 1",
                lit(&db)
            ))
            .await?
        {
            out.push(obj(kinds::TABLE, r[0].clone(), None));
        }
        for r in self
            .strings(&format!(
                "SELECT table_name, stable_name FROM information_schema.ins_tables WHERE db_name = {} AND type = 'CHILD_TABLE' LIMIT {MAX_SUBTABLES}",
                lit(&db)
            ))
            .await?
        {
            out.push(obj(SUBTABLE, r[0].clone(), r.get(1).cloned()));
        }
        // Views (Enterprise), streams and topics: missing on some editions.
        for (kind, sql) in [
            (kinds::VIEW, format!("SELECT view_name FROM information_schema.ins_views WHERE db_name = {}", lit(&db))),
            (kinds::STREAM, format!("SELECT stream_name FROM information_schema.ins_streams WHERE source_db = {}", lit(&db))),
            (kinds::TOPIC, format!("SELECT topic_name FROM information_schema.ins_topics WHERE db_name = {}", lit(&db))),
        ] {
            if let Ok(rows) = self.strings(&sql).await {
                out.extend(rows.into_iter().filter_map(|r| r.into_iter().next()).map(|n| obj(kind, n, None)));
            }
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let db = self.obj_db(obj)?;
        Ok(self
            .describe(&db, &obj.name)
            .await?
            .into_iter()
            .map(|(mut c, tag)| {
                if tag {
                    c.data_type.push_str(" TAG");
                }
                c
            })
            .collect())
    }

    /// Supertables (tags as `tag` columns) and normal tables; child
    /// tables are instances of their supertable and don't add a schema.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let db = self.db()?;
        let mut out = Vec::new();
        let objs = self.list_objects().await?;
        for o in objs.into_iter().filter(|o| o.kind == SUPERTABLE || o.kind == kinds::TABLE) {
            let cols = self.describe(&db, &o.name).await?;
            let create = self.strings(&format!("SHOW CREATE {} {}", if o.kind == SUPERTABLE { "STABLE" } else { "TABLE" }, qualified(Some(&db), &o.name))).await?;
            let def = create.first().and_then(|r| r.get(1)).cloned().unwrap_or_default();
            let comment = def.rfind(" COMMENT '").map(|i| {
                let rest = &def[i + 10..];
                let mut s = String::new();
                let mut chars = rest.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            s.push('\'');
                            continue;
                        }
                        break;
                    }
                    s.push(c);
                }
                s
            });
            let mut options = std::collections::BTreeMap::new();
            if let Some(ttl) = def.rfind(" TTL ").map(|i| def[i + 5..].split_whitespace().next().unwrap_or("").to_string()).filter(|t| t != "0") {
                options.insert("ttl".to_string(), ttl);
            }
            out.push(TableSchema {
                kind: o.kind,
                schema: Some(db.clone()),
                name: o.name,
                columns: cols
                    .into_iter()
                    .map(|(c, tag)| ColumnDef {
                        name: c.name,
                        data_type: c.data_type,
                        nullable: true,
                        options: if tag { [(TAG.to_string(), "true".to_string())].into() } else { Default::default() },
                        ..Default::default()
                    })
                    .collect(),
                comment,
                options,
                ..Default::default()
            });
        }
        // Tag indexes of the supertables (not the implicit one on the first tag).
        let rows = self
            .strings(&format!(
                "SELECT index_name, table_name, column_name FROM information_schema.ins_indexes WHERE db_name = {} AND index_type = 'tag_index'",
                ddl::lit(&db)
            ))
            .await
            .unwrap_or_default();
        for r in rows {
            let (Some(ix), Some(tb), Some(col)) = (r.first(), r.get(1), r.get(2)) else { continue };
            let Some(t) = out.iter_mut().find(|t| &t.name == tb) else { continue };
            let first_tag = t.columns.iter().find(|c| c.options.contains_key(TAG)).map(|c| c.name.as_str());
            if !ddl::is_implicit_index(tb, first_tag, ix, col) {
                t.indexes.push(dbine_driver::IndexDef { name: ix.clone(), columns: vec![col.clone()], ..Default::default() });
            }
        }
        for t in &mut out {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(out)
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let db = self.obj_db(obj)?;
        let show = |what: &str| format!("SHOW CREATE {what} {}", qualified(Some(&db), &obj.name));
        let sql = match obj.kind.as_str() {
            SUPERTABLE => show("STABLE"),
            kinds::TABLE | SUBTABLE => show("TABLE"),
            kinds::VIEW => show("VIEW"),
            kinds::STREAM => {
                let rows = self.strings(&format!("SELECT sql FROM information_schema.ins_streams WHERE stream_name = {}", lit(&obj.name))).await?;
                return Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()));
            }
            kinds::TOPIC => {
                let rows = self.strings(&format!("SELECT sql FROM information_schema.ins_topics WHERE topic_name = {}", lit(&obj.name))).await?;
                return Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).map(|s| format!("CREATE TOPIC {} AS {s}", q(&obj.name))));
            }
            _ => return Ok(None),
        };
        let rows = self.strings(&sql).await?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().nth(1)).map(|s| format!("{s};")))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT *\nFROM {}\nLIMIT {limit}", qualified(obj.schema(), &obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let d = dbine_driver::ScriptDialect::generic();
        for unit in dbine_driver::sql::split_script(text, &d) {
            let stmt = dbine_driver::sql::strip_comments(&unit.text, &d, false);
            if stmt.trim().is_empty() {
                continue;
            }
            self.run(stmt.trim(), max_rows, out).await.map_err(|e| script::shift(script::place(e, &unit.text), &unit))?;
        }
        Ok(())
    }

    /// Plans of the reads: `EXPLAIN VERBOSE TRUE` (estimated) or, after
    /// running the statement, `EXPLAIN ANALYZE VERBOSE TRUE` (it runs
    /// again, only reads get there). Other statements have no plan.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        for stmt in split_statements(text) {
            let read = matches!(first_word(&stmt).as_str(), "SELECT" | "WITH");
            if analyze {
                self.run(&stmt, max_rows, out).await?;
            }
            if !read {
                if !analyze {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.chars().take(80).collect::<String>()));
                }
                continue;
            }
            let prefix = if analyze { "EXPLAIN ANALYZE VERBOSE TRUE" } else { "EXPLAIN VERBOSE TRUE" };
            let raw = self.plan_text(&format!("{prefix} {stmt}")).await?;
            out.plans.push(plan_of(&stmt, &raw, analyze));
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
            let Some(stmt) = cancel.current.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
            let conn = conn.clone();
            rt.spawn(async move {
                let find = format!("SELECT kill_id FROM performance_schema.perf_queries WHERE `sql` = {}", lit(&stmt));
                if let Ok(a) = conn.sql(None, &find).await {
                    for r in a.data {
                        if let Some(id) = r.first().map(text) {
                            let _ = conn.sql(None, &format!("KILL QUERY {}", lit(&id))).await;
                        }
                    }
                }
            });
        }))
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.query(&format!("CREATE DATABASE {}", q(name))).await.map(|_| ())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.query(&format!("DROP DATABASE {}", q(name))).await.map(|_| ())
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
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

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        TdSession::processes(self).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_connection_query(id).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        self.kill_connection(id).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        let version = self.strings("SELECT SERVER_VERSION()").await?.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or_default();
        let dnodes = self.records("SHOW DNODES").await.unwrap_or_default();
        let mnodes = self.records("SHOW MNODES").await.unwrap_or_default();
        let cluster = self.records("SHOW CLUSTER").await.unwrap_or_default();
        let vars = self
            .strings("SELECT name, value FROM information_schema.ins_dnode_variables WHERE dnode_id = 1 AND name IN ('timezone', 'numOfCores', 'totalMemoryKB', 'maxShellConns', 'supportVnodes', 'monitor', 'queryPolicy')")
            .await
            .unwrap_or_default();
        let var = |n: &str| vars.iter().find(|r| r.first().map(String::as_str) == Some(n)).and_then(|r| r.get(1)).cloned();
        let conns = match self.records("SELECT * FROM performance_schema.perf_connections").await {
            Ok(r) => Some(r),
            Err(e) => {
                snap.notes.push(format!("No se pudieron leer las conexiones ({e}): hace falta un usuario con permiso sobre performance_schema."));
                None
            }
        };
        let queries = self.records("SELECT * FROM performance_schema.perf_queries").await.ok();
        // taosKeeper's monitor database: the newest row per dnode.
        let mut keeper = self.records("SELECT last_row(*), dnode_id FROM log.taosd_dnodes_info GROUP BY dnode_id").await;
        if keeper.is_err() {
            keeper = self.records("SELECT last_row(*), dnode_id FROM log.dnodes_info GROUP BY dnode_id").await;
        }
        let keeper = match keeper {
            Ok(k) if !k.is_empty() => k,
            _ => {
                snap.notes.push("El CPU, la memoria, el disco y la red salen de la base log que llena taosKeeper; este servidor no la tiene (monitor = 0 o sin taosKeeper).".into());
                Vec::new()
            }
        };
        let k = |col: &str| -> Option<f64> {
            let vals: Vec<f64> = keeper.iter().filter_map(|r| num(r.get(&format!("last_row({col})")))).collect();
            (!vals.is_empty()).then(|| vals.iter().sum())
        };
        let avg = |col: &str| k(col).map(|s| s / keeper.len().max(1) as f64);
        let kb = |v: Option<f64>| v.map(|x| x * 1024.0);
        let mem_total = k("mem_total");
        let usage = self.records("SELECT db_name, sum(data1 + data2 + data3) AS data, sum(wal) AS wal FROM information_schema.ins_disk_usage GROUP BY db_name").await.ok();
        let dbs = self.records("SELECT name, vgroups, ntables, replica, `precision`, status FROM information_schema.ins_databases").await.unwrap_or_default();
        let storage: Option<f64> = usage.as_ref().map(|u| u.iter().filter_map(|r| Some(num(r.get("data")).unwrap_or(0.0) + num(r.get("wal")).unwrap_or(0.0))).sum::<f64>() * 1024.0);
        let tables: f64 = dbs.iter().filter(|r| !SYSTEM_DBS.contains(&r.get("name").map(text).unwrap_or_default().as_str())).filter_map(|r| num(r.get("ntables"))).sum();
        use MetricUnit::*;
        snap.metrics = vec![
            Metric::new("cpu", "CPU del servidor", "CPU", Percent, avg("cpu_system")),
            Metric::new("cpu_engine", "CPU de taosd", "CPU", Percent, avg("cpu_engine")),
            Metric::new("mem_used", "Memoria usada (host)", "Memoria", Bytes, kb(mem_total.zip(k("mem_free")).map(|(t, f)| t - f))).max(kb(mem_total)),
            Metric::new("mem_engine", "Memoria de taosd", "Memoria", Bytes, kb(k("mem_engine"))).max(kb(mem_total)),
            Metric::new("connections", "Conexiones", "Conexiones", Count, conns.as_ref().map(|c| c.len() as f64))
                .max(var("maxShellConns").and_then(|v| dbine_driver::monitor::num(&v))),
            Metric::new("active_sessions", "Consultas en curso", "Conexiones", Count, queries.as_ref().map(|q| q.len() as f64)),
            Metric::new("net_in", "Red entrante (por segundo)", "Red", Bytes, kb(k("system_net_in"))),
            Metric::new("net_out", "Red saliente (por segundo)", "Red", Bytes, kb(k("system_net_out"))),
            Metric::new("disk_read", "Lectura de disco (por segundo)", "Disco", Bytes, kb(k("io_read_disk"))),
            Metric::new("disk_write", "Escritura en disco (por segundo)", "Disco", Bytes, kb(k("io_write_disk"))),
            Metric::new("storage_used", "Espacio de las bases", "Almacenamiento", Bytes, storage),
            Metric::new("disk_used", "Disco del servidor", "Almacenamiento", Bytes, k("disk_used")).max(k("disk_total")),
            Metric::new("tables", "Tablas", "Datos", Count, Some(tables)),
            Metric::new("vnodes", "Vnodes", "Cluster", Count, Some(dnodes.iter().filter_map(|r| num(r.get("vnodes"))).sum()))
                .max(Some(dnodes.iter().filter_map(|r| num(r.get("support_vnodes"))).sum())),
            Metric::new(
                "dnodes_ready",
                "Dnodes listos",
                "Cluster",
                Count,
                Some(dnodes.iter().filter(|r| r.get("status").map(text).as_deref() == Some("ready")).count() as f64),
            )
            .max(Some(dnodes.len() as f64)),
            Metric::new("uptime", "Tiempo activo", "Servidor", Seconds, keeper.first().and_then(|r| num(r.get("last_row(uptime)")))),
        ];

        let mut nodes = MonitorTable::new("nodes", "Dnodes", &["id", "endpoint", "estado", "vnodes", "máx. vnodes", "CPU %", "memoria taosd", "iniciado"]);
        for d in &dnodes {
            let id = d.get("id").map(text).unwrap_or_default();
            let kr = keeper.iter().find(|r| r.get("dnode_id").map(text) == Some(id.clone()));
            nodes.rows.push(vec![
                d.get("id").cloned().unwrap_or(Value::Null),
                d.get("endpoint").cloned().unwrap_or(Value::Null),
                d.get("status").cloned().unwrap_or(Value::Null),
                d.get("vnodes").cloned().unwrap_or(Value::Null),
                d.get("support_vnodes").cloned().unwrap_or(Value::Null),
                kr.and_then(|r| r.get("last_row(cpu_engine)")).cloned().unwrap_or(Value::Null),
                kr.and_then(|r| num(r.get("last_row(mem_engine)"))).map(|v| Value::String(format!("{:.0} MiB", v / 1024.0))).unwrap_or(Value::Null),
                d.get("reboot_time").map(|v| Value::String(timestamp(&text(v)))).unwrap_or(Value::Null),
            ]);
        }
        snap.tables.push(nodes);
        let mut mn = MonitorTable::new("replication", "Mnodes", &["id", "endpoint", "rol", "estado", "rol desde"]);
        for m in &mnodes {
            mn.rows.push(vec![
                m.get("id").cloned().unwrap_or(Value::Null),
                m.get("endpoint").cloned().unwrap_or(Value::Null),
                m.get("role").cloned().unwrap_or(Value::Null),
                m.get("status").cloned().unwrap_or(Value::Null),
                m.get("role_time").map(|v| Value::String(timestamp(&text(v)))).unwrap_or(Value::Null),
            ]);
        }
        snap.tables.push(mn);
        if let Some(c) = &conns {
            let mut t = MonitorTable::new("sessions", "Sesiones", &["id", "usuario", "aplicación", "cliente", "inicio", "último acceso"]);
            for r in c.iter().take(200) {
                let app = r.get("user_app").map(text).filter(|s| !s.is_empty()).or_else(|| r.get("app").map(text)).unwrap_or_default();
                let client = r.get("user_ip").map(text).filter(|s| !s.is_empty()).or_else(|| r.get("end_point").map(text)).unwrap_or_default();
                t.rows.push(vec![
                    r.get("conn_id").cloned().unwrap_or(Value::Null),
                    r.get("user").cloned().unwrap_or(Value::Null),
                    Value::String(app),
                    Value::String(client),
                    r.get("login_time").map(|v| Value::String(timestamp(&text(v)))).unwrap_or(Value::Null),
                    r.get("last_access").map(|v| Value::String(timestamp(&text(v)))).unwrap_or(Value::Null),
                ]);
            }
            snap.tables.push(t);
        }
        if let Some(qs) = &queries {
            let mut t = MonitorTable::new("queries", "Consultas en curso", &["kill_id", "usuario", "aplicación", "duración (ms)", "subconsultas", "consulta"]);
            for r in qs.iter().take(200) {
                t.rows.push(vec![
                    r.get("kill_id").cloned().unwrap_or(Value::Null),
                    r.get("user").cloned().unwrap_or(Value::Null),
                    r.get("app").cloned().unwrap_or(Value::Null),
                    num(r.get("exec_usec")).map(|u| Value::from((u / 1000.0).round())).unwrap_or(Value::Null),
                    r.get("sub_num").cloned().unwrap_or(Value::Null),
                    Value::String(r.get("sql").map(text).unwrap_or_default().chars().take(2000).collect()),
                ]);
            }
            snap.tables.push(t);
        }
        let mut t = MonitorTable::new("databases", "Bases y tamaños", &["base", "vgroups", "tablas", "réplicas", "precisión", "datos", "WAL"]);
        for d in dbs.iter().filter(|r| !SYSTEM_DBS.contains(&r.get("name").map(text).unwrap_or_default().as_str())) {
            let name = d.get("name").map(text).unwrap_or_default();
            let u = usage.as_ref().and_then(|u| u.iter().find(|r| r.get("db_name").map(text).as_deref() == Some(name.as_str())));
            let size = |col: &str| u.and_then(|r| num(r.get(col))).map(|kb| Value::from((kb * 1024.0) as u64)).unwrap_or(Value::Null);
            t.rows.push(vec![
                Value::String(name.clone()),
                d.get("vgroups").cloned().unwrap_or(Value::Null),
                d.get("ntables").cloned().unwrap_or(Value::Null),
                d.get("replica").cloned().unwrap_or(Value::Null),
                d.get("precision").cloned().unwrap_or(Value::Null),
                size("data"),
                size("wal"),
            ]);
        }
        snap.tables.push(t);
        if usage.is_none() {
            snap.notes.push("El tamaño por base sale de information_schema.ins_disk_usage (TDengine 3.3.5 o posterior).".into());
        }

        snap.info.push(("Versión".into(), version));
        if let Some(c) = cluster.first() {
            snap.info.push(("Edición".into(), c.get("version").map(text).unwrap_or_default()));
            snap.info.push(("Cluster".into(), c.get("name").map(text).unwrap_or_default()));
        }
        for (label, name) in [("Zona horaria", "timezone"), ("Núcleos", "numOfCores"), ("Memoria total (KB)", "totalMemoryKB"), ("Máx. conexiones", "maxShellConns"), ("Monitor (taosKeeper)", "monitor")] {
            if let Some(v) = var(name) {
                snap.info.push((label.into(), v));
            }
        }
        if keeper.len() > 0 && dnodes.len() > keeper.len() {
            snap.notes.push("Algunos dnodes todavía no reportaron a taosKeeper.".into());
        }
        snap.notes.push("TDengine no expone contadores de consultas o filas por segundo por SQL; la red y el disco son tasas por segundo que calcula taosKeeper en cada intervalo.".into());
        Ok(snap)
    }

    /// The user's own row of `SHOW USERS` (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

/// The browse query restricted by the grid's column filters. Identifiers
/// go in backticks (double quotes are strings in TDengine) and literals
/// take backslash escapes, so LIKE patterns rely on the default `\`
/// escape: no ESCAPE clause.
fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &ddl::literal, like: "LIKE", true_literal: "true", false_literal: "false" };
    let mut parts = Vec::new();
    for f in filters {
        let c = sql_condition(std::slice::from_ref(f), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => s.to_string(),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_browse_in_tdengine_syntax() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT *\nFROM `iot`.`d1`\nLIMIT 200",
                &[
                    f("loc", FilterOp::Eq, vec![json!("O'Brien\\x")]),
                    f("loc", FilterOp::Contains, vec![json!("a_b")]),
                    f("v", FilterOp::Ge, vec![json!(1.5)]),
                    f("v", FilterOp::NotNull, vec![]),
                    f("gid", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM `iot`.`d1`\nWHERE `loc` = 'O''Brien\\\\x'\n  AND `loc` LIKE '%a\\\\_b%'\n  AND `v` >= 1.5\n  AND `v` IS NOT NULL\n  AND `gid` IN (1, 2)\nLIMIT 200"
        );
    }

    #[test]
    fn cells_and_timestamps() {
        assert_eq!(cell(json!("2024-01-31T13:45:00.123Z"), "TIMESTAMP"), json!("2024-01-31 13:45:00.123"));
        assert_eq!(cell(json!("2024-01-31T10:45:00.123-03:00"), "TIMESTAMP"), json!("2024-01-31 10:45:00.123 -03:00"));
        assert_eq!(cell(json!("cafe"), "VARBINARY"), json!("0xCAFE"));
        assert_eq!(cell(json!(18446744073709551615u64), "BIGINT UNSIGNED"), json!("18446744073709551615"));
        assert_eq!(cell(json!("1.25"), "DECIMAL(10,2)"), json!("1.25"));
    }

    #[test]
    fn statements() {
        assert!(returns_rows(" select 1") && returns_rows("SHOW DATABASES") && !returns_rows("insert into t values (now, 1)"));
        assert_eq!(use_target("USE `power`").as_deref(), Some("power"));
        assert_eq!(use_target("select 1"), None);
        assert!(matches!(td_error(855, "Authentication failure".into()), Error::AuthFailed(_)));
        assert!(matches!(td_error(9750, "x".into()), Error::Statement(e) if e.code.as_deref() == Some("0x2616")));
    }

    #[test]
    fn analyze_plan_text() {
        let t = "-> Data Exchange 2:1 (cost=0.018..0.500 rows=2 width=64)\n   -> Projection (cost=0.136..0.136 rows=2 columns=5 width=64 input_order=asc )\n      -> Table Scan on st (cost=0.000..0.136 rows=2 columns=3 width=64 order=[asc|1 desc|0] mode=ts_order data_load=data)\n            I/O: total_blocks=0.5 load_blocks=0.5\nPlanning Time: 0.207 ms\nExecution Time: 1.785 ms";
        let p = plan_of("select * from st", t, true);
        assert_eq!(p.root.op, "Data Exchange 2:1");
        assert_eq!(p.root.actual_rows, Some(2.0));
        assert_eq!(p.root.actual_ms, Some(0.5));
        assert!(p.root.props.iter().any(|(k, v)| k == "Execution Time" && v == "1.785 ms"));
        let scan = &p.root.children[0].children[0];
        assert_eq!(scan.op, "Table Scan on st");
        assert_eq!(scan.object.as_deref(), Some("st"));
        assert!(scan.props.iter().any(|(k, _)| k == "I/O"));
        let e = plan_of("s", "-> Aggregate (functions=2 width=30 input_order=asc )\n      Output: columns=2 width=30 blocking=1", false);
        assert_eq!(e.root.op, "Aggregate");
        assert!(e.root.props.iter().any(|(k, _)| k == "Output"));
    }

    #[test]
    fn driver_info() {
        let d = drivers();
        assert_eq!(d[0].info().id, "tdengine");
        assert!(d[0].capabilities().monitor && d[0].capabilities().create_database && d[0].supports_explain());
    }
}

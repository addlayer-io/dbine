//! InfluxDB 3 (Core, Enterprise) with SQL: `POST /api/v3/query_sql`, one
//! statement per request, rows as JSON lines. Tables are the measurements.

use crate::http::{self, send_err};
use crate::monitor;
use crate::plan;
use crate::processes;
use crate::profiler;
use crate::v1::cell;
use dbine_driver::{
    async_trait, kinds, ColumnDef, ColumnInfo, ConnectionConfig, DbObject, DriverInfo, Error, Family, Field, FieldKind,
    KeyDef, Language, Metric, MetricUnit, MonitorSnapshot, MonitorTable, ObjectKindInfo, ObjectRef, Plan, QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{json, Value as J};

pub fn info() -> DriverInfo {
    DriverInfo {
        id: "influxdb3",
        name: "InfluxDB 3 (SQL)",
        family: Family::TimeSeries,
        language: Language::Sql,
        dialect: "influxdb3",
        default_port: 8181,
        fields: vec![
            Field::host().help("Servidor o URL."),
            Field::port().placeholder("8181"),
            Field::new("database", "Base de datos", FieldKind::Text).placeholder("(ninguna)"),
            Field::new("token", "Token", FieldKind::Password).secret().help("Vacío si el servidor corre sin autenticación."),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::MEASUREMENT, "Tablas", true, true, false)],
    }
}

pub struct SqlSession {
    pub(crate) http: reqwest::Client,
    pub(crate) base: String,
    token: String,
    pub(crate) db: Option<String>,
    read_only: bool,
    /// The running profiler, if any.
    profiler: Option<profiler::V3State>,
}

pub async fn connect(cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
    let db = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
    let token = cfg.option("token").or(cfg.password.as_deref().filter(|p| !p.is_empty())).unwrap_or("").to_string();
    let s = SqlSession {
        http: http::client(cfg)?,
        base: http::base_url(cfg, 8181),
        token,
        db: db.map(str::to_string),
        read_only: cfg.read_only,
        profiler: None,
    };
    s.databases().await.map_err(|e| match e {
        Error::Query(m) => Error::Connect(m),
        e => e,
    })?;
    Ok(Box::new(s))
}

/// A JSON object with its keys in the server's order (serde_json's map
/// sorts them unless the whole build enables `preserve_order`).
pub struct OrderedRow(pub Vec<(String, J)>);

impl<'de> Deserialize<'de> for OrderedRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedRow;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<OrderedRow, A::Error> {
                let mut v = Vec::new();
                while let Some((k, val)) = m.next_entry::<String, J>()? {
                    v.push((k, val));
                }
                Ok(OrderedRow(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// JSON lines as a table: columns in order of appearance (a row leaves out
/// its nulls), then one row per line.
pub fn jsonl_table(body: &str) -> Result<(Vec<String>, Vec<Vec<J>>)> {
    let mut rows: Vec<OrderedRow> = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        rows.push(serde_json::from_str(line)?);
    }
    let mut columns: Vec<String> = Vec::new();
    for r in &rows {
        for (k, _) in &r.0 {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let table = rows
        .into_iter()
        .map(|r| {
            let mut cells = vec![J::Null; columns.len()];
            for (k, v) in r.0 {
                if let Some(i) = columns.iter().position(|c| *c == k) {
                    cells[i] = cell(&v);
                }
            }
            cells
        })
        .collect();
    Ok((columns, table))
}

/// Tables from `information_schema.columns` rows (table, column, type,
/// nullable) in table / ordinal order. Tags (dictionary-encoded strings)
/// and `time` key the series, as in [`Session::columns`].
pub fn schema_tables(rows: &[Vec<J>]) -> Vec<TableSchema> {
    let mut out: Vec<TableSchema> = Vec::new();
    for r in rows {
        let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let (table, name, data_type) = (s(0), s(1), s(2));
        if out.last().is_none_or(|t| t.name != table) {
            out.push(TableSchema {
                kind: kinds::MEASUREMENT.into(),
                name: table,
                primary_key: Some(KeyDef { name: None, columns: Vec::new() }),
                ..Default::default()
            });
        }
        let t = out.last_mut().unwrap();
        let tag = data_type.starts_with("Dictionary");
        let mut options = std::collections::BTreeMap::new();
        if tag {
            options.insert("tag".to_string(), "true".to_string());
        }
        if tag || name == "time" {
            t.primary_key.as_mut().unwrap().columns.push(name.clone());
        }
        t.columns.push(ColumnDef { nullable: s(3) != "NO" && !tag && name != "time", name, data_type, options, ..Default::default() });
    }
    out
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl SqlSession {
    pub(crate) fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            req
        } else {
            req.bearer_auth(&self.token)
        }
    }

    async fn databases(&self) -> Result<Vec<String>> {
        let req = self.http.get(format!("{}/api/v3/configure/database", self.base)).query(&[("format", "json")]);
        let body = http::text(self.auth(req).send().await.map_err(send_err)?).await?;
        let v: J = serde_json::from_str(&body)?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d.as_object()?.values().next()?.as_str().map(str::to_string))
            .collect())
    }

    /// A call to the database configuration API.
    pub(crate) async fn configure(&self, req: reqwest::RequestBuilder, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se pueden {what} bases.")));
        }
        http::text(self.auth(req).send().await.map_err(send_err)?).await?;
        Ok(())
    }

    pub(crate) async fn sql(&self, q: &str) -> Result<(Vec<String>, Vec<Vec<J>>)> {
        let db = self.db.as_deref().ok_or_else(|| Error::Query("No hay una base de datos seleccionada.".into()))?;
        let req = self.http.post(format!("{}/api/v3/query_sql", self.base)).json(&json!({ "db": db, "q": q, "format": "jsonl" }));
        let body = http::text(self.auth(req).send().await.map_err(send_err)?).await?;
        jsonl_table(&body)
    }
}

#[async_trait]
impl Session for SqlSession {
    /// A `SELECT` streamed as JSON lines (see `transfer.rs`).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let db = self.db.clone().ok_or_else(|| Error::Query("No hay una base de datos seleccionada.".into()))?;
        let cols = crate::transfer::read_columns(&self.columns(&spec.table).await?, &spec.columns)?;
        let quote = |n: &str| dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, n);
        let list = match &spec.columns {
            Some(c) => c.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", "),
            None => "*".to_string(),
        };
        let mut q = format!("SELECT {list} FROM {}", quote(&spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            q.push_str(&format!(" WHERE {f}"));
        }
        let req = self.http.post(format!("{}/api/v3/query_sql", self.base)).json(&json!({ "db": db, "q": q, "format": "jsonl" }));
        crate::transfer::read_jsonl(self.auth(req), &cols, &sink).await
    }

    /// Line protocol to `/api/v3/write_lp`, all or nothing per request
    /// (see `transfer.rs`).
    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden escribir puntos.".into()));
        }
        let db = self.db.clone().ok_or_else(|| Error::Query("No hay una base de datos seleccionada.".into()))?;
        let target = self.columns(&spec.table).await?;
        let params = [("db", db.as_str()), ("precision", "nanosecond"), ("accept_partial", "false")];
        let url = reqwest::Url::parse_with_params(&format!("{}/api/v3/write_lp", self.base), &params).map_err(|e| Error::Query(e.to_string()))?;
        let auth = if self.token.is_empty() { crate::transfer::Auth::None } else { crate::transfer::Auth::Bearer(self.token.clone()) };
        let ep = crate::transfer::Endpoint { http: self.http.clone(), url: url.into(), auth, api: crate::Api::Sql };
        crate::transfer::load(ep, &spec.table.name, &target, spec, columns, source, progress).await
    }

    async fn server_version(&mut self) -> Result<String> {
        let req = self.http.get(format!("{}/ping", self.base));
        let resp = self.auth(req).send().await.map_err(send_err)?;
        let header = resp.headers().get("X-Influxdb-Version").and_then(|v| v.to_str().ok()).map(str::to_string);
        let v: J = serde_json::from_str(&http::text(resp).await.unwrap_or_default()).unwrap_or_default();
        let version = v.get("version").and_then(|v| v.as_str()).map(str::to_string).or(header).unwrap_or_default();
        let product = v.get("product_name").and_then(|p| p.as_str()).unwrap_or("InfluxDB 3");
        Ok(format!("{product} {version}").trim().to_string())
    }

    /// `/metrics`, `/ping` and the `system` tables of a database (the
    /// connection's, or the first one): running and recent queries, and
    /// Parquet files per table.
    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        Err(Error::Unsupported(crate::security::TOKENS.into()))
    }

    async fn grants(&mut self, _principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        Err(Error::Unsupported(crate::security::TOKENS.into()))
    }

    /// `system.queries` is read through a database, the session's or the
    /// first one, as the monitor does.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        let saved = self.db.clone();
        if self.db.is_none() {
            self.db = self.databases().await?.into_iter().find(|d| !d.starts_with('_'));
            if self.db.is_none() {
                self.db = saved;
                return Ok(Vec::new());
            }
        }
        let r = tokio::time::timeout(processes::QUERY_LIMIT, self.sql(processes::V3_QUERIES)).await;
        self.db = saved;
        let (cols, rows) = r.map_err(|_| Error::Query("system.queries no respondió a tiempo".into()))??;
        Ok(processes::v3_rows(&cols, &rows, chrono::Utc::now().timestamp_millis()))
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Unsupported(processes::V3_NO_CANCEL.into()))
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let resp = self.auth(self.http.get(format!("{}/metrics", self.base))).send().await.map_err(send_err)?;
        let prom = http::text(resp).await.ok().map(|t| monitor::Prom::parse(&t));
        let ping = match self.auth(self.http.get(format!("{}/ping", self.base))).send().await {
            Ok(r) => http::text(r).await.ok().and_then(|b| serde_json::from_str::<J>(&b).ok()),
            Err(_) => None,
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut s = monitor::v3_snapshot(prom.as_ref(), ping.as_ref(), now);

        let dbs = self.databases().await.unwrap_or_default();
        let db = self.db.clone().or_else(|| dbs.iter().find(|d| !d.starts_with('_')).cloned());
        if let Some(i) = s.metrics.iter().position(|m| m.key == "uptime") {
            let n = dbs.iter().filter(|d| !d.starts_with('_')).count() as f64;
            s.metrics.insert(i, Metric::new("databases", "Bases de datos", "Almacenamiento", MetricUnit::Count, Some(n)));
        }
        let Some(db) = db else {
            s.notes.push("No hay bases de datos: las consultas en curso y el tamaño de los archivos se leen de las tablas system de una base.".into());
            return Ok(s);
        };
        let saved = self.db.replace(db.clone());
        let q = "SELECT id, query_type, phase, issue_time, end2end_duration, max_memory, query_text, running \
                 FROM system.queries ORDER BY issue_time DESC LIMIT 200";
        match self.sql(q).await {
            Ok((cols, rows)) => {
                let ri = cols.iter().position(|c| c == "running");
                let running = |r: &&Vec<J>| ri.and_then(|i| r.get(i)).and_then(|v| v.as_bool()).unwrap_or(false);
                let own = |r: &&Vec<J>| r.iter().any(|v| v.as_str().is_some_and(|t| t.contains("FROM system.queries ORDER BY issue_time")));
                let now_running: Vec<Vec<J>> = rows.iter().filter(running).filter(|r| !own(r)).cloned().collect();
                let recent: Vec<Vec<J>> = rows.iter().filter(|r| !own(r)).take(20).cloned().collect();
                s.tables.push(monitor::v3_queries_table(&cols, &now_running, true));
                s.tables.push(monitor::v3_queries_table(&cols, &recent, false));
            }
            Err(e) => s.notes.push(format!("No se pudo leer system.queries: {e}")),
        }
        let q = "SELECT table_name, count(*) AS files, sum(row_count) AS row_count, sum(size_bytes) AS size_bytes \
                 FROM system.parquet_files GROUP BY table_name ORDER BY size_bytes DESC LIMIT 20";
        match self.sql(q).await {
            Ok((cols, rows)) => {
                let at = |r: &Vec<J>, n: &str| cols.iter().position(|c| c == n).and_then(|i| r.get(i)).cloned().unwrap_or(J::Null);
                let mut t = MonitorTable::new(
                    "top_objects",
                    &format!("Tablas más grandes de «{db}» (Parquet)"),
                    &["tabla", "archivos", "filas", "tamaño (bytes)"],
                );
                let mut total = 0.0;
                for r in &rows {
                    total += at(r, "size_bytes").as_f64().unwrap_or(0.0);
                    t.rows.push(vec![at(r, "table_name"), at(r, "files"), at(r, "row_count"), at(r, "size_bytes")]);
                }
                let label = format!("Parquet persistido de «{db}»");
                let i = s.metrics.iter().position(|m| m.key == "databases").unwrap_or(s.metrics.len());
                s.metrics.insert(i, Metric::new("storage_used", &label, "Almacenamiento", MetricUnit::Bytes, Some(total)));
                s.tables.push(t);
                s.notes.push(format!(
                    "El espacio usado es el de los archivos Parquet ya persistidos de «{db}»; los datos que todavía están en memoria (WAL) no suman."
                ));
            }
            Err(e) => s.notes.push(format!("No se pudo leer system.parquet_files: {e}")),
        }
        self.db = saved;
        Ok(s)
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::v3_start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::v3_poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut v: Vec<String> = self.databases().await?.into_iter().filter(|d| !d.starts_with('_')).collect();
        v.sort();
        Ok(v)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        if self.db.is_none() {
            return Ok(Vec::new());
        }
        let (_, rows) = self
            .sql("SELECT table_name FROM information_schema.tables WHERE table_schema = 'iox' ORDER BY table_name")
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.first()?.as_str().map(str::to_string))
            .map(|name| DbObject { kind: kinds::MEASUREMENT.into(), schema: None, name, parent: None })
            .collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let (_, rows) = self
            .sql(&format!(
                "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
                 WHERE table_schema = 'iox' AND table_name = {} ORDER BY ordinal_position",
                sql_str(&obj.name)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let (name, data_type) = (s(0), s(1));
                // Tags are dictionary-encoded strings; with time they key the series.
                let key = name == "time" || data_type.starts_with("Dictionary");
                ColumnInfo {
                    nullable: s(2) != "NO" && !key,
                    primary_key: key,
                    auto_increment: false,
                    default_value: None,
                    name,
                    data_type,
                }
            })
            .collect())
    }

    async fn definition(&mut self, _obj: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        if self.db.is_none() {
            return Ok(Vec::new());
        }
        let (_, rows) = self
            .sql(
                "SELECT table_name, column_name, data_type, is_nullable FROM information_schema.columns \
                 WHERE table_schema = 'iox' ORDER BY table_name, ordinal_position",
            )
            .await?;
        Ok(schema_tables(&rows))
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.create_database_with(name, &std::collections::BTreeMap::new()).await
    }

    /// With the retention period (see [`crate::create_db`]).
    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let body = crate::create_db::database(name, options)?;
        let url = format!("{}{}", self.base, crate::create_db::V3_PATH);
        self.configure(self.http.post(url).json(&body), "crear").await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.db.as_deref() == Some(name) {
            return Err(Error::Query(format!("No se puede borrar «{name}»: es la base de datos de esta conexión.")));
        }
        let url = format!("{}/api/v3/configure/database", self.base);
        self.configure(self.http.delete(url).query(&[("db", name)]), "borrar").await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let t = dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, &obj.name);
        format!("SELECT *\nFROM {t}\nORDER BY time DESC\nLIMIT {limit}")
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for stmt in dbine_driver::sql::split_statements(text) {
            let (columns, rows) = self.sql(&stmt).await?;
            out.begin_result(columns.into_iter().map(|name| ResultColumn { name, type_name: String::new() }).collect());
            for row in rows {
                out.push_row(row, max_rows);
            }
        }
        Ok(())
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for stmt in dbine_driver::sql::split_statements(text) {
            let head = stmt.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
            if analyze {
                self.execute(&stmt, max_rows, out).await?;
            }
            if !matches!(head.as_str(), "select" | "with") {
                if !analyze {
                    out.messages.push(format!("Sin plan para «{}»: solo se explican consultas SELECT.", stmt.trim()));
                }
                continue;
            }
            let (columns, rows) = self.sql(&format!("EXPLAIN {}{stmt}", if analyze { "ANALYZE " } else { "" })).await?;
            let col = |n: &str| columns.iter().position(|c| c == n);
            let (Some(ty), Some(pl)) = (col("plan_type"), col("plan")) else {
                return Err(Error::Query("EXPLAIN no devolvió las columnas plan_type y plan".into()));
            };
            let s = |r: &Vec<J>, i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let parts: Vec<(String, String)> = rows.iter().map(|r| (s(r, ty), s(r, pl))).collect();
            // The physical plan is what runs (with metrics when analyzed);
            // the logical one goes along in the raw text.
            let main = parts
                .iter()
                .find(|(t, _)| t == "physical_plan" || t == "Plan with Metrics")
                .or_else(|| parts.last())
                .map(|(_, p)| p.clone())
                .unwrap_or_default();
            let root = plan::datafusion_tree(&main).unwrap_or_default();
            let raw = parts.iter().map(|(t, p)| format!("{t}:\n{}", p.trim_end())).collect::<Vec<_>>().join("\n\n");
            out.plans.push(Plan { statement: stmt.clone(), root, actual: analyze, raw_format: "text".into(), raw });
        }
        Ok(())
    }

    /// Admin token or not, from the admin-only `system.tokens` (see
    /// `permissions`).
    /// "Propiedades" (see [`crate::properties`]).
    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        let q = "SELECT name FROM system.tokens LIMIT 1";
        let req = self.http.post(format!("{}/api/v3/query_sql", self.base)).json(&json!({ "db": "_internal", "q": q, "format": "json" }));
        let answer = match self.auth(req).send().await {
            Ok(resp) => http::text(resp).await,
            Err(e) => Err(send_err(e)),
        };
        crate::permissions::v3(answer, database)
    }
}

/// The browse query restricted by the grid's column filters (DataFusion
/// SQL). LIKE patterns rely on the default `\` escape: no ESCAPE clause.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: dbine_driver::sql::Quote::Double, literal: &sql_literal, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
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

fn sql_literal(v: &J) -> String {
    dbine_driver::ddl::sql_literal(&dbine_driver::ddl::SqlFlavor::ansi(), v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_browse_before_order_by() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<J>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT *\nFROM \"cpu\"\nORDER BY time DESC\nLIMIT 200",
                &[
                    f("host", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("path", FilterOp::Contains, vec![json!("50%")]),
                    f("usage", FilterOp::Lt, vec![json!(3)]),
                    f("region", FilterOp::IsNull, vec![]),
                    f("id", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM \"cpu\"\nWHERE \"host\" = 'O''Brien'\n  AND \"path\" LIKE '%50\\%%'\n  AND \"usage\" < 3\n  AND \"region\" IS NULL\n  AND \"id\" IN (1, 2)\nORDER BY time DESC\nLIMIT 200"
        );
    }

    #[test]
    fn schema_from_information_schema() {
        let r = |t: &str, c: &str, ty: &str, n: &str| vec![json!(t), json!(c), json!(ty), json!(n)];
        let t = schema_tables(&[
            r("cpu", "host", "Dictionary(Int32, Utf8)", "YES"),
            r("cpu", "time", "Timestamp(Nanosecond, None)", "NO"),
            r("cpu", "value", "Float64", "YES"),
            r("mem", "time", "Timestamp(Nanosecond, None)", "NO"),
        ]);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].primary_key.as_ref().unwrap().columns, ["host", "time"]);
        assert_eq!(t[0].columns[0].options["tag"], "true");
        assert!(!t[0].columns[0].nullable && t[0].columns[2].nullable);
        assert_eq!(t[1].columns.len(), 1);
    }

    #[test]
    fn json_lines_keep_column_order_and_fill_nulls() {
        let (cols, rows) = jsonl_table(
            "{\"time\":\"2024-01-31T13:45:00\",\"zeta\":1,\"alpha\":\"a\"}\n{\"time\":\"2024-01-31T13:46:00\",\"beta\":2.5}\n",
        )
        .unwrap();
        assert_eq!(cols, ["time", "zeta", "alpha", "beta"]);
        assert_eq!(rows[0], vec![json!("2024-01-31 13:45:00"), json!(1), json!("a"), J::Null]);
        assert_eq!(rows[1], vec![json!("2024-01-31 13:46:00"), J::Null, J::Null, json!(2.5)]);
        assert!(jsonl_table("").unwrap().0.is_empty());
    }
}

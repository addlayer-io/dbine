//! InfluxDB 2.x (and Cloud 2) with Flux: `POST /api/v2/query` answered in
//! annotated CSV. Buckets are the databases, measurements the objects.

use crate::csv;
use crate::monitor;
use crate::plan;
use crate::http::{self, send_err};
use dbine_driver::{
    async_trait, kinds, ColumnDef, ColumnInfo, ConnectionConfig, DbObject, DriverInfo, Error, Family, Field, FieldKind,
    KeyDef, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, Plan, QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use serde_json::json;
use std::collections::BTreeMap;

pub fn info() -> DriverInfo {
    DriverInfo {
        id: "influxdb",
        name: "InfluxDB 2 (Flux)",
        family: Family::TimeSeries,
        language: Language::Flux,
        dialect: "",
        default_port: 8086,
        fields: vec![
            Field::host().help("Servidor o URL (https://… para InfluxDB Cloud)."),
            Field::port().placeholder("8086"),
            Field::new("org", "Organización", FieldKind::Text).required(),
            Field::new("token", "Token", FieldKind::Password).required().secret(),
            Field::new("database", "Bucket", FieldKind::Text).placeholder("(ninguno)"),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Buckets",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::MEASUREMENT, "Measurements", true, true, false)],
    }
}

pub struct FluxSession {
    pub(crate) http: reqwest::Client,
    pub(crate) base: String,
    pub(crate) org: String,
    token: String,
    bucket: Option<String>,
    read_only: bool,
}

pub async fn connect(cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
    let org = cfg.option("org").ok_or_else(|| Error::Connect("Falta la organización.".into()))?.to_string();
    let token = cfg.option("token").or(cfg.password.as_deref().filter(|p| !p.is_empty())).unwrap_or("").to_string();
    let bucket = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
    let s = FluxSession {
        http: http::client(cfg)?,
        base: http::base_url(cfg, 8086),
        org,
        token,
        bucket: bucket.map(str::to_string),
        read_only: cfg.read_only,
    };
    // Checks the token and the organization in one call.
    s.get("/api/v2/buckets", &[("org", s.org.as_str()), ("limit", "1")]).await.map_err(|e| match e {
        Error::Query(m) => Error::Connect(m),
        e => e,
    })?;
    Ok(Box::new(s))
}

/// A Flux string literal.
pub fn flux_str(s: &str) -> String {
    format!("\"{}\"", http::escape(s, '"').replace("${", "\\${"))
}

/// Flux calls that write or send data somewhere.
const WRITES: &[&str] = &["to", "wideTo", "post", "message", "sendEvent", "endpoint", "publish", "exec", "delete"];

/// The first writing call of a script (outside strings and comments).
pub fn first_write(script: &str) -> Option<String> {
    let mut code = String::with_capacity(script.len());
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(n) = chars.next() {
                    if n == '\\' {
                        chars.next();
                    } else if n == '"' {
                        break;
                    }
                }
                code.push_str("\"\"");
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                code.push('\n');
            }
            c => code.push(c),
        }
    }
    let b: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_alphabetic() || b[i] == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            let word: String = b[start..i].iter().collect();
            let mut j = i;
            while j < b.len() && b[j].is_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == '(' && WRITES.contains(&word.as_str()) {
                return Some(word);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// One Flux script with the tag and field keys of every measurement, as
/// `(measurement, "tag" | "field", key)` rows.
pub fn keys_script(bucket: &str, measurements: &[String]) -> String {
    let b = flux_str(bucket);
    let mut s = String::from("import \"influxdata/influxdb/schema\"\n");
    let mut names = Vec::new();
    for (i, m) in measurements.iter().enumerate() {
        let m = flux_str(m);
        for (f, kind) in [("measurementTagKeys", "tag"), ("measurementFieldKeys", "field")] {
            let v = format!("{}{i}", &kind[..1]);
            s.push_str(&format!(
                "{v} = schema.{f}(bucket: {b}, measurement: {m}, {SCHEMA_RANGE})\n  \
                 |> map(fn: (r) => ({{m: {m}, k: \"{kind}\", _value: string(v: r._value)}}))\n"
            ));
            names.push(v);
        }
    }
    s.push_str(&format!("union(tables: [{}])\n  |> group()", names.join(", ")));
    s
}

impl FluxSession {
    pub(crate) async fn send(&self, req: reqwest::RequestBuilder) -> Result<serde_json::Value> {
        let resp = req.header("Authorization", format!("Token {}", self.token)).send().await.map_err(send_err)?;
        let body = http::text(resp).await?;
        Ok(serde_json::from_str(&body).unwrap_or_default())
    }

    pub(crate) fn check_writable(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se pueden {what} buckets.")));
        }
        Ok(())
    }

    pub(crate) async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Token {}", self.token))
            .query(query)
            .send()
            .await
            .map_err(send_err)?;
        Ok(serde_json::from_str(&http::text(resp).await?)?)
    }

    async fn flux(&self, script: &str) -> Result<Vec<csv::Table>> {
        let body = json!({
            "query": script,
            "type": "flux",
            "dialect": { "header": true, "annotations": ["datatype", "group", "default"], "delimiter": "," },
        });
        let resp = self
            .http
            .post(format!("{}/api/v2/query", self.base))
            .query(&[("org", self.org.as_str())])
            .header("Authorization", format!("Token {}", self.token))
            .header("Accept", "application/csv")
            .json(&body)
            .send()
            .await
            .map_err(send_err)?;
        csv::parse(&http::text(resp).await?).map_err(Error::Query)
    }

    /// Every `_value` of a schema function's answer.
    async fn values(&self, script: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for t in self.flux(script).await? {
            if let Some(i) = t.columns.iter().position(|c| c == "_value") {
                out.extend(t.rows.iter().filter_map(|r| r[i].as_str().map(str::to_string)));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn bucket(&self) -> Result<&str> {
        self.bucket.as_deref().ok_or_else(|| Error::Query("No hay un bucket seleccionado.".into()))
    }
}

/// Where schema lookups look: the whole history, before 1970 and future
/// points included.
const SCHEMA_RANGE: &str = crate::transfer::FLUX_ALL_TIME;

#[async_trait]
impl Session for FluxSession {
    /// The measurement pivoted, streamed as annotated CSV (see `transfer.rs`).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let catalog = self.columns(&spec.table).await?;
        let cols = crate::transfer::read_columns(&catalog, &spec.columns)?;
        let tags: Vec<&str> = catalog.iter().filter(|c| c.data_type == "tag").map(|c| c.name.as_str()).collect();
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        // Strings the CSV can't give back take a slower script: look first.
        let check = crate::transfer::flux_check_script(self.bucket()?, &spec.table.name, &tags);
        let lossy = self.flux(&check).await.map_err(crate::transfer::flux_err)?.iter().any(|t| !t.rows.is_empty());
        let script = crate::transfer::flux_read_script(self.bucket()?, &spec.table.name, lossy, filter);
        let body = json!({
            "query": script,
            "type": "flux",
            "dialect": { "header": true, "annotations": ["datatype", "group", "default"], "delimiter": "," },
        });
        let req = self
            .http
            .post(format!("{}/api/v2/query", self.base))
            .query(&[("org", self.org.as_str())])
            .header("Authorization", format!("Token {}", self.token))
            .header("Accept", "application/csv")
            .json(&body);
        crate::transfer::read_flux(req, &cols, &sink).await
    }

    /// Line protocol to `/api/v2/write` (see `transfer.rs`).
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
        let bucket = self.bucket()?.to_string();
        let target = self.columns(&spec.table).await?;
        let params = [("org", self.org.as_str()), ("bucket", bucket.as_str()), ("precision", "ns")];
        let url = reqwest::Url::parse_with_params(&format!("{}/api/v2/write", self.base), &params).map_err(|e| Error::Query(e.to_string()))?;
        let auth = crate::transfer::Auth::Token(self.token.clone());
        let ep = crate::transfer::Endpoint { http: self.http.clone(), url: url.into(), auth, api: crate::Api::Flux };
        crate::transfer::load(ep, &spec.table.name, &target, spec, columns, source, progress).await
    }

    async fn server_version(&mut self) -> Result<String> {
        let resp = self.http.get(format!("{}/health", self.base)).send().await.map_err(send_err)?;
        let v: serde_json::Value = serde_json::from_str(&http::text(resp).await?).unwrap_or_default();
        let version = v.get("version").and_then(|v| v.as_str()).unwrap_or("");
        Ok(format!("InfluxDB {}", if version.is_empty() { "2 (Cloud)" } else { version }).trim().to_string())
    }

    /// `/metrics` (Prometheus), `/health` and the bucket names (the storage
    /// metrics label buckets by id).
    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        Err(Error::Unsupported(crate::security::TOKENS.into()))
    }

    async fn grants(&mut self, _principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        Err(Error::Unsupported(crate::security::TOKENS.into()))
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        Err(Error::Unsupported(crate::processes::V2_UNSUPPORTED.into()))
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Unsupported(crate::processes::V2_UNSUPPORTED.into()))
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let resp = self
            .http
            .get(format!("{}/metrics", self.base))
            .header("Authorization", format!("Token {}", self.token))
            .send()
            .await
            .map_err(send_err)?;
        let prom = http::text(resp).await.ok().map(|t| monitor::Prom::parse(&t));
        let health = match self.http.get(format!("{}/health", self.base)).send().await {
            Ok(r) => http::text(r).await.ok().and_then(|b| serde_json::from_str(&b).ok()),
            Err(_) => None,
        };
        let mut buckets = BTreeMap::new();
        if let Ok(v) = self.get("/api/v2/buckets", &[("org", self.org.as_str()), ("limit", "100")]).await {
            for b in v.get("buckets").and_then(|b| b.as_array()).into_iter().flatten() {
                if let (Some(id), Some(name)) = (b.get("id").and_then(|i| i.as_str()), b.get("name").and_then(|n| n.as_str())) {
                    buckets.insert(id.to_string(), name.to_string());
                }
            }
        }
        Ok(monitor::v2_snapshot(prom.as_ref(), health.as_ref(), &buckets))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let off = offset.to_string();
            let v = self.get("/api/v2/buckets", &[("org", self.org.as_str()), ("limit", "100"), ("offset", &off)]).await?;
            let page: Vec<String> = v
                .get("buckets")
                .and_then(|b| b.as_array())
                .map(|b| b.iter().filter_map(|b| b.get("name")?.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let n = page.len();
            out.extend(page.into_iter().filter(|b| !b.starts_with('_')));
            if n < 100 {
                break;
            }
            offset += 100;
        }
        out.sort();
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Some(bucket) = self.bucket.clone() else { return Ok(Vec::new()) };
        let script = format!(
            "import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: {}, {SCHEMA_RANGE})",
            flux_str(&bucket)
        );
        Ok(self
            .values(&script)
            .await?
            .into_iter()
            .map(|name| DbObject { kind: kinds::MEASUREMENT.into(), schema: None, name, parent: None })
            .collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let (b, m) = (flux_str(self.bucket()?), flux_str(&obj.name));
        let tags = self
            .values(&format!(
                "import \"influxdata/influxdb/schema\"\nschema.measurementTagKeys(bucket: {b}, measurement: {m}, {SCHEMA_RANGE})"
            ))
            .await?;
        let fields = self
            .values(&format!(
                "import \"influxdata/influxdb/schema\"\nschema.measurementFieldKeys(bucket: {b}, measurement: {m}, {SCHEMA_RANGE})"
            ))
            .await?;
        let col = |name: String, data_type: &str, key: bool| ColumnInfo {
            name,
            data_type: data_type.into(),
            nullable: !key,
            primary_key: key,
            auto_increment: false,
            default_value: None,
        };
        let mut out = vec![col("_time".into(), "time", true)];
        // Flux's own columns, not tags; other keys may start with `_`.
        let own = ["_start", "_stop", "_field", "_measurement", "_time", "_value"];
        out.extend(tags.into_iter().filter(|t| !own.contains(&t.as_str())).map(|t| col(t, "tag", true)));
        out.extend(fields.into_iter().map(|f| col(f, "field", false)));
        Ok(out)
    }

    async fn definition(&mut self, _obj: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
    }

    /// Two scripts: the measurements, then every one's tag and field keys.
    /// Flux reports no field types without reading the data (`field`, as
    /// in [`Session::columns`]); `_time` and the tags key the series.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let measurements: Vec<String> = self.list_objects().await?.into_iter().map(|o| o.name).collect();
        if measurements.is_empty() {
            return Ok(Vec::new());
        }
        let mut tables: BTreeMap<String, TableSchema> = measurements
            .iter()
            .map(|m| {
                let time = ColumnDef { name: "_time".into(), data_type: "time".into(), nullable: false, ..Default::default() };
                let t = TableSchema {
                    kind: kinds::MEASUREMENT.into(),
                    name: m.clone(),
                    columns: vec![time],
                    primary_key: Some(KeyDef { name: None, columns: vec!["_time".into()] }),
                    ..Default::default()
                };
                (m.clone(), t)
            })
            .collect();
        let mut keys: Vec<(String, String, String)> = Vec::new();
        for t in self.flux(&keys_script(self.bucket()?, &measurements)).await? {
            let col = |n: &str| t.columns.iter().position(|c| c == n);
            let (Some(m), Some(k), Some(v)) = (col("m"), col("k"), col("_value")) else { continue };
            let s = |r: &Vec<serde_json::Value>, i: usize| r[i].as_str().unwrap_or_default().to_string();
            keys.extend(t.rows.iter().map(|r| (s(r, m), s(r, k), s(r, v))));
        }
        // Tags first, then fields, each sorted (like `columns`).
        keys.sort_by(|a, b| (&a.0, a.1 != "tag", &a.2).cmp(&(&b.0, b.1 != "tag", &b.2)));
        keys.dedup();
        for (m, kind, key) in keys {
            let Some(t) = tables.get_mut(&m) else { continue };
            if kind == "tag" {
                if key.starts_with('_') {
                    continue;
                }
                if let Some(pk) = &mut t.primary_key {
                    pk.columns.push(key.clone());
                }
                let options = [("tag".to_string(), "true".to_string())].into();
                t.columns.push(ColumnDef { name: key, data_type: "tag".into(), nullable: false, options, ..Default::default() });
            } else {
                t.columns.push(ColumnDef { name: key, data_type: "field".into(), ..Default::default() });
            }
        }
        Ok(tables.into_values().collect())
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.create_database_with(name, &std::collections::BTreeMap::new()).await
    }

    /// `POST /api/v2/buckets` with the retention rules (see [`crate::create_db`]).
    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.check_writable("crear")?;
        let orgs = self.get("/api/v2/orgs", &[("org", self.org.as_str())]).await?;
        let org_id = orgs
            .pointer("/orgs/0/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Query(format!("No se encontró la organización «{}».", self.org)))?
            .to_string();
        let body = crate::create_db::bucket(name, &org_id, options)?;
        self.send(self.http.post(format!("{}{}", self.base, crate::create_db::V2_PATH)).json(&body)).await?;
        Ok(())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.check_writable("borrar")?;
        if self.bucket.as_deref() == Some(name) {
            return Err(Error::Query(format!("No se puede borrar «{name}»: es el bucket de esta conexión.")));
        }
        let v = self.get("/api/v2/buckets", &[("org", self.org.as_str()), ("name", name)]).await?;
        let id = v
            .pointer("/buckets/0/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Query(format!("No existe el bucket «{name}».")))?
            .to_string();
        self.send(self.http.delete(format!("{}/api/v2/buckets/{id}", self.base))).await?;
        Ok(())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let bucket = self.bucket.as_deref().unwrap_or("");
        format!(
            "from(bucket: {})\n  |> range(start: 0)\n  |> filter(fn: (r) => r._measurement == {})\n  |> limit(n: {limit})\n  \
             |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n  |> group()\n  \
             |> drop(columns: [\"_start\", \"_stop\"])\n  |> limit(n: {limit})",
            flux_str(bucket),
            flux_str(&obj.name)
        )
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.read_only {
            if let Some(w) = first_write(text) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó la función {w}(). Solo se permiten consultas que leen."
                )));
            }
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        let tables = self.flux(text).await?;
        if tables.is_empty() {
            out.push_affected(0);
        }
        for t in tables {
            out.begin_result(
                t.columns.into_iter().zip(t.types).map(|(name, type_name)| ResultColumn { name, type_name }).collect(),
            );
            for row in t.rows {
                out.push_row(row, max_rows);
            }
        }
        Ok(())
    }

    /// Flux has no estimated plan; a run with the `profiler` package on
    /// returns the plan it used and each operator's time.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if !analyze {
            return Err(Error::Unsupported(
                "Flux no tiene plan estimado: usá «Ejecutar con plan» para ver el perfil real de la consulta (paquete profiler)."
                    .into(),
            ));
        }
        if self.read_only {
            if let Some(w) = first_write(text) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó la función {w}(). Solo se permiten consultas que leen."
                )));
            }
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        let tables = self.flux(&with_profiler(text)).await?;
        let mut query: Option<plan::Row> = None;
        let mut operators: Vec<plan::Row> = Vec::new();
        let mut results = Vec::new();
        for t in tables {
            let m = t.columns.iter().position(|c| c == "_measurement");
            let kind = m.and_then(|i| t.rows.first()?.get(i)?.as_str().map(str::to_string)).unwrap_or_default();
            let rows = || t.rows.iter().map(|r| t.columns.iter().cloned().zip(r.iter().cloned()).collect::<plan::Row>());
            match kind.as_str() {
                "profiler/query" => query = rows().next(),
                "profiler/operator" => operators.extend(rows()),
                _ => results.push(t),
            }
        }
        if results.is_empty() {
            out.push_affected(0);
        }
        for t in results {
            out.begin_result(
                t.columns.into_iter().zip(t.types).map(|(name, type_name)| ResultColumn { name, type_name }).collect(),
            );
            for row in t.rows {
                out.push_row(row, max_rows);
            }
        }
        let raw = query
            .as_ref()
            .and_then(|q| q.iter().find(|(c, _)| c == "flux/query-plan"))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_string();
        out.plans.push(Plan {
            statement: text.trim().to_string(),
            root: plan::flux_profile_tree(query.as_ref(), &operators),
            actual: true,
            raw_format: "dot".into(),
            raw,
        });
        Ok(())
    }

    /// `write:buckets` from the visible authorizations (see `permissions`).
    /// "Propiedades" (see [`crate::properties`]).
    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        let list = self.get("/api/v2/authorizations", &[]).await;
        crate::permissions::v2(list, &self.org, database)
    }
}

/// The script with the query and operator profilers on: `import "profiler"`
/// with the other imports (which must come first) and the option after them.
pub fn with_profiler(script: &str) -> String {
    let lines: Vec<&str> = script.lines().collect();
    // Past the leading imports, blank lines and comments.
    let mut end = 0;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with("import ") {
            end = i + 1;
        } else if !(t.is_empty() || t.starts_with("//")) {
            break;
        }
    }
    let mut out: Vec<String> = lines[..end].iter().map(|s| s.to_string()).collect();
    if !lines[..end].iter().any(|l| l.trim().trim_start_matches("import").trim() == "\"profiler\"") {
        out.push("import \"profiler\"".into());
    }
    out.push("option profiler.enabledProfilers = [\"query\", \"operator\"]".into());
    out.extend(lines[end..].iter().map(|s| s.to_string()));
    out.join("\n")
}

/// The browse query restricted by the grid's column filters: one Flux
/// `filter()` right after the pivot (where fields are columns), and without
/// the per-series `limit()` before it, which would cut the points before
/// they're filtered. Flux is strictly typed, so numbers compare as floats
/// (`float(v: r["c"])`) and `_time` against `time(v: "…")`. There are no
/// SQL conditions in Flux.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::FilterOp;
    use serde_json::Value as J;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let float = |n: &serde_json::Number| {
        let s = n.as_f64().unwrap_or_default().to_string();
        if s.contains('.') { s } else { format!("{s}.0") }
    };
    let mut parts = Vec::new();
    for f in filters {
        let c = format!("r[{}]", flux_str(&f.column));
        let text = |v: &J| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let cmp = |op: &str| -> Result<String> {
            Ok(match first()? {
                J::Number(n) => format!("(exists {c} and float(v: {c}) {op} {})", float(n)),
                J::Bool(b) => format!("{c} {op} {b}"),
                J::String(s) if f.column == "_time" => format!("{c} {op} time(v: {})", flux_str(s)),
                v => format!("{c} {op} {}", flux_str(&text(v))),
            })
        };
        let re = || first().map(|v| crate::v1::regex_literal(&text(v)));
        let set = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(match f.values.iter().map(J::as_number).collect::<Option<Vec<_>>>() {
                Some(ns) => format!("contains(value: float(v: {c}), set: [{}])", ns.into_iter().map(float).collect::<Vec<_>>().join(", ")),
                None => format!("contains(value: {c}, set: [{}])", f.values.iter().map(|v| flux_str(&text(v))).collect::<Vec<_>>().join(", ")),
            })
        };
        parts.push(match f.op {
            FilterOp::Eq => cmp("==")?,
            FilterOp::Ne => cmp("!=")?,
            FilterOp::Gt => cmp(">")?,
            FilterOp::Ge => cmp(">=")?,
            FilterOp::Lt => cmp("<")?,
            FilterOp::Le => cmp("<=")?,
            FilterOp::Contains => format!("{c} =~ /{}/", re()?),
            FilterOp::NotContains => format!("{c} !~ /{}/", re()?),
            FilterOp::StartsWith => format!("{c} =~ /^{}/", re()?),
            FilterOp::EndsWith => format!("{c} =~ /{}$/", re()?),
            FilterOp::IsNull => format!("not exists {c}"),
            FilterOp::NotNull => format!("exists {c}"),
            FilterOp::IsEmpty => format!("{c} == \"\""),
            FilterOp::NotEmpty => format!("(exists {c} and {c} != \"\")"),
            FilterOp::In => format!("(exists {c} and {})", set()?),
            FilterOp::NotIn => format!("(not exists {c} or not {})", set()?),
            FilterOp::IsTrue => format!("{c} == true"),
            FilterOp::IsFalse => format!("{c} == false"),
            FilterOp::TrueOrNull => format!("(not exists {c} or {c} == true)"),
            FilterOp::FalseOrNull => format!("(not exists {c} or {c} == false)"),
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported("Flux no toma condiciones SQL: se filtran en la grilla".into()))
            }
        });
    }
    let filter = format!("  |> filter(fn: (r) => {})", parts.join(" and "));
    let mut lines: Vec<String> = browse.lines().map(str::to_string).collect();
    let pivot = lines.iter().position(|l| l.trim_start().starts_with("|> pivot("));
    let limit = lines.iter().position(|l| l.trim_start().starts_with("|> limit("));
    match (pivot, limit) {
        (Some(p), Some(l)) if l < p => {
            lines.insert(p + 1, filter);
            lines.remove(l);
        }
        (Some(p), _) => lines.insert(p + 1, filter),
        _ => return Err(Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into())),
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_browse_after_the_pivot() {
        use dbine_driver::{ColumnFilter, FilterOp};
        use serde_json::json;
        let f = |column: &str, op: FilterOp, values: Vec<serde_json::Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let browse = "from(bucket: \"b\")\n  |> range(start: 0)\n  |> filter(fn: (r) => r._measurement == \"cpu\")\n  |> limit(n: 200)\n  |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n  |> group()\n  |> drop(columns: [\"_start\", \"_stop\"])\n  |> limit(n: 200)";
        assert_eq!(
            filtered_browse(
                browse,
                &[
                    f("host", FilterOp::Eq, vec![json!("O\"Brien ${x}")]),
                    f("usage", FilterOp::Gt, vec![json!(5)]),
                    f("path", FilterOp::Contains, vec![json!("a.b")]),
                    f("region", FilterOp::IsNull, vec![]),
                    f("code", FilterOp::In, vec![json!(1), json!(2.5)]),
                    f("_time", FilterOp::Ge, vec![json!("2024-01-01T00:00:00Z")]),
                ]
            )
            .unwrap(),
            "from(bucket: \"b\")\n  |> range(start: 0)\n  |> filter(fn: (r) => r._measurement == \"cpu\")\n  |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n  |> filter(fn: (r) => r[\"host\"] == \"O\\\"Brien \\${x}\" and (exists r[\"usage\"] and float(v: r[\"usage\"]) > 5.0) and r[\"path\"] =~ /a\\.b/ and not exists r[\"region\"] and (exists r[\"code\"] and contains(value: float(v: r[\"code\"]), set: [1.0, 2.5])) and r[\"_time\"] >= time(v: \"2024-01-01T00:00:00Z\"))\n  |> group()\n  |> drop(columns: [\"_start\", \"_stop\"])\n  |> limit(n: 200)"
        );
        assert!(matches!(filtered_browse(browse, &[ColumnFilter { column: "x".into(), op: FilterOp::Sql, values: vec![], sql: Some("1".into()) }]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn profiler_goes_after_the_imports() {
        let s = with_profiler("import \"strings\"\n\nfrom(bucket: \"b\")");
        assert_eq!(
            s,
            "import \"strings\"\nimport \"profiler\"\noption profiler.enabledProfilers = [\"query\", \"operator\"]\n\nfrom(bucket: \"b\")"
        );
        let s = with_profiler("import \"profiler\"\nfrom(bucket: \"b\")");
        assert_eq!(s.matches("import \"profiler\"").count(), 1);
        assert!(with_profiler("from(bucket: \"b\")").starts_with("import \"profiler\"\noption"));
    }

    #[test]
    fn one_script_for_every_measurement() {
        let s = keys_script("b", &["cpu".into(), "m\"x".into()]);
        assert!(s.starts_with("import \"influxdata/influxdb/schema\"\nt0 = schema.measurementTagKeys(bucket: \"b\", measurement: \"cpu\""));
        assert!(s.contains("f1 = schema.measurementFieldKeys(bucket: \"b\", measurement: \"m\\\"x\""));
        assert!(s.contains("({m: \"cpu\", k: \"field\", _value: string(v: r._value)})"));
        assert!(s.ends_with("union(tables: [t0, f0, t1, f1])\n  |> group()"));
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(flux_str(r#"a"b\c${x}"#), r#""a\"b\\c\${x}""#);
    }

    #[test]
    fn writes_are_found_outside_strings() {
        assert_eq!(first_write("from(bucket: \"b\") |> range(start: -1h) |> to(bucket: \"c\")").as_deref(), Some("to"));
        assert_eq!(first_write("import \"http\"\nhttp.post(url: \"x\")").as_deref(), Some("post"));
        assert_eq!(first_write("from(bucket: \"to(\") |> range(start: -1h) // to(bucket)"), None);
        assert_eq!(first_write("x = 1\ntotal(x)"), None);
    }
}

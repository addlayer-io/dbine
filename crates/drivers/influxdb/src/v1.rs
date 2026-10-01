//! InfluxDB 1.x with InfluxQL: `/query?q=…&db=…`, answered in JSON. Also
//! serves the 1.x compatibility API of InfluxDB 2 and 3. Reads go by GET,
//! so on a read-only connection the server itself refuses writes.

use crate::http::{self, send_err};
use crate::monitor;
use crate::plan;
use crate::profiler;
use dbine_driver::{
    async_trait, json_f64, json_i64, json_u64, kinds, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DriverInfo, Error, Family, Field, KeyDef, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, Plan, QueryOutcome,
    Result, ResultColumn, Session, TableSchema,
};
use serde_json::Value as J;
use std::collections::BTreeMap;

pub fn info() -> DriverInfo {
    DriverInfo {
        id: "influxdb1",
        name: "InfluxDB 1 (InfluxQL)",
        family: Family::TimeSeries,
        language: Language::Sql,
        dialect: "influxql",
        default_port: 8086,
        fields: Field::server_set(),
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::MEASUREMENT, "Measurements", true, true, false)],
    }
}

/// Retention policies and continuous queries: the objects InfluxQL creates
/// besides databases. `mi_base` is the database to change.
pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: "retention_policy",
            label: "Nueva política de retención",
            template: "CREATE RETENTION POLICY \"{name}\" ON \"mi_base\" DURATION 30d REPLICATION 1 SHARD DURATION 1d".into(),
        },
        CreateTemplate {
            kind: "continuous_query",
            label: "Nueva consulta continua",
            template: "CREATE CONTINUOUS QUERY \"{name}\" ON \"mi_base\"\nBEGIN\n  SELECT mean(\"value\") INTO \"cpu_1h\"\n  \
                       FROM \"cpu\"\n  GROUP BY time(1h), *\nEND"
                .into(),
        },
    ]
}

/// Measurements with their tags and fields from `SHOW MEASUREMENTS`,
/// `SHOW TAG KEYS` and `SHOW FIELD KEYS` answers (one result each). Like
/// [`Session::columns`]: `time` and the tags key the series.
pub fn schema_tables(results: &[J]) -> Vec<TableSchema> {
    let series = |i: usize| -> Vec<J> {
        results.get(i).and_then(|r| r.get("series")).and_then(|s| s.as_array()).cloned().unwrap_or_default()
    };
    let rows = |s: &J| -> Vec<Vec<J>> {
        s.get("values").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_array().cloned()).collect()
    };
    let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
    for s in series(0) {
        for r in rows(&s) {
            let name = str_at(&r, 0);
            let time = ColumnDef { name: "time".into(), data_type: "time".into(), nullable: false, ..Default::default() };
            let t = TableSchema {
                kind: kinds::MEASUREMENT.into(),
                name: name.clone(),
                columns: vec![time],
                primary_key: Some(KeyDef { name: None, columns: vec!["time".into()] }),
                ..Default::default()
            };
            tables.insert(name, t);
        }
    }
    for (i, tag) in [(1, true), (2, false)] {
        for s in series(i) {
            let Some(t) = s.get("name").and_then(|n| n.as_str()).and_then(|n| tables.get_mut(n)) else { continue };
            for r in rows(&s) {
                let name = str_at(&r, 0);
                if tag {
                    if let Some(pk) = &mut t.primary_key {
                        pk.columns.push(name.clone());
                    }
                    let options = [("tag".to_string(), "true".to_string())].into();
                    t.columns.push(ColumnDef { name, data_type: "tag".into(), nullable: false, options, ..Default::default() });
                } else {
                    t.columns.push(ColumnDef { name, data_type: str_at(&r, 1), ..Default::default() });
                }
            }
        }
    }
    tables.into_values().collect()
}

pub struct InfluxQlSession {
    http: reqwest::Client,
    base: String,
    username: String,
    password: String,
    db: Option<String>,
    /// Retention policy set with `USE db.rp`.
    rp: Option<String>,
    read_only: bool,
    /// The running profiler, if any.
    profiler: Option<profiler::V1State>,
}

pub async fn connect(cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
    let db = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));
    let mut s = InfluxQlSession {
        http: http::client(cfg)?,
        base: http::base_url(cfg, 8086),
        username: cfg.username_or_empty().to_string(),
        password: cfg.password_or_empty().to_string(),
        db: db.map(str::to_string),
        rp: None,
        read_only: cfg.read_only,
        profiler: None,
    };
    // Checks the credentials (with auth disabled anything passes).
    s.query("SHOW DATABASES").await.map_err(|e| match e {
        Error::Query(m) if m.contains("authorization") || m.contains("authentication") => Error::AuthFailed(m),
        e => e,
    })?;
    Ok(Box::new(s))
}

/// An InfluxQL identifier, always double-quoted.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", http::escape(name, '"'))
}

/// `DELETE FROM "m" WHERE time = … AND "tag" = '…'` per point. InfluxQL
/// deletes by time and tags only (its key: `time` plus every tag), and a
/// missing tag compares as `''`. Without a time in the key the statement
/// would drop the series' whole history, so it's refused.
pub fn delete_script(measurement: &str, keys: &[Vec<(String, J)>]) -> Result<String> {
    let mut out = Vec::new();
    for key in keys {
        let Some(time) = key.iter().find(|(k, _)| k.eq_ignore_ascii_case("time")).map(|(_, v)| v).filter(|v| !v.is_null()) else {
            return Err(Error::Unsupported(
                "InfluxQL borra un punto por su marca de tiempo y sus tags: la clave de la fila tiene que incluir la columna time".into(),
            ));
        };
        let mut conds = vec![match time {
            J::Number(n) => format!("time = {n}"),
            J::String(s) => format!("time = '{}'", http::escape(s, '\'')),
            other => format!("time = '{}'", http::escape(&other.to_string(), '\'')),
        }];
        conds.extend(key.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("time")).map(|(k, v)| match v {
            J::Null => format!("{} = ''", ident(k)),
            J::String(s) => format!("{} = '{}'", ident(k), http::escape(s, '\'')),
            other => format!("{} = '{}'", ident(k), http::escape(&other.to_string(), '\'')),
        }));
        out.push(format!("DELETE FROM {} WHERE {};", ident(measurement), conds.join(" AND ")));
    }
    Ok(out.join("\n"))
}

/// `USE db` / `USE db.rp` / `USE "d b"."r p"` (the influx CLI's).
fn use_target(stmt: &str) -> Option<(String, Option<String>)> {
    let s = stmt.trim();
    let rest = s.get(..4).filter(|h| h.eq_ignore_ascii_case("use ")).map(|_| s[4..].trim())?;
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(n) = chars.next() {
                    match n {
                        '\\' => cur.extend(chars.next()),
                        '"' => break,
                        n => cur.push(n),
                    }
                }
            }
            '.' => parts.push(std::mem::take(&mut cur)),
            c if c.is_whitespace() => return None,
            c => cur.push(c),
        }
    }
    parts.push(cur);
    match parts.as_slice() {
        [db] if !db.is_empty() => Some((db.clone(), None)),
        [db, rp] if !db.is_empty() && !rp.is_empty() => Some((db.clone(), Some(rp.clone()))),
        _ => None,
    }
}

/// A refused request: `error parsing query: … at line L, char C` placed in
/// `text` (the request started at `base`).
fn parse_error(msg: &str, text: &str, base: usize) -> Error {
    let num = |key: &str| -> Option<usize> {
        let at = msg.rfind(key)?;
        msg[at + key.len()..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    };
    let (Some(line), Some(ch)) = (num("at line "), num(", char ")) else { return Error::Query(msg.to_string()) };
    let q = &text[base..];
    let mut start = 0;
    for _ in 1..line.max(1) {
        match q[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return Error::Query(msg.to_string()),
        }
    }
    let end = q[start..].find('\n').map_or(q.len(), |e| start + e);
    let off = base + start + q[start..end].char_indices().nth(ch.saturating_sub(1)).map_or(end - start, |(i, _)| i);
    let line = text[..off].matches('\n').count() as u32 + 1;
    dbine_driver::ScriptError::new(msg).at_offset(off).at_line(line).into()
}

/// Statement kinds that only read; `SELECT … INTO` writes and is refused too.
pub fn first_write(script: &str) -> Option<String> {
    for stmt in dbine_driver::sql::split_statements(script) {
        let words: Vec<String> = stmt
            .split(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let first = words.first().cloned().unwrap_or_default();
        if !matches!(first.as_str(), "select" | "show" | "explain" | "use") {
            return Some(first.to_uppercase());
        }
        if first == "select" && words.iter().any(|w| w == "into") {
            return Some("SELECT … INTO".into());
        }
    }
    None
}

impl InfluxQlSession {
    /// Consecutive statements of `text` in one request; a failure placed
    /// in `text` (the parser's `at line L, char C`, or the statement the
    /// server stopped at).
    async fn run_chunk(&self, text: &str, units: &[&dbine_driver::ScriptStatement], max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let (Some(first), Some(last)) = (units.first(), units.last()) else { return Ok(()) };
        let q = &text[first.start..last.end];
        let v = match self.raw(q, first_write(q).is_some()).await {
            Ok(v) => v,
            Err(Error::Query(m)) => return Err(parse_error(&m, text, first.start)),
            Err(e) => return Err(e),
        };
        for (i, r) in v.get("results").and_then(|r| r.as_array()).into_iter().flatten().enumerate() {
            for m in r.get("messages").and_then(|m| m.as_array()).into_iter().flatten() {
                if let Some(t) = m.get("text").and_then(|t| t.as_str()) {
                    match m.get("level").and_then(|l| l.as_str()) {
                        Some("warning") => out.warning(t),
                        _ => out.info(t),
                    }
                }
            }
            if let Some(e) = r.get("error").and_then(|e| e.as_str()) {
                let n = r.get("statement_id").and_then(|n| n.as_u64()).map_or(i, |n| n as usize);
                let u = units.get(n).unwrap_or(first);
                let line = text[..u.start].matches('\n').count() as u32 + 1;
                return Err(dbine_driver::ScriptError::new(e).at_offset(u.start).at_line(line).into());
            }
            let tables = statement_tables(r);
            if tables.is_empty() {
                out.push_affected(0);
            }
            for (header, rows) in tables {
                out.begin_result(header.into_iter().map(|name| ResultColumn { name, type_name: String::new() }).collect());
                for row in rows {
                    out.push_row(row, max_rows);
                }
            }
        }
        Ok(())
    }

    /// One request with the whole script; InfluxDB runs its statements in
    /// order and answers one result per statement.
    async fn raw(&self, q: &str, write: bool) -> Result<J> {
        let mut params = vec![("q", q)];
        if let Some(db) = &self.db {
            params.push(("db", db.as_str()));
        }
        if let Some(rp) = &self.rp {
            params.push(("rp", rp.as_str()));
        }
        let req = if write {
            self.http.post(format!("{}/query", self.base)).form(&params)
        } else {
            self.http.get(format!("{}/query", self.base)).query(&params)
        };
        let req = if self.username.is_empty() { req } else { req.basic_auth(&self.username, Some(&self.password)) };
        let resp = req.send().await.map_err(send_err)?;
        let v: J = serde_json::from_str(&http::text(resp).await?)?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(Error::Query(e.to_string()));
        }
        Ok(v)
    }

    /// A read with its results, stopping at the first failing statement.
    pub(crate) async fn query(&mut self, q: &str) -> Result<Vec<J>> {
        let v = self.raw(q, false).await?;
        let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        if let Some(e) = results.iter().find_map(|r| r.get("error").and_then(|e| e.as_str())) {
            return Err(Error::Query(e.to_string()));
        }
        Ok(results)
    }

    /// A statement that writes (by POST), failing on its error.
    async fn write_statement(&self, q: &str) -> Result<()> {
        let v = self.raw(q, true).await?;
        let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        match results.iter().find_map(|r| r.get("error").and_then(|e| e.as_str())) {
            Some(e) => Err(Error::Query(e.to_string())),
            None => Ok(()),
        }
    }

    /// The first column of every row, as text lines (EXPLAIN's answer).
    async fn lines(&mut self, q: &str) -> Result<Vec<String>> {
        Ok(self.first_column(q).await?.iter().map(|r| r.first().and_then(|v| v.as_str()).unwrap_or_default().to_string()).collect())
    }

    /// The first column of every series' rows.
    async fn first_column(&mut self, q: &str) -> Result<Vec<Vec<J>>> {
        let mut out = Vec::new();
        for r in self.query(q).await? {
            for s in r.get("series").and_then(|s| s.as_array()).into_iter().flatten() {
                out.extend(s.get("values").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_array().cloned()));
            }
        }
        Ok(out)
    }
}

fn str_at(row: &[J], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

pub fn cell(v: &J) -> J {
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
        J::String(s) => http::iso_time(s).map_or_else(|| v.clone(), J::String),
        J::Array(_) | J::Object(_) => J::String(v.to_string()),
        other => other.clone(),
    }
}

/// One statement's result as tables: series with the same columns share a
/// table, with the measurement name and tags in front when there are
/// several series or `GROUP BY` tags.
pub fn statement_tables(result: &J) -> Vec<(Vec<String>, Vec<Vec<J>>)> {
    let series = result.get("series").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    let tag_keys: Vec<String> = {
        let mut keys: Vec<String> = Vec::new();
        for s in &series {
            for k in s.get("tags").and_then(|t| t.as_object()).into_iter().flat_map(|t| t.keys()) {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
        keys
    };
    let with_name = series.len() > 1;
    let mut tables: Vec<(Vec<String>, Vec<Vec<J>>)> = Vec::new();
    for s in &series {
        let cols: Vec<String> =
            s.get("columns").and_then(|c| c.as_array()).into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect();
        let mut header: Vec<String> = Vec::new();
        if with_name {
            header.push("name".into());
        }
        header.extend(tag_keys.iter().cloned());
        header.extend(cols.iter().cloned());
        let name = s.get("name").cloned().unwrap_or(J::Null);
        let tags = s.get("tags").and_then(|t| t.as_object());
        let prefix: Vec<J> = with_name
            .then(|| name.clone())
            .into_iter()
            .chain(tag_keys.iter().map(|k| tags.and_then(|t| t.get(k)).cloned().unwrap_or(J::Null)))
            .collect();
        let rows: Vec<Vec<J>> = s
            .get("values")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .map(|r| prefix.iter().cloned().chain(r.as_array().into_iter().flatten().map(cell)).collect())
            .collect();
        match tables.iter_mut().find(|(h, _)| *h == header) {
            Some((_, existing)) => existing.extend(rows),
            None => tables.push((header, rows)),
        }
    }
    tables
}

#[async_trait]
impl Session for InfluxQlSession {
    /// `SELECT *` chunked, in nanoseconds (see `transfer.rs`).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let cols = crate::transfer::read_columns(&self.columns(&spec.table).await?, &spec.columns)?;
        let mut q = format!("SELECT * FROM {}", ident(&spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            q.push_str(&format!(" WHERE {f}"));
        }
        let request = |params: &[(&str, &str)]| {
            let mut params = params.to_vec();
            if let Some(db) = &self.db {
                params.push(("db", db.as_str()));
            }
            let req = self.http.get(format!("{}/query", self.base)).query(&params);
            if self.username.is_empty() { req } else { req.basic_auth(&self.username, Some(&self.password)) }
        };
        // Chunks of any size in bytes: the read cuts their points out as they arrive.
        let chunk = crate::transfer::CHUNK_SIZE.to_string();
        let req = request(&[("q", q.as_str()), ("chunked", "true"), ("chunk_size", chunk.as_str()), ("epoch", "ns")]);
        crate::transfer::read_influxql(req, &cols, &sink).await
    }

    /// Line protocol to `/write` (see `transfer.rs`).
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
        let url = reqwest::Url::parse_with_params(&format!("{}/write", self.base), &[("db", db.as_str()), ("precision", "ns")])
            .map_err(|e| Error::Query(e.to_string()))?;
        let auth = if self.username.is_empty() {
            crate::transfer::Auth::None
        } else {
            crate::transfer::Auth::Basic(self.username.clone(), self.password.clone())
        };
        let ep = crate::transfer::Endpoint { http: self.http.clone(), url: url.into(), auth, api: crate::Api::InfluxQl };
        crate::transfer::load(ep, &spec.table.name, &target, spec, columns, source, progress).await
    }

    async fn server_version(&mut self) -> Result<String> {
        let resp = self.http.get(format!("{}/ping", self.base)).send().await.map_err(send_err)?;
        let v = resp.headers().get("X-Influxdb-Version").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        Ok(format!("InfluxDB {v}").trim().to_string())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.first_column("SHOW DATABASES").await?;
        let mut v: Vec<String> = rows.iter().map(|r| str_at(r, 0)).filter(|d| !d.is_empty() && d != "_internal").collect();
        v.sort();
        Ok(v)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        if self.db.is_none() {
            return Ok(Vec::new());
        }
        let rows = self.first_column("SHOW MEASUREMENTS").await?;
        Ok(rows
            .iter()
            .map(|r| DbObject { kind: kinds::MEASUREMENT.into(), schema: None, name: str_at(r, 0), parent: None })
            .collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let m = ident(&obj.name);
        let tags = self.first_column(&format!("SHOW TAG KEYS FROM {m}")).await?;
        let fields = self.first_column(&format!("SHOW FIELD KEYS FROM {m}")).await?;
        let col = |name: String, data_type: String, key: bool| ColumnInfo {
            name,
            data_type,
            nullable: !key,
            primary_key: key,
            auto_increment: false,
            default_value: None,
        };
        let mut out = vec![col("time".into(), "time".into(), true)];
        out.extend(tags.iter().map(|r| col(str_at(r, 0), "tag".into(), true)));
        out.extend(fields.iter().map(|r| col(str_at(r, 0), str_at(r, 1), false)));
        Ok(out)
    }

    async fn definition(&mut self, _obj: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        if self.db.is_none() {
            return Ok(Vec::new());
        }
        Ok(schema_tables(&self.query("SHOW MEASUREMENTS; SHOW TAG KEYS; SHOW FIELD KEYS").await?))
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        self.write_statement(&format!("CREATE DATABASE {}", ident(name))).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()));
        }
        if self.db.as_deref() == Some(name) {
            return Err(Error::Query(format!("No se puede borrar «{name}»: es la base de datos de esta conexión.")));
        }
        self.write_statement(&format!("DROP DATABASE {}", ident(name))).await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT * FROM {} ORDER BY time DESC LIMIT {limit}", ident(&obj.name))
    }

    /// The script goes to `/query` as the influx CLI's statements would:
    /// the server runs them in order and stops at the first failure. `USE
    /// db[.rp]` is the CLI's: it sets the database (and retention policy)
    /// of the statements after it, and of later runs.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let write = first_write(text);
        if self.read_only {
            if let Some(kw) = &write {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, SHOW, EXPLAIN)."
                )));
            }
        }
        let units = dbine_driver::sql::split_script(text, &dbine_driver::ScriptDialect::generic());
        let mut chunk: Vec<&dbine_driver::ScriptStatement> = Vec::new();
        for u in &units {
            if let Some((db, rp)) = use_target(&u.text) {
                self.run_chunk(text, &chunk, max_rows, out).await?;
                chunk.clear();
                out.info(match &rp {
                    Some(rp) => format!("Base de datos: {db} (política de retención {rp})"),
                    None => format!("Base de datos: {db}"),
                });
                self.db = Some(db);
                self.rp = rp;
                out.push_affected(0);
                if let Some(r) = out.results.last_mut() {
                    r.tag = Some("USE".into());
                }
            } else {
                chunk.push(u);
            }
        }
        self.run_chunk(text, &chunk, max_rows, out).await
    }

    /// `SHOW STATS`, `SHOW DIAGNOSTICS` and `SHOW QUERIES`, one request each
    /// so a refused one (they need an admin when auth is on) leaves the rest.
    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        crate::security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        crate::security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut answers = Vec::new();
        for q in ["SHOW STATS", "SHOW DIAGNOSTICS", "SHOW QUERIES"] {
            answers.push(match self.query(q).await {
                Ok(r) => r.into_iter().next(),
                Err(Error::Connect(m)) => return Err(Error::Connect(m)),
                Err(_) => None,
            });
        }
        Ok(monitor::v1_snapshot(answers[0].as_ref(), answers[1].as_ref(), answers[2].as_ref()))
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::v1_start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::v1_poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if analyze && self.read_only {
            if let Some(kw) = first_write(text) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, SHOW, EXPLAIN)."
                )));
            }
        }
        for stmt in dbine_driver::sql::split_statements(text) {
            let words: Vec<String> = stmt
                .split(|c: char| c.is_whitespace() || c == '(' || c == ',')
                .filter(|w| !w.is_empty())
                .map(str::to_ascii_lowercase)
                .collect();
            let select = words.first().is_some_and(|w| w == "select");
            // SELECT … INTO writes: EXPLAIN ANALYZE would run it again.
            let writes = select && words.iter().any(|w| w == "into");
            if analyze {
                self.execute(&stmt, max_rows, out).await?;
            }
            if !select {
                if !analyze {
                    out.messages.push(format!("Sin plan para «{}»: InfluxQL solo explica SELECT.", stmt.trim()));
                }
                continue;
            }
            let actual = analyze && !writes;
            let lines = self.lines(&format!("EXPLAIN {}{stmt}", if actual { "ANALYZE " } else { "" })).await?;
            let root = if actual {
                plan::influxql_analyze_tree(&lines).unwrap_or_default()
            } else {
                plan::influxql_explain_tree(&lines)
            };
            let raw = lines.join("\n").trim_end().to_string();
            out.plans.push(Plan { statement: stmt.clone(), root, actual, raw_format: "text".into(), raw });
        }
        Ok(())
    }

    /// Admin or not, from `SHOW USERS` (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        crate::permissions::v1(self, database).await
    }
}

/// A text as a regular expression that matches it literally, between the
/// `/…/` of InfluxQL and Flux.
pub(crate) fn regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The browse query (`SELECT * FROM "m" ORDER BY time DESC LIMIT n`)
/// restricted by the grid's column filters, in InfluxQL: regular
/// expressions for text matches, ORs for lists. InfluxQL can't test for
/// null (a point without the field simply doesn't match).
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, FilterOp};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let lit = |v: &J| match v {
        J::String(s) => format!("'{}'", http::escape(s, '\'')),
        J::Null => "''".into(),
        J::Number(_) | J::Bool(_) => v.to_string(),
        other => format!("'{}'", http::escape(&other.to_string(), '\'')),
    };
    let mut parts = Vec::new();
    for f in filters {
        let c = if f.column.eq_ignore_ascii_case("time") { "time".to_string() } else { ident(&f.column) };
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let re = || first().map(|v| regex_literal(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        let list = |op: &str, join: &str| {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(format!("({})", f.values.iter().map(|v| format!("{c} {op} {}", lit(v))).collect::<Vec<_>>().join(join)))
        };
        let sql = || f.sql.as_deref().unwrap_or("").trim().to_string();
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", lit(first()?)),
            FilterOp::Ne => format!("{c} != {}", lit(first()?)),
            FilterOp::Gt => format!("{c} > {}", lit(first()?)),
            FilterOp::Ge => format!("{c} >= {}", lit(first()?)),
            FilterOp::Lt => format!("{c} < {}", lit(first()?)),
            FilterOp::Le => format!("{c} <= {}", lit(first()?)),
            FilterOp::Contains => format!("{c} =~ /{}/", re()?),
            FilterOp::NotContains => format!("{c} !~ /{}/", re()?),
            FilterOp::StartsWith => format!("{c} =~ /^{}/", re()?),
            FilterOp::EndsWith => format!("{c} =~ /{}$/", re()?),
            FilterOp::IsEmpty => format!("{c} = ''"),
            FilterOp::NotEmpty => format!("{c} != ''"),
            FilterOp::In => list("=", " OR ")?,
            FilterOp::NotIn => list("!=", " AND ")?,
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::Sql => format!("({})", sql()),
            FilterOp::SqlRight => format!("{c} {}", sql()),
            FilterOp::IsNull | FilterOp::NotNull | FilterOp::TrueOrNull | FilterOp::FalseOrNull => {
                return Err(Error::Unsupported("InfluxQL no compara con valores nulos".into()))
            }
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
    fn filtered_browse_in_influxql() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<J>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT * FROM \"cpu\" ORDER BY time DESC LIMIT 200",
                &[
                    f("host", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("usage", FilterOp::Gt, vec![json!(12.5)]),
                    f("path", FilterOp::StartsWith, vec![json!("/var.log")]),
                    f("region", FilterOp::In, vec![json!("a"), json!("b")]),
                    f("time", FilterOp::Ge, vec![json!("2024-01-01T00:00:00Z")]),
                ]
            )
            .unwrap(),
            "SELECT * FROM \"cpu\"\nWHERE \"host\" = 'O\\'Brien'\n  AND \"usage\" > 12.5\n  AND \"path\" =~ /^\\/var\\.log/\n  AND (\"region\" = 'a' OR \"region\" = 'b')\n  AND time >= '2024-01-01T00:00:00Z'\nORDER BY time DESC LIMIT 200"
        );
        assert!(matches!(filtered_browse("SELECT * FROM \"cpu\" LIMIT 5", &[f("x", FilterOp::IsNull, vec![])]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn single_series_keeps_its_columns() {
        let r = json!({"statement_id":0,"series":[{"name":"cpu","columns":["time","value"],"values":[["2024-01-31T13:45:00Z",1.5]]}]});
        let t = statement_tables(&r);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, ["time", "value"]);
        assert_eq!(t[0].1[0], vec![json!("2024-01-31 13:45:00"), json!(1.5)]);
    }

    #[test]
    fn grouped_series_get_name_and_tags() {
        let r = json!({"series":[
            {"name":"cpu","tags":{"host":"a"},"columns":["time","mean"],"values":[[0,1]]},
            {"name":"cpu","tags":{"host":"b"},"columns":["time","mean"],"values":[[0,2]]}
        ]});
        let t = statement_tables(&r);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, ["name", "host", "time", "mean"]);
        assert_eq!(t[0].1[1], vec![json!("cpu"), json!("b"), json!(0), json!(2)]);
    }

    #[test]
    fn schema_from_show_answers() {
        let results = vec![
            json!({"series":[{"name":"measurements","columns":["name"],"values":[["mem"],["cpu"]]}]}),
            json!({"series":[{"name":"cpu","columns":["tagKey"],"values":[["host"],["region"]]}]}),
            json!({"series":[{"name":"cpu","columns":["fieldKey","fieldType"],"values":[["value","float"]]},
                             {"name":"mem","columns":["fieldKey","fieldType"],"values":[["used","integer"]]}]}),
        ];
        let t = schema_tables(&results);
        assert_eq!(t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cpu", "mem"]);
        let cols: Vec<(&str, &str)> = t[0].columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
        assert_eq!(cols, [("time", "time"), ("host", "tag"), ("region", "tag"), ("value", "float")]);
        assert_eq!(t[0].primary_key.as_ref().unwrap().columns, ["time", "host", "region"]);
        assert_eq!(t[0].columns[1].options["tag"], "true");
        assert_eq!(t[1].primary_key.as_ref().unwrap().columns, ["time"]);
    }

    #[test]
    fn read_only_check() {
        assert_eq!(first_write("SELECT * FROM cpu; SHOW MEASUREMENTS"), None);
        assert_eq!(first_write("select * into b from a").as_deref(), Some("SELECT … INTO"));
        assert_eq!(first_write("SHOW DATABASES; DROP DATABASE x").as_deref(), Some("DROP"));
        assert_eq!(first_write("USE telegraf; SELECT * FROM cpu"), None);
    }

    #[test]
    fn use_and_errors() {
        assert_eq!(use_target("USE telegraf"), Some(("telegraf".into(), None)));
        assert_eq!(use_target("use \"my db\".\"a.rp\""), Some(("my db".into(), Some("a.rp".into()))));
        assert_eq!(use_target("USEFUL"), None);
        assert_eq!(use_target("USE"), None);
        let t = "USE a;\nSHOW DATABASES;\nSELECT * FROM;";
        let base = t.find("SHOW").unwrap();
        // The request was "SHOW DATABASES;\nSELECT * FROM": line 2, char 14.
        let Error::Statement(e) = parse_error("error parsing query: found EOF, expected identifier at line 2, char 14", t, base) else { panic!() };
        assert_eq!((e.line, e.offset), (Some(3), Some(t.len() - 1)));
        assert!(matches!(parse_error("database not found: x", t, 0), Error::Query(_)));
        assert_eq!(ident("a\"b"), r#""a\"b""#);
    }

    #[test]
    fn deletes_points_by_time_and_tags() {
        let keys = vec![
            vec![("time".into(), json!("2024-01-31T13:45:00Z")), ("host".into(), json!("O'Brien")), ("region".into(), J::Null)],
            vec![("time".into(), json!(1706708700000000000_i64))],
        ];
        assert_eq!(
            delete_script("cpu", &keys).unwrap(),
            "DELETE FROM \"cpu\" WHERE time = '2024-01-31T13:45:00Z' AND \"host\" = 'O\\'Brien' AND \"region\" = '';\n\
             DELETE FROM \"cpu\" WHERE time = 1706708700000000000;"
        );
        assert!(matches!(delete_script("cpu", &[vec![("host".into(), json!("a"))]]), Err(Error::Unsupported(_))));
        assert!(matches!(delete_script("cpu", &[vec![("time".into(), J::Null)]]), Err(Error::Unsupported(_))));
    }
}

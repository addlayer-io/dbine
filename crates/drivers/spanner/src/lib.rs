//! Google Cloud Spanner (GoogleSQL dialect) over its REST API. A session is
//! one Spanner session: reads run in single-use read-only transactions, DML
//! in a read-write transaction committed right after, and DDL through
//! `UpdateDatabaseDdl` (a long-running operation we wait for).

#[path = "../../bigquery/src/gcp.rs"]
mod gcp;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod script;
mod security;
mod backup;
mod structure;
mod sync;
mod transfer;

use base64::Engine;
use dbine_driver::sql::{qualified_name, quote_ident, select_top, split_statements, Limit, Quote};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, ForeignKeyDef, IndexDef,
    Language, ObjectKindInfo, ObjectRef, QueryOutcome, ResultColumn, Result, SchemaInfo, Session, TableSchema,
};
use serde_json::{json, Map, Value as Json};
use std::time::Duration;

const API: &str = "https://spanner.googleapis.com";

pub fn drivers() -> Vec<std::sync::Arc<dyn Driver>> {
    vec![std::sync::Arc::new(SpannerDriver { info: info() })]
}

fn info() -> DriverInfo {
    let mut fields = gcp::fields();
    fields.push(Field::new("instance", "Instancia", FieldKind::Text).required());
    fields.push(Field::database().required());
    fields.push(gcp::endpoint_field("http://localhost:9020"));
    fields.push(Field::read_only());
    DriverInfo {
        id: "spanner",
        name: "Google Cloud Spanner",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "spanner",
        default_port: 0,
        fields,
        databases_label: "Bases de datos",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::sequences()],
    }
}

pub struct SpannerDriver {
    info: DriverInfo,
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    tokens: gcp::Tokens,
    base: String,
}

impl Api {
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Json> {
        let req = match self.tokens.bearer().await? {
            Some(b) => req.header("Authorization", b),
            None => req,
        };
        let resp = req.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        if !(200..300).contains(&status) {
            // The emulator's gateway answers `{"code", "message"}`.
            let flat: Option<Json> = serde_json::from_str(&body).ok();
            if let Some(m) = flat.as_ref().and_then(|j| j.get("message")).and_then(Json::as_str) {
                return Err(if status == 401 { Error::AuthFailed(m.into()) } else { Error::Query(m.into()) });
            }
            return Err(gcp::api_error(status, &body));
        }
        Ok(if body.is_empty() { Json::Null } else { serde_json::from_str(&body)? })
    }

    /// `path` is a resource name (`projects/…`) plus an optional `:verb`.
    async fn post(&self, path: &str, body: &Json) -> Result<Json> {
        self.send(self.http.post(format!("{}/v1/{path}", self.base)).json(body)).await
    }

    async fn get(&self, path: &str) -> Result<Json> {
        self.send(self.http.get(format!("{}/v1/{path}", self.base))).await
    }
}

pub struct SpannerSession {
    api: Api,
    instance: String,
    database: String,
    session: String,
    seqno: i64,
    read_only: bool,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// Manual mode: the first DML opens a read-write transaction that
    /// stays open until COMMIT / ROLLBACK.
    manual: bool,
    /// The open read-write transaction (manual mode or `BEGIN`).
    tx: Option<String>,
    /// Spanner aborted it: only a rollback ends it.
    tx_failed: bool,
}

#[async_trait]
impl Driver for SpannerDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        script::dialect()
    }

    /// One request per statement, as spanner-cli runs a file; the session
    /// (and an open read-write transaction) lasts between them.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    /// Read-write transactions across statements (`BEGIN` … `COMMIT`, or
    /// the editor's manual mode), as spanner-cli's `BEGIN RW`.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Databases are created and dropped through the admin API.
    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: true, monitor: true, ..Default::default() }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// `insert` mutations committed by windows (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(DesignerSpec {
            schemas: true,
            comments: false,
            table_options: vec![
                Field::new(OPT_PARENT, "Intercalar en la tabla padre", FieldKind::Text)
                    .help("INTERLEAVE IN PARENT: la clave primaria tiene que empezar con la de la tabla padre."),
                Field::new(
                    OPT_ON_DELETE,
                    "Al borrar el padre",
                    FieldKind::Select(vec![("", "NO ACTION"), ("CASCADE", "CASCADE")]),
                ),
            ],
            ..DesignerSpec::sql_table(DATA_TYPES.to_vec())
        })
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        // Only `{name}`: the default schema is unnamed (``), so `{schema}`.
        // would not parse there. Views must name their columns' tables.
        vec![CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE VIEW `{name}` SQL SECURITY INVOKER AS\nSELECT\n    t.id,\n    t.nombre\nFROM tabla AS t;".into(),
        }]
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    /// Fine-grained access control: database roles (users are IAM's).
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Named schemas (see `security::schema_spec`).
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec())
    }

    fn create_schema_script(&self, _database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(name)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, _cascade: bool) -> Result<String> {
        security::drop_schema(name)
    }

    /// Backups through the Database Admin API, with DBine's own
    /// `CREATE BACKUP` / `RESTORE DATABASE` / `DROP BACKUP` statements.
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(table_ddl(table, parts))
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> Result<String> {
        Ok(insert_script(target, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        Ok(delete_script(target, keys))
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let req = |k: &str, what: &str| {
            cfg.option(k).map(|v| v.trim().to_string()).ok_or_else(|| Error::Connect(format!("falta {what}")))
        };
        let project = req("project_id", "el ID del proyecto")?;
        let instance = format!("projects/{project}/instances/{}", req("instance", "la instancia")?);
        let db = database
            .or(Some(cfg.database.as_str()))
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| Error::Connect("falta la base de datos".into()))?;
        let http = gcp::http_client()?;
        let api = Api {
            tokens: gcp::Tokens::from_config(cfg, http.clone())?,
            http,
            base: cfg.option("endpoint_url").unwrap_or(API).trim_end_matches('/').to_string(),
        };
        let database = format!("{instance}/databases/{db}");
        let session = tokio::time::timeout(Duration::from_secs(20), create_session(&api, &database))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| match e {
                Error::Query(m) => Error::Connect(m),
                other => other,
            })?;
        Ok(Box::new(SpannerSession {
            api,
            instance,
            database,
            session,
            seqno: 0,
            read_only: cfg.read_only,
            profiler: None,
            manual: false,
            tx: None,
            tx_failed: false,
        }))
    }
}

async fn create_session(api: &Api, database: &str) -> Result<String> {
    let s = api.post(&format!("{database}/sessions"), &json!({})).await?;
    s.get("name").and_then(Json::as_str).map(str::to_string).ok_or_else(|| Error::Connect("sesión sin nombre".into()))
}

impl Drop for SpannerSession {
    fn drop(&mut self) {
        let api = self.api.clone();
        let mut names = vec![self.session.clone()];
        names.extend(self.profiler.as_ref().map(profiler::State::session));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                for name in names {
                    let _ = api.send(api.http.delete(format!("{}/v1/{name}", api.base))).await;
                }
            });
        }
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    Read,
    Dml,
    Ddl,
}

fn classify(stmt: &str) -> Kind {
    let s = stmt.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    let word: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    match word.as_str() {
        "INSERT" | "UPDATE" | "DELETE" => Kind::Dml,
        "CREATE" | "ALTER" | "DROP" | "GRANT" | "REVOKE" | "ANALYZE" | "RENAME" | "RESTORE" => Kind::Ddl,
        _ => Kind::Read,
    }
}

impl SpannerSession {
    /// executeSql, recreating the session once if Spanner dropped it (idle
    /// sessions expire after an hour).
    async fn execute_sql(&mut self, mut body: Json) -> Result<Json> {
        for attempt in 0..2 {
            self.seqno += 1;
            body["seqno"] = json!(self.seqno.to_string());
            match self.api.post(&format!("{}:executeSql", self.session), &body).await {
                // An open transaction dies with its session: no retry.
                Err(Error::Query(m)) if m.contains("Session not found") && self.tx.is_some() => {
                    self.tx = None;
                    self.tx_failed = false;
                    return Err(Error::Query(format!("{m} (la transacción abierta se perdió: la sesión venció)")));
                }
                Err(Error::Query(m)) if attempt == 0 && m.contains("Session not found") => {
                    self.session = create_session(&self.api, &self.database).await?;
                }
                other => return other,
            }
        }
        unreachable!("the loop returns")
    }

    async fn read(&mut self, sql: &str, params: Option<(Json, Json)>) -> Result<Json> {
        let mut body = json!({ "sql": sql, "transaction": { "singleUse": { "readOnly": { "strong": true } } } });
        if let Some((p, t)) = params {
            body["params"] = p;
            body["paramTypes"] = t;
        }
        self.execute_sql(body).await
    }

    /// Rows of a catalog query with `@p0, @p1…` string parameters, as text.
    async fn text_rows(&mut self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
        let params = (!args.is_empty()).then(|| {
            let p: Map<String, Json> = args.iter().enumerate().map(|(i, a)| (format!("p{i}"), json!(a))).collect();
            let t: Map<String, Json> =
                (0..args.len()).map(|i| (format!("p{i}"), json!({ "code": "STRING" }))).collect();
            (Json::Object(p), Json::Object(t))
        });
        let r = self.read(sql, params).await?;
        Ok(r.get("rows")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .map(|row| {
                row.as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| match v {
                        Json::Null => None,
                        Json::String(s) => Some(s.clone()),
                        other => Some(other.to_string()),
                    })
                    .collect()
            })
            .collect())
    }

    async fn dml(&mut self, sql: &str) -> Result<u64> {
        let r = self.dml_mode(sql, None).await?;
        Ok(r.pointer("/stats/rowCountExact").and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64())).unwrap_or(0))
    }

    /// DML in a read-write transaction: committed when it ran (normal or
    /// PROFILE), rolled back when it was only planned (PLAN).
    async fn dml_mode(&mut self, sql: &str, mode: Option<&str>) -> Result<Json> {
        let mut body = json!({ "sql": sql, "transaction": { "begin": { "readWrite": {} } } });
        if let Some(m) = mode {
            body["queryMode"] = json!(m);
        }
        let r = self.execute_sql(body).await?;
        let tx = r.pointer("/metadata/transaction/id").and_then(Json::as_str).unwrap_or_default().to_string();
        let end = if mode == Some("PLAN") { "rollback" } else { "commit" };
        self.api.post(&format!("{}:{end}", self.session), &json!({ "transactionId": tx })).await?;
        Ok(r)
    }

    /// One editor statement: `raw` is sent (the server's positions count
    /// in it), `stmt` (without comments) tells what it is. In a read-write
    /// transaction reads and DML run inside it; DDL can't.
    async fn run_statement(&mut self, raw: &str, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if let Some(cmd) = script::tx_command(stmt) {
            match cmd {
                script::TxCommand::Begin if self.tx.is_some() => {
                    return Err(Error::Query("Ya hay una transacción abierta: confirmala (COMMIT) o deshacela (ROLLBACK) antes.".into()));
                }
                script::TxCommand::Begin => {
                    if self.read_only {
                        return Err(Error::Query("Conexión de solo lectura: solo se permiten consultas.".into()));
                    }
                    let r = self.api.post(&format!("{}:beginTransaction", self.session), &json!({ "options": { "readWrite": {} } })).await?;
                    self.tx = r.get("id").and_then(Json::as_str).map(str::to_string);
                    self.tx_failed = false;
                    out.info("Transacción iniciada.");
                }
                script::TxCommand::Commit => {
                    let had = self.tx.is_some();
                    self.commit().await?;
                    out.info(if had { "Transacción confirmada." } else { "No hay una transacción abierta." });
                }
                script::TxCommand::Rollback => {
                    let had = self.tx.is_some();
                    self.rollback().await?;
                    out.info(if had { "Transacción deshecha." } else { "No hay una transacción abierta." });
                }
            }
            out.push_affected(0);
            if let Some(r) = out.results.last_mut() {
                r.tag = stmt.split_whitespace().next().map(str::to_ascii_uppercase);
            }
            return Ok(());
        }
        let kind = classify(stmt);
        if self.read_only && kind != Kind::Read {
            return Err(Error::Query("Conexión de solo lectura: solo se permiten consultas.".into()));
        }
        let tag: String = stmt.split_whitespace().take(if kind == Kind::Ddl { 2 } else { 1 }).collect::<Vec<_>>().join(" ").to_ascii_uppercase();
        match kind {
            Kind::Ddl if self.tx.is_some() => {
                return Err(Error::Query(
                    "Spanner no ejecuta DDL dentro de una transacción: confirmala (COMMIT) o deshacela (ROLLBACK) antes.".into(),
                ))
            }
            Kind::Ddl => match backup::parse(stmt)? {
                Some(cmd) => backup::run(self, cmd, out).await?,
                None => {
                    self.ddl(raw).await?;
                    out.push_affected(0);
                }
            },
            Kind::Dml if self.tx.is_some() || self.manual => {
                let selector = match &self.tx {
                    Some(id) => json!({ "id": id }),
                    None => json!({ "begin": { "readWrite": {} } }),
                };
                let r = match self.execute_sql(json!({ "sql": raw, "transaction": selector })).await {
                    Ok(r) => r,
                    Err(e) => {
                        if self.tx.is_some() && e.to_string().contains("ABORTED") {
                            self.tx_failed = true;
                        }
                        return Err(e);
                    }
                };
                if self.tx.is_none() {
                    self.tx = r.pointer("/metadata/transaction/id").and_then(Json::as_str).map(str::to_string);
                }
                let n = r.pointer("/stats/rowCountExact").and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64()));
                out.push_affected(n.unwrap_or(0));
            }
            Kind::Dml => {
                let n = self.dml(raw).await?;
                out.push_affected(n);
            }
            Kind::Read if self.tx.is_some() => {
                let id = self.tx.clone().unwrap_or_default();
                let r = self.execute_sql(json!({ "sql": raw, "transaction": { "id": id } })).await?;
                push_rows(&r, max_rows, out);
            }
            Kind::Read => {
                let r = self.read(raw, None).await?;
                push_rows(&r, max_rows, out);
            }
        }
        if kind != Kind::Read {
            if let Some(r) = out.results.last_mut() {
                r.tag.get_or_insert(tag);
            }
        }
        Ok(())
    }

    /// COMMIT or ROLLBACK of the open transaction (nothing when none).
    async fn end_transaction(&mut self, how: &str) -> Result<()> {
        let Some(id) = self.tx.take() else { return Ok(()) };
        let failed = std::mem::take(&mut self.tx_failed);
        let r = self.api.post(&format!("{}:{how}", self.session), &json!({ "transactionId": id })).await;
        match (r, how) {
            (Ok(_), _) => Ok(()),
            // A failed commit leaves it aborted: only a rollback ends it.
            (Err(e), "commit") => {
                self.tx = Some(id);
                self.tx_failed = true;
                Err(e)
            }
            // Rolling back what Spanner already aborted.
            (Err(_), _) if failed => Ok(()),
            (Err(e), _) => Err(e),
        }
    }

    async fn ddl(&mut self, sql: &str) -> Result<()> {
        let url = format!("{}/v1/{}/ddl", self.api.base, self.database);
        let op = self.api.send(self.api.http.patch(url).json(&json!({ "statements": [sql] }))).await?;
        self.wait(op).await
    }

    /// Wait for a long-running admin operation; its error as `Err`.
    async fn wait(&self, mut op: Json) -> Result<()> {
        let mut delay = Duration::from_millis(200);
        while op.get("done").and_then(Json::as_bool) != Some(true) {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            let name = op.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
            if name.is_empty() {
                break;
            }
            op = self.api.get(&name).await?;
        }
        if let Some(m) = op.pointer("/error/message").and_then(Json::as_str) {
            return Err(Error::Query(m.to_string()));
        }
        Ok(())
    }
}

const DATA_TYPES: &[&str] = &[
    "INT64", "FLOAT64", "FLOAT32", "NUMERIC", "BOOL", "STRING(MAX)", "STRING(255)", "BYTES(MAX)", "DATE", "TIMESTAMP",
    "JSON", "ARRAY<STRING(MAX)>", "ARRAY<INT64>",
];
/// Table options: `INTERLEAVE IN PARENT <parent> [ON DELETE CASCADE]`.
const OPT_PARENT: &str = "interleave_in_parent";
const OPT_ON_DELETE: &str = "on_delete";
/// Column options of generated columns: `AS (<expr>) [STORED]`.
const OPT_GENERATED: &str = "generated_as";
const OPT_STORED: &str = "stored";
/// Columns left out of `SELECT *` (the TOKENLIST ones of search indexes).
const OPT_HIDDEN: &str = "hidden";

fn bq(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

fn bq_qualified(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Backtick, schema.filter(|s| !s.is_empty()), name)
}

/// A column as CREATE TABLE and `ADD COLUMN` write it: type, NOT NULL, and
/// the generated expression, identity or default.
fn column_def(c: &ColumnDef) -> String {
    let mut l = format!("{} {}", bq(&c.name), c.data_type);
    if !c.nullable {
        l.push_str(" NOT NULL");
    }
    if let Some(g) = c.options.get(OPT_GENERATED).filter(|g| !g.is_empty()) {
        l.push_str(&format!(" AS ({g})"));
        if c.options.get(OPT_STORED).is_some_and(|s| s == "true") {
            l.push_str(" STORED");
        }
    } else if c.auto_increment {
        l.push_str(" GENERATED BY DEFAULT AS IDENTITY (BIT_REVERSED_POSITIVE)");
    } else if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
        l.push_str(&format!(" DEFAULT ({d})"));
    }
    if c.options.get(OPT_HIDDEN).is_some_and(|s| s == "true") {
        l.push_str(" HIDDEN");
    }
    l
}

/// GoogleSQL DDL. The primary key goes after the column list and is
/// required; interleaving, defaults `DEFAULT (expr)`, identity columns and
/// generated columns are kept; there are no comments. Dropping a table
/// needs its indexes dropped first.
fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let schema = t.schema.as_deref();
    let name = bq_qualified(schema, &t.name);
    let guard = |s: &'static str| if parts.if_exists { s } else { "" };
    let cols = |c: &[String]| c.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", ");
    let mut out: Vec<String> = Vec::new();

    if parts.drop {
        for ix in &t.indexes {
            out.push(structure::drop_index(schema, ix, parts.if_exists));
        }
        out.push(format!("DROP TABLE {}{name};", guard("IF EXISTS ")));
    }

    if parts.create {
        let mut lines: Vec<String> = t.columns.iter().map(|c| format!("  {}", column_def(c))).collect();
        lines.extend(t.checks.iter().map(|c| format!("  {}", structure::check_clause(&t.name, c))));
        let pk = t.primary_key.as_ref().map(|k| cols(&k.columns)).unwrap_or_default();
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n) PRIMARY KEY ({pk})",
            if parts.drop { "" } else { guard("IF NOT EXISTS ") },
            lines.join(",\n")
        );
        if let Some(p) = t.options.get(OPT_PARENT).filter(|p| !p.is_empty()) {
            // The parent is named as written (it may carry its schema).
            s.push_str(&format!(",\n  INTERLEAVE IN PARENT {}", p.split('.').map(bq).collect::<Vec<_>>().join(".")));
            if t.options.get(OPT_ON_DELETE).is_some_and(|a| a.eq_ignore_ascii_case("CASCADE")) {
                s.push_str(" ON DELETE CASCADE");
            }
        }
        s.push(';');
        out.push(s);
    }

    if parts.indexes {
        for ix in &t.indexes {
            out.push(structure::index_ddl(schema, &t.name, ix, parts.if_exists));
        }
    }

    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            let mut s = format!("ALTER TABLE {name} ADD ");
            if let Some(n) = fk.name.as_deref().filter(|n| !n.is_empty()) {
                s.push_str(&format!("CONSTRAINT {} ", bq(n)));
            }
            let target = bq_qualified(fk.ref_schema.as_deref().or(schema), &fk.ref_table);
            s.push_str(&format!("FOREIGN KEY ({}) REFERENCES {target} ({})", cols(&fk.columns), cols(&fk.ref_columns)));
            // Spanner has ON DELETE CASCADE / NO ACTION and no ON UPDATE.
            if fk.on_delete.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("CASCADE")) {
                s.push_str(" ON DELETE CASCADE");
            }
            s.push(';');
            out.push(s);
        }
    }
    out.join("\n")
}

/// A GoogleSQL string literal. The quote goes as `\x27` rather than `\'`
/// so statement splitting (which knows no backslash escapes) stays right.
fn lit(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => o.push_str("\\\\"),
            '\'' => o.push_str("\\x27"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

fn literal(v: &Json) -> String {
    match v {
        Json::Null => "NULL".into(),
        Json::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Json::Number(n) => n.to_string(),
        Json::String(s) => lit(s),
        other => lit(&other.to_string()),
    }
}

/// Multi-row `INSERT … VALUES`, 100 rows per statement.
fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> String {
    let head = format!(
        "INSERT INTO {} ({}) VALUES",
        bq_qualified(target.schema(), &target.name),
        columns.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", ")
    );
    rows.chunks(100)
        .map(|chunk| {
            let tuples: Vec<String> =
                chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
            format!("{head}\n  {};", tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … WHERE <key>` per changed row, with GoogleSQL literals.
/// Spanner rejects an UPDATE without WHERE, so every change needs a key.
fn update_script(target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
    if changes.iter().any(|c| !c.set.is_empty() && c.key.is_empty()) {
        return Err(Error::Unsupported("Cloud Spanner exige un WHERE: la fila necesita su clave primaria".into()));
    }
    Ok(dbine_driver::ddl::update_script_with(Quote::Backtick, target.schema(), &target.name, changes, &literal))
}

/// `DELETE … WHERE <key>` per row key, with GoogleSQL literals. A key
/// without columns is skipped (Spanner rejects a DELETE without WHERE).
fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Json)>]) -> String {
    dbine_driver::ddl::delete_script_with(Quote::Backtick, target.schema(), &target.name, keys, &literal)
}

/// The browse query restricted by the grid's column filters, with
/// GoogleSQL literals. LIKE has no ESCAPE clause there: patterns rely on
/// the default `\` escape.
fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &literal, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
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

/// `("", "t")` → `(None, "t")`: the default schema is the empty string.
fn schema_of(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

#[async_trait]
impl Session for SpannerSession {
    async fn server_version(&mut self) -> Result<String> {
        let emulated = !self.api.base.starts_with(API);
        Ok(format!("Cloud Spanner{} ({})", if emulated { " (emulador)" } else { "" }, self.database))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = format!("{}/databases?pageSize=1000", self.instance);
            if let Some(t) = token.take() {
                url.push_str(&format!("&pageToken={t}"));
            }
            let r = self.api.get(&url).await?;
            for d in r.get("databases").and_then(Json::as_array).into_iter().flatten() {
                if let Some(n) = d.get("name").and_then(Json::as_str) {
                    out.push(n.rsplit('/').next().unwrap_or(n).to_string());
                }
            }
            match r.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rows = self
            .text_rows(
                "SELECT table_schema, table_name, table_type FROM INFORMATION_SCHEMA.TABLES
                 WHERE table_schema NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS') ORDER BY table_schema, table_name",
                &[],
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .into_iter()
            .map(|r| DbObject {
                kind: if r.get(2).cloned().flatten().as_deref() == Some("VIEW") { kinds::VIEW } else { kinds::TABLE }.into(),
                schema: schema_of(r.first().cloned().flatten()),
                name: r.get(1).cloned().flatten().unwrap_or_default(),
                parent: None,
            })
            .collect();
        let seqs = self.text_rows("SELECT SCHEMA, NAME FROM INFORMATION_SCHEMA.SEQUENCES ORDER BY SCHEMA, NAME", &[]).await?;
        out.extend(seqs.into_iter().map(|r| DbObject {
            kind: kinds::SEQUENCE.into(),
            schema: schema_of(r.first().cloned().flatten()),
            name: r.get(1).cloned().flatten().unwrap_or_default(),
            parent: None,
        }));
        Ok(out)
    }

    /// `INFORMATION_SCHEMA.SCHEMATA`, without the default schema (named
    /// ""; its objects carry no schema); INFORMATION_SCHEMA and SPANNER_SYS
    /// are the system ones.
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let rows = self.text_rows("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA ORDER BY SCHEMA_NAME", &[]).await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|r| schema_of(r.into_iter().next().flatten()))
                .map(|name| SchemaInfo { system: name == "INFORMATION_SCHEMA" || name == "SPANNER_SYS", name })
                .collect(),
        ))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = obj.schema().unwrap_or("");
        let pk = self
            .text_rows(
                "SELECT column_name FROM INFORMATION_SCHEMA.INDEX_COLUMNS
                 WHERE table_schema = @p0 AND table_name = @p1 AND index_type = 'PRIMARY_KEY'",
                &[schema, &obj.name],
            )
            .await?;
        let pk: Vec<String> = pk.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        let rows = self
            .text_rows(
                "SELECT column_name, spanner_type, is_nullable, column_default, is_generated
                 FROM INFORMATION_SCHEMA.COLUMNS WHERE table_schema = @p0 AND table_name = @p1
                 ORDER BY ordinal_position",
                &[schema, &obj.name],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let s = |i: usize| r.get(i).cloned().flatten();
                let name = s(0).unwrap_or_default();
                ColumnInfo {
                    primary_key: pk.contains(&name),
                    name,
                    data_type: s(1).unwrap_or_default(),
                    nullable: s(2).as_deref() != Some("NO"),
                    default_value: s(3),
                    auto_increment: false,
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let schema = obj.schema().unwrap_or("");
        if obj.kind == kinds::VIEW {
            let rows = self
                .text_rows(
                    "SELECT view_definition FROM INFORMATION_SCHEMA.VIEWS WHERE table_schema = @p0 AND table_name = @p1",
                    &[schema, &obj.name],
                )
                .await?;
            let q = qualified(obj);
            return Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).map(|b| format!("CREATE VIEW {q} SQL SECURITY INVOKER AS\n{b}")));
        }
        // The database DDL has every CREATE TABLE and CREATE SEQUENCE; pick this one's.
        let ddl = self.api.get(&format!("{}/ddl", self.database)).await?;
        let full = if schema.is_empty() { obj.name.clone() } else { format!("{schema}.{}", obj.name) };
        let what = if obj.kind == kinds::SEQUENCE { "SEQUENCE" } else { "TABLE" };
        Ok(ddl
            .get("statements")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .find(|s| structure::created_name(s, what).is_some_and(|n| n == full))
            .map(|s| format!("{s};")))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Backtick, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let d = script::dialect();
        for unit in dbine_driver::sql::split_script(text, &d) {
            let stmt = dbine_driver::sql::strip_comments(&unit.text, &d, false).trim().to_string();
            if stmt.is_empty() {
                continue;
            }
            match self.run_statement(&unit.text, &stmt, max_rows, out).await {
                Ok(()) => {}
                Err(Error::Query(m)) => return Err(script::shift(script::error(&m, &unit.text), &unit)),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<Option<dbine_driver::TxState>> {
        Ok(Some(match (&self.tx, self.tx_failed) {
            (Some(_), true) => dbine_driver::TxState::Failed,
            (Some(_), false) => dbine_driver::TxState::Open,
            (None, _) => dbine_driver::TxState::Idle,
        }))
    }

    /// Back to autocommit commits what's open (as JDBC does).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on && self.tx.is_some() {
            self.commit().await?;
        }
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.end_transaction("commit").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.end_transaction("rollback").await
    }

    /// `queryMode: PLAN` (nothing runs; DML is planned in a transaction
    /// that is rolled back) or `PROFILE` (the statement runs once, as with
    /// `execute`, and each operator carries its execution stats).
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mode = if analyze { "PROFILE" } else { "PLAN" };
        for stmt in split_statements(text) {
            let kind = classify(&stmt);
            if analyze && self.read_only && kind != Kind::Read {
                return Err(Error::Query("Conexión de solo lectura: solo se permiten consultas.".into()));
            }
            let r = match kind {
                Kind::Ddl if analyze => {
                    match backup::parse(&stmt)? {
                        Some(cmd) => backup::run(self, cmd, out).await?,
                        None => {
                            self.ddl(&stmt).await?;
                            out.push_affected(0);
                        }
                    }
                    continue;
                }
                Kind::Ddl => {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.split_whitespace().collect::<Vec<_>>().join(" ")));
                    continue;
                }
                Kind::Dml => {
                    let r = self.dml_mode(&stmt, Some(mode)).await?;
                    if analyze {
                        let n = r.pointer("/stats/rowCountExact").and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64()));
                        out.push_affected(n.unwrap_or(0));
                    }
                    r
                }
                Kind::Read => {
                    let r = self
                        .execute_sql(json!({ "sql": stmt, "queryMode": mode, "transaction": { "singleUse": { "readOnly": { "strong": true } } } }))
                        .await?;
                    if analyze {
                        push_rows(&r, max_rows, out);
                    }
                    r
                }
            };
            match plan::from_response(&stmt, &r, analyze) {
                Some(p) => out.plans.push(p),
                None => out.messages.push("Spanner no devolvió el plan de la consulta.".into()),
            }
        }
        Ok(())
    }

    /// INFORMATION_SCHEMA in three queries: columns (with interleaving),
    /// index columns (primary key included; FK backing indexes left out)
    /// and foreign key columns. Spanner has no comments.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let cols = self
            .text_rows(
                "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, t.PARENT_TABLE_NAME, t.ON_DELETE_ACTION, c.COLUMN_NAME, c.SPANNER_TYPE,
                        c.IS_NULLABLE, c.COLUMN_DEFAULT, c.GENERATION_EXPRESSION, c.IS_STORED, c.IS_IDENTITY, c.IS_HIDDEN
                 FROM INFORMATION_SCHEMA.COLUMNS c
                 JOIN INFORMATION_SCHEMA.TABLES t ON t.TABLE_CATALOG = c.TABLE_CATALOG AND t.TABLE_SCHEMA = c.TABLE_SCHEMA
                  AND t.TABLE_NAME = c.TABLE_NAME
                 WHERE t.TABLE_TYPE = 'BASE TABLE' AND t.TABLE_SCHEMA NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS')
                 ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION",
                &[],
            )
            .await?;
        let mut out: Vec<TableSchema> = Vec::new();
        for r in cols {
            let s = |i: usize| r.get(i).cloned().flatten();
            let (schema, name) = (schema_of(s(0)), s(1).unwrap_or_default());
            if out.last().is_none_or(|t| t.schema != schema || t.name != name) {
                let mut t = TableSchema { kind: kinds::TABLE.into(), schema, name, ..Default::default() };
                if let Some(p) = s(2).filter(|p| !p.is_empty()) {
                    // Reported unqualified; the parent is in the child's schema.
                    let p = match &t.schema {
                        Some(sch) => format!("{sch}.{p}"),
                        None => p,
                    };
                    t.options.insert(OPT_PARENT.into(), p);
                    if let Some(a) = s(3).filter(|a| a == "CASCADE") {
                        t.options.insert(OPT_ON_DELETE.into(), a);
                    }
                }
                out.push(t);
            }
            let mut c = ColumnDef {
                name: s(4).unwrap_or_default(),
                data_type: s(5).unwrap_or_default(),
                nullable: s(6).as_deref() != Some("NO"),
                default_value: s(7),
                auto_increment: s(10).as_deref() == Some("YES"),
                ..Default::default()
            };
            if let Some(g) = s(8) {
                c.options.insert(OPT_GENERATED.into(), g);
                c.options.insert(OPT_STORED.into(), (s(9).as_deref() == Some("YES")).to_string());
            }
            if s(11).as_deref() == Some("true") {
                c.options.insert(OPT_HIDDEN.into(), "true".into());
            }
            out.last_mut().unwrap().columns.push(c);
        }
        let find = |out: &[TableSchema], schema: &Option<String>, name: &str| -> Option<usize> {
            out.iter().position(|t| &t.schema == schema && t.name == name)
        };

        let idx = self
            .text_rows(
                "SELECT ic.TABLE_SCHEMA, ic.TABLE_NAME, ic.INDEX_NAME, ic.INDEX_TYPE, ic.COLUMN_NAME, i.IS_UNIQUE, i.IS_NULL_FILTERED,
                        CAST(ic.ORDINAL_POSITION AS STRING), ic.COLUMN_ORDERING, i.PARENT_TABLE_NAME, i.FILTER,
                        i.SEARCH_PARTITION_BY, i.SEARCH_ORDER_BY
                 FROM INFORMATION_SCHEMA.INDEX_COLUMNS ic
                 JOIN INFORMATION_SCHEMA.INDEXES i ON i.TABLE_CATALOG = ic.TABLE_CATALOG AND i.TABLE_SCHEMA = ic.TABLE_SCHEMA
                  AND i.TABLE_NAME = ic.TABLE_NAME AND i.INDEX_NAME = ic.INDEX_NAME AND i.INDEX_TYPE = ic.INDEX_TYPE
                 WHERE ic.TABLE_SCHEMA NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS')
                   AND i.INDEX_TYPE IN ('PRIMARY_KEY', 'INDEX', 'SEARCH', 'VECTOR') AND NOT i.SPANNER_IS_MANAGED
                   AND (ic.ORDINAL_POSITION IS NOT NULL OR i.INDEX_TYPE <> 'PRIMARY_KEY')
                 ORDER BY ic.TABLE_SCHEMA, ic.TABLE_NAME, ic.INDEX_NAME, ic.ORDINAL_POSITION IS NULL, ic.ORDINAL_POSITION, ic.COLUMN_NAME",
                &[],
            )
            .await?;
        // Search and vector indexes keep their OPTIONS only in the DDL.
        let special = idx.iter().any(|r| matches!(r.get(3).cloned().flatten().as_deref(), Some(structure::SEARCH | structure::VECTOR)));
        let ddl_options = if special {
            let ddl = self.api.get(&format!("{}/ddl", self.database)).await?;
            structure::index_options_from_ddl(ddl.get("statements").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str))
        } else {
            Default::default()
        };
        for r in idx {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let o = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
            let (schema, table) = (schema_of(Some(s(0))), s(1));
            let Some(ti) = find(&out, &schema, &table) else { continue };
            let t = &mut out[ti];
            let (index, column, ty) = (s(2), s(4), s(3));
            if ty == "PRIMARY_KEY" {
                t.primary_key.get_or_insert_with(Default::default).columns.push(column);
                continue;
            }
            let pos = t.indexes.iter().position(|i| i.name == index).unwrap_or_else(|| {
                let mut ix = IndexDef {
                    name: index.clone(),
                    unique: s(5) == "true" && ty == "INDEX",
                    kind: match ty.as_str() {
                        structure::SEARCH | structure::VECTOR => Some(ty.clone()),
                        _ => (s(6) == "true").then(|| structure::NULL_FILTERED.into()),
                    },
                    filter: o(10),
                    ..Default::default()
                };
                if let Some(p) = o(9) {
                    // Reported unqualified, like a table's parent.
                    ix.options.insert(structure::OPT_INTERLEAVE.into(), match &schema {
                        Some(sch) => format!("{sch}.{p}"),
                        None => p,
                    });
                }
                if let Some(p) = o(11) {
                    ix.options.insert(structure::OPT_PARTITION_BY.into(), p);
                }
                if let Some(p) = o(12) {
                    ix.options.insert(structure::OPT_ORDER_BY.into(), p);
                }
                let full = match &schema {
                    Some(sch) => format!("{sch}.{index}"),
                    None => index.clone(),
                };
                if let Some(opts) = ddl_options.get(&full) {
                    ix.options.extend(opts.clone());
                }
                t.indexes.push(ix);
                t.indexes.len() - 1
            });
            let ix = &mut t.indexes[pos];
            if o(7).is_none() {
                ix.include.push(column);
            } else {
                if s(8) == "DESC" {
                    let d = ix.options.entry(structure::OPT_DESC.into()).or_default();
                    if !d.is_empty() {
                        d.push_str(", ");
                    }
                    d.push_str(&column);
                }
                ix.columns.push(column);
            }
        }

        // CHECKs, without the ones Spanner keeps for NOT NULL columns.
        let checks = self
            .text_rows(
                "SELECT tc.TABLE_SCHEMA, tc.TABLE_NAME, cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE
                 FROM INFORMATION_SCHEMA.CHECK_CONSTRAINTS cc
                 JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_CATALOG = cc.CONSTRAINT_CATALOG
                  AND tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME
                 WHERE tc.CONSTRAINT_TYPE = 'CHECK' AND tc.TABLE_SCHEMA NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS')
                 ORDER BY tc.TABLE_SCHEMA, tc.TABLE_NAME, cc.CONSTRAINT_NAME",
                &[],
            )
            .await?;
        for r in checks {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let name = s(2);
            if structure::not_null_check(&name) {
                continue;
            }
            let Some(ti) = find(&out, &schema_of(Some(s(0))), &s(1)) else { continue };
            out[ti].checks.push(dbine_driver::CheckDef { name: Some(name), expression: s(3) });
        }

        let fks = self
            .text_rows(
                "SELECT k.TABLE_SCHEMA, k.TABLE_NAME, rc.CONSTRAINT_NAME, k.COLUMN_NAME, u.TABLE_SCHEMA, u.TABLE_NAME,
                        u.COLUMN_NAME, rc.DELETE_RULE
                 FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS rc
                 JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k ON k.CONSTRAINT_CATALOG = rc.CONSTRAINT_CATALOG
                  AND k.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = rc.CONSTRAINT_NAME
                 JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE u ON u.CONSTRAINT_CATALOG = rc.UNIQUE_CONSTRAINT_CATALOG
                  AND u.CONSTRAINT_SCHEMA = rc.UNIQUE_CONSTRAINT_SCHEMA AND u.CONSTRAINT_NAME = rc.UNIQUE_CONSTRAINT_NAME
                  AND u.ORDINAL_POSITION = k.POSITION_IN_UNIQUE_CONSTRAINT
                 ORDER BY k.TABLE_SCHEMA, k.TABLE_NAME, rc.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                &[],
            )
            .await?;
        for r in fks {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let Some(ti) = find(&out, &schema_of(Some(s(0))), &s(1)) else { continue };
            let t = &mut out[ti];
            let name = Some(s(2));
            if let Some(fk) = t.foreign_keys.iter_mut().find(|f| f.name == name) {
                fk.columns.push(s(3));
                fk.ref_columns.push(s(6));
            } else {
                t.foreign_keys.push(ForeignKeyDef {
                    name,
                    columns: vec![s(3)],
                    ref_schema: schema_of(Some(s(4))),
                    ref_table: s(5),
                    ref_columns: vec![s(6)],
                    on_delete: (s(7) == "CASCADE").then(|| "CASCADE".into()),
                    on_update: None,
                });
            }
        }
        Ok(out)
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, database).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        if let Some(old) = self.profiler.take() {
            profiler::stop(self, old).await;
        }
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
        if let Some(state) = self.profiler.take() {
            profiler::stop(self, state).await;
        }
        Ok(())
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        let body = json!({ "createStatement": format!("CREATE DATABASE {}", bq(name)) });
        let op = self.api.post(&format!("{}/databases", self.instance), &body).await?;
        self.wait(op).await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, source, progress).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let path = format!("{}/databases/{name}", self.instance);
        if path == self.database {
            return Err(Error::Query(format!("No se puede borrar la base «{name}»: es la de esta conexión.")));
        }
        self.api.send(self.api.http.delete(format!("{}/v1/{path}", self.api.base))).await.map(|_| ())
    }

    /// IAM's permission tests on the instance and the database (see
    /// `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::check(self, database).await)
    }
}

/// A read's rows into `out`.
fn push_rows(r: &Json, max_rows: usize, out: &mut QueryOutcome) {
    let fields = r.pointer("/metadata/rowType/fields").and_then(Json::as_array).cloned().unwrap_or_default();
    out.begin_result(
        fields
            .iter()
            .map(|f| ResultColumn {
                name: f.get("name").and_then(Json::as_str).unwrap_or("").to_string(),
                type_name: type_name(f.get("type").unwrap_or(&Json::Null)),
            })
            .collect(),
    );
    for row in r.get("rows").and_then(Json::as_array).into_iter().flatten() {
        let vals = row.as_array().map_or(&[][..], Vec::as_slice);
        out.push_row(
            fields
                .iter()
                .enumerate()
                .map(|(i, f)| cell(vals.get(i).unwrap_or(&Json::Null), f.get("type").unwrap_or(&Json::Null)))
                .collect(),
            max_rows,
        );
    }
}

fn qualified(obj: &ObjectRef) -> String {
    match obj.schema() {
        Some(s) => format!("{}.{}", quote_ident(Quote::Backtick, s), quote_ident(Quote::Backtick, &obj.name)),
        None => quote_ident(Quote::Backtick, &obj.name),
    }
}

/// `INT64`, `ARRAY<STRING>`, `STRUCT<…>`.
fn type_name(t: &Json) -> String {
    let code = t.get("code").and_then(Json::as_str).unwrap_or("");
    match code {
        "ARRAY" => format!("ARRAY<{}>", type_name(t.get("arrayElementType").unwrap_or(&Json::Null))),
        "STRUCT" => "STRUCT".into(),
        _ => code.to_string(),
    }
}

fn cell(v: &Json, t: &Json) -> Json {
    match t.get("code").and_then(Json::as_str) {
        _ if v.is_null() => Json::Null,
        Some("ARRAY") | Some("STRUCT") => Json::String(typed(v, t).to_string()),
        Some("JSON") => v.as_str().and_then(|s| serde_json::from_str::<Json>(s).ok()).map_or_else(|| v.clone(), |j| Json::String(j.to_string())),
        _ => typed(v, t),
    }
}

fn typed(v: &Json, t: &Json) -> Json {
    if v.is_null() {
        return Json::Null;
    }
    let code = t.get("code").and_then(Json::as_str).unwrap_or("");
    match (code, v) {
        ("ARRAY", Json::Array(a)) => {
            let et = t.get("arrayElementType").unwrap_or(&Json::Null);
            Json::Array(a.iter().map(|e| typed(e, et)).collect())
        }
        ("STRUCT", Json::Array(a)) => {
            let fields = t.pointer("/structType/fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
            let m: Map<String, Json> = fields
                .iter()
                .zip(a)
                .map(|(f, e)| {
                    (f.get("name").and_then(Json::as_str).unwrap_or("").to_string(), typed(e, f.get("type").unwrap_or(&Json::Null)))
                })
                .collect();
            Json::Object(m)
        }
        ("INT64" | "ENUM", Json::String(s)) => s.parse::<i64>().map_or_else(|_| v.clone(), json_i64),
        ("FLOAT64" | "FLOAT32", Json::Number(n)) => n.as_f64().map_or_else(|| v.clone(), json_f64),
        ("BYTES" | "PROTO", Json::String(s)) => {
            base64::engine::general_purpose::STANDARD.decode(s).map_or_else(|_| v.clone(), |b| json_bytes(&b))
        }
        ("TIMESTAMP", Json::String(s)) => Json::String(format!("{} UTC", s.replacen('T', " ", 1).trim_end_matches('Z'))),
        _ => v.clone(),
    }
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
            let grant = |grantable| d.schema_grant_script(None, "ventas", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }
    use dbine_driver::KeyDef;

    #[test]
    fn filtered_browse_uses_googlesql_literals() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Json>| ColumnFilter { column: column.into(), op, values, sql: None };
        let browse = "SELECT *\nFROM `Singers`\nLIMIT 200";
        assert_eq!(
            filtered_browse(
                browse,
                &[
                    f("Name", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("Bio", FilterOp::Contains, vec![json!("50%")]),
                    f("Age", FilterOp::Le, vec![json!(40)]),
                    f("Deleted", FilterOp::NotNull, vec![]),
                    f("Id", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM `Singers`\nWHERE `Name` = 'O\\x27Brien'\n  AND `Bio` LIKE '%50\\\\%%'\n  AND `Age` <= 40\n  AND `Deleted` IS NOT NULL\n  AND `Id` IN (1, 2)\nLIMIT 200"
        );
    }

    #[test]
    fn update_script_by_key() {
        let t = ObjectRef { kind: "table".into(), schema: None, name: "Clientes".into() };
        let c = dbine_driver::RowChange {
            key: vec![("Id".into(), json!(7)), ("Region".into(), Json::Null)],
            set: vec![("Nombre".into(), json!("O'Brien")), ("Baja".into(), Json::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(&t, &[c]).unwrap(),
            "UPDATE `Clientes` SET `Nombre` = 'O\\x27Brien', `Baja` = NULL WHERE `Id` = 7 AND `Region` IS NULL;"
        );
        let keyless = dbine_driver::RowChange { key: vec![], set: vec![("Nombre".into(), json!("x"))], ..Default::default() };
        assert!(update_script(&t, &[keyless]).is_err());
    }

    #[test]
    fn delete_script_by_key() {
        let t = ObjectRef { kind: "table".into(), schema: None, name: "Clientes".into() };
        let keys = vec![vec![("Nombre".into(), json!("O'Brien")), ("Region".into(), Json::Null)], vec![]];
        assert_eq!(delete_script(&t, &keys), "DELETE FROM `Clientes` WHERE `Nombre` = 'O\\x27Brien' AND `Region` IS NULL;");
    }

    #[test]
    fn statements_are_routed_by_keyword() {
        assert_eq!(classify("SELECT 1"), Kind::Read);
        assert_eq!(classify("  (SELECT 1)"), Kind::Read);
        assert_eq!(classify("insert into t (a) values (1)"), Kind::Dml);
        assert_eq!(classify("CREATE TABLE t (a INT64) PRIMARY KEY (a)"), Kind::Ddl);
    }

    #[test]
    fn cells_from_a_recorded_response() {
        let r: Json = serde_json::from_str(
            r#"{"metadata":{"rowType":{"fields":[{"name":"b","type":{"code":"FLOAT64"}},{"name":"c","type":{"code":"BOOL"}},
            {"name":"d","type":{"code":"BYTES"}},{"name":"t","type":{"code":"TIMESTAMP"}},
            {"name":"arr","type":{"code":"ARRAY","arrayElementType":{"code":"INT64"}}},{"name":"n","type":{"code":"NUMERIC"}},
            {"name":"dt","type":{"code":"DATE"}},{"name":"j","type":{"code":"JSON"}},{"name":"nan","type":{"code":"FLOAT64"}},
            {"name":"i","type":{"code":"INT64"}},
            {"name":"st","type":{"code":"ARRAY","arrayElementType":{"code":"STRUCT","structType":{"fields":[{"name":"x","type":{"code":"INT64"}},{"name":"y","type":{"code":"STRING"}}]}}}}]}},
            "rows":[[1.5,true,"YWI=","2024-01-31T13:45:00.5Z",["1","2"],"1.1","2024-01-31","{\"a\": 1}","NaN","9007199254740993",[["1","s"]]]]}"#,
        )
        .unwrap();
        let fields = r.pointer("/metadata/rowType/fields").unwrap().as_array().unwrap();
        let row = r.pointer("/rows/0").unwrap().as_array().unwrap();
        let cells: Vec<Json> = fields.iter().zip(row).map(|(f, v)| cell(v, &f["type"])).collect();
        assert_eq!(
            cells,
            vec![
                json!(1.5),
                json!(true),
                json!("0x6162"),
                json!("2024-01-31 13:45:00.5 UTC"),
                json!("[1,2]"),
                json!("1.1"),
                json!("2024-01-31"),
                json!("{\"a\":1}"),
                json!("NaN"),
                json!("9007199254740993"),
                json!("[{\"x\":1,\"y\":\"s\"}]")
            ]
        );
        assert_eq!(type_name(&fields[4]["type"]), "ARRAY<INT64>");
    }

    #[test]
    fn googlesql_table_ddl() {
        let mut child = TableSchema {
            kind: kinds::TABLE.into(),
            name: "lineas".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "INT64".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "n".into(), data_type: "INT64".into(), nullable: false, ..Default::default() },
                ColumnDef { name: "estado".into(), data_type: "STRING(20)".into(), nullable: true, default_value: Some("'nuevo'".into()), ..Default::default() },
                ColumnDef {
                    name: "doble".into(),
                    data_type: "INT64".into(),
                    nullable: true,
                    options: [(OPT_GENERATED.to_string(), "n * 2".to_string()), (OPT_STORED.into(), "true".into())].into(),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into(), "n".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("fk_prod".into()),
                columns: vec!["n".into()],
                ref_table: "productos".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                on_update: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![IndexDef { name: "ix_estado".into(), columns: vec!["estado".into()], unique: true, kind: Some("NULL_FILTERED".into()), filter: None, ..Default::default() }],
            comment: Some("ignorado".into()),
            ..Default::default()
        };
        child.options.insert(OPT_PARENT.into(), "pedidos".into());
        child.options.insert(OPT_ON_DELETE.into(), "CASCADE".into());
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        assert_eq!(
            table_ddl(&child, all),
            "DROP INDEX IF EXISTS `ix_estado`;\nDROP TABLE IF EXISTS `lineas`;\n\
             CREATE TABLE `lineas` (\n  `id` INT64 NOT NULL GENERATED BY DEFAULT AS IDENTITY (BIT_REVERSED_POSITIVE),\n  `n` INT64 NOT NULL,\n  \
             `estado` STRING(20) DEFAULT ('nuevo'),\n  `doble` INT64 AS (n * 2) STORED\n) PRIMARY KEY (`id`, `n`),\n  \
             INTERLEAVE IN PARENT `pedidos` ON DELETE CASCADE;\n\
             CREATE UNIQUE NULL_FILTERED INDEX IF NOT EXISTS `ix_estado` ON `lineas` (`estado`);\n\
             ALTER TABLE `lineas` ADD CONSTRAINT `fk_prod` FOREIGN KEY (`n`) REFERENCES `productos` (`id`) ON DELETE CASCADE;"
        );
        child.schema = Some("ventas".into());
        let s = table_ddl(&child, DdlParts { create: true, if_exists: true, indexes: true, ..Default::default() });
        assert!(s.starts_with("CREATE TABLE IF NOT EXISTS `ventas`.`lineas` ("), "{s}");
        assert!(s.ends_with("INDEX IF NOT EXISTS `ventas`.`ix_estado` ON `ventas`.`lineas` (`estado`);"), "{s}");
    }

    #[test]
    fn googlesql_inserts() {
        let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() };
        let s = insert_script(&t, &["a".into(), "b".into()], &[vec![json!(1), json!("O'B\\r\nx")], vec![json!(true), Json::Null]]);
        assert_eq!(s, "INSERT INTO `t` (`a`, `b`) VALUES\n  (1, 'O\\x27B\\\\r\\nx'),\n  (TRUE, NULL);");
        assert_eq!(split_statements(&s).len(), 1);
        let d = SpannerDriver { info: info() };
        assert!(d.capabilities().foreign_keys && d.capabilities().create_database);
        let spec = d.designer().unwrap();
        assert!(spec.schemas && spec.auto_increment && spec.foreign_keys && !spec.comments);
        assert_eq!(d.create_templates().iter().map(|t| t.kind).collect::<Vec<_>>(), [kinds::VIEW]);
    }

    #[test]
    fn table_names_in_ddl() {
        let name = |d: &str| structure::created_name(d, "TABLE");
        assert_eq!(name("CREATE TABLE Singers (\n  id INT64\n) PRIMARY KEY(id)").as_deref(), Some("Singers"));
        assert_eq!(name("CREATE TABLE `s`.`t` (a INT64)").as_deref(), Some("s.t"));
        assert_eq!(name("CREATE INDEX i ON t(a)"), None);
    }
}

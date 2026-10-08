//! Snowflake over the SQL API v2 (`/api/v2/statements`), with a
//! programmatic access token (PAT) or key-pair JWT. Every request is its own
//! server session, so a script goes in one request (MULTI_STATEMENT_COUNT=0)
//! to keep `USE`, variables and temp tables between its statements; the
//! session's database, schema, warehouse and role ride along with each one,
//! and what a run changes of them (and its `ALTER SESSION` / `SET`) carries
//! to the next run (see `script`).

use base64::Engine;
mod backup;
mod blocking;
mod blocks;
mod create_db;
mod script;
mod ddl;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod properties;
mod search;
mod security;
mod sync;
mod transfer;

use dbine_driver::sql::{qualified_name, select_top, Limit, Quote};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, MonitorSnapshot,
    ObjectKindInfo, ObjectRef, QueryOutcome, ResultColumn, Result, RowChange, SchemaInfo, Session, TableSchema,
};
use ddl::column_type;
use serde::Serialize;
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(SnowflakeDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "snowflake",
        name: "Snowflake",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "snowflake",
        default_port: 443,
        fields: vec![
            Field::new("account", "Cuenta", FieldKind::Text)
                .required()
                .placeholder("miorg-micuenta")
                .help("Identificador de la cuenta (org-cuenta o localizador.región), o la URL completa."),
            Field::username().required(),
            Field::new(
                "auth_mode",
                "Autenticación",
                FieldKind::Select(vec![("pat", "Token de acceso programático (PAT)"), ("keypair", "Par de claves (JWT)")]),
            )
            .default_value("pat")
            .help("La API SQL de Snowflake no acepta contraseña: usá un PAT o un par de claves."),
            Field::new("token", "Token (PAT)", FieldKind::Password).secret().when("auth_mode", &["pat"]),
            Field::new("private_key", "Clave privada (PEM, sin cifrar)", FieldKind::Textarea)
                .secret()
                .placeholder("-----BEGIN PRIVATE KEY-----")
                .when("auth_mode", &["keypair"]),
            Field::new("warehouse", "Warehouse", FieldKind::Text),
            Field::new("role", "Rol", FieldKind::Text),
            Field::database(),
            Field::new("schema", "Esquema predeterminado", FieldKind::Text).placeholder("PUBLIC"),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::materialized_views(),
            ObjectKindInfo::functions(),
            ObjectKindInfo::procedures(),
            ObjectKindInfo::sequences(),
            ObjectKindInfo::new(kinds::STREAM, "Streams", false, true, true),
            ObjectKindInfo::new(ddl::TASK, "Tareas", false, false, true),
        ],
    }
}

pub struct SnowflakeDriver {
    info: DriverInfo,
}

#[derive(Clone)]
enum Auth {
    Pat(String),
    KeyPair { issuer: String, subject: String, key: Arc<jsonwebtoken::EncodingKey> },
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    /// The body of the last refused request (code, SQLSTATE, position).
    last_error: Arc<Mutex<Option<Json>>>,
}

/// `myorg-acct` / `xy12345.us-east-1` / a URL → the API base URL.
fn base_url(account: &str) -> String {
    let a = account.trim().trim_end_matches('/');
    let host = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://")).unwrap_or(a);
    if host.contains(".snowflakecomputing.") || a.starts_with("http") {
        if a.starts_with("http") {
            a.to_string()
        } else {
            format!("https://{host}")
        }
    } else {
        format!("https://{host}.snowflakecomputing.com")
    }
}

/// The account part of the JWT claims: before the first dot, uppercase.
fn jwt_account(account: &str) -> String {
    let a = account.trim();
    let host = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://")).unwrap_or(a);
    host.split('.').next().unwrap_or(host).to_ascii_uppercase()
}

/// DER bytes from a PEM block.
fn pem_der(pem: &str) -> Result<(String, Vec<u8>)> {
    let pem = pem.trim();
    let label = pem
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
        .ok_or_else(|| Error::AuthFailed("la clave privada no está en formato PEM".into()))?
        .to_string();
    if label.contains("ENCRYPTED") {
        return Err(Error::AuthFailed("la clave privada está cifrada; exportala sin passphrase".into()));
    }
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::AuthFailed(format!("clave privada inválida: {e}")))?;
    Ok((label, der))
}

/// `SHA256:<base64 of the public key's SHA-256>`, as Snowflake shows it in
/// `DESC USER` (RSA_PUBLIC_KEY_FP).
fn public_key_fingerprint(pem: &str) -> Result<String> {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::{KeyPair, RsaKeyPair};
    let (label, der) = pem_der(pem)?;
    let kp = if label == "RSA PRIVATE KEY" { RsaKeyPair::from_der(&der) } else { RsaKeyPair::from_pkcs8(&der) }
        .map_err(|e| Error::AuthFailed(format!("clave privada RSA inválida: {e}")))?;
    let spki = kp.public_key().as_der().map_err(|e| Error::AuthFailed(format!("clave pública: {e}")))?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, spki.as_ref());
    Ok(format!("SHA256:{}", base64::engine::general_purpose::STANDARD.encode(digest.as_ref())))
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    sub: &'a str,
    iat: u64,
    exp: u64,
}

fn auth(cfg: &ConnectionConfig) -> Result<Auth> {
    let account = cfg.option("account").unwrap_or("");
    match cfg.option("auth_mode").unwrap_or("pat") {
        "keypair" => {
            let pem = cfg.option("private_key").ok_or_else(|| Error::AuthFailed("falta la clave privada".into()))?;
            let user = cfg.username_or_empty().trim().to_ascii_uppercase();
            let subject = format!("{}.{user}", jwt_account(account));
            let issuer = format!("{subject}.{}", public_key_fingerprint(pem)?);
            let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.trim().as_bytes())
                .map_err(|e| Error::AuthFailed(format!("clave privada inválida: {e}")))?;
            Ok(Auth::KeyPair { issuer, subject, key: Arc::new(key) })
        }
        _ => {
            let t = cfg
                .option("token")
                .or(cfg.password.as_deref().filter(|p| !p.is_empty()))
                .ok_or_else(|| Error::AuthFailed("falta el token de acceso (PAT)".into()))?;
            Ok(Auth::Pat(t.trim().to_string()))
        }
    }
}

impl Auth {
    /// `(Authorization, X-Snowflake-Authorization-Token-Type)`.
    fn headers(&self) -> Result<(String, &'static str)> {
        match self {
            Auth::Pat(t) => Ok((format!("Bearer {t}"), "PROGRAMMATIC_ACCESS_TOKEN")),
            Auth::KeyPair { issuer, subject, key } => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let claims = Claims { iss: issuer, sub: subject, iat: now, exp: now + 3540 };
                let jwt = jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &claims, key)
                    .map_err(|e| Error::AuthFailed(e.to_string()))?;
                Ok((format!("Bearer {jwt}"), "KEYPAIR_JWT"))
            }
        }
    }
}

impl Api {
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Json)> {
        let (authz, kind) = self.auth.headers()?;
        let resp = req
            .header("Authorization", authz)
            .header("X-Snowflake-Authorization-Token-Type", kind)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        let body: Json = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }));
        match status {
            200 | 202 => Ok((status, body)),
            401 | 403 if body.get("sqlState").is_none() => Err(Error::AuthFailed(message(&body, status))),
            _ => {
                let m = message(&body, status);
                if let Ok(mut g) = self.last_error.lock() {
                    *g = Some(body);
                }
                Err(Error::Query(m))
            }
        }
    }

    async fn post(&self, path: &str, body: &Json) -> Result<(u16, Json)> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body)).await
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, Json)> {
        self.send(self.http.get(format!("{}{path}", self.base)).query(query)).await
    }
}

fn message(body: &Json, status: u16) -> String {
    let m = body.get("message").and_then(Json::as_str).filter(|m| !m.is_empty());
    match (m, body.get("sqlState").and_then(Json::as_str)) {
        (Some(m), Some(s)) => format!("{m} (SQLSTATE {s})"),
        (Some(m), None) => m.to_string(),
        _ => format!("HTTP {status}"),
    }
}

#[derive(Default, Clone)]
struct Context {
    database: Option<String>,
    schema: Option<String>,
    warehouse: Option<String>,
    role: Option<String>,
}

pub struct SnowflakeSession {
    api: Api,
    ctx: Context,
    /// `ALTER SESSION` and variables replayed in each editor request.
    carry: script::Carry,
    handle: Arc<Mutex<Option<String>>>,
    /// The monitor's warehouse-bound parts, refreshed now and then.
    mon: monitor::Cache,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

#[async_trait]
impl Driver for SnowflakeDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<Field> {
        create_db::fields()
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(name, options)
    }

    /// "Propiedades" (see [`properties`]).
    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(database, changes)
    }

    /// snowsql's reading: backslash escapes, `$$` bodies; anonymous blocks
    /// kept whole (see `script`).
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        script::dialect()
    }

    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        script::units(text)
    }

    /// The whole script goes in one request: the SQL API gives each
    /// request its own server session, so a statement per request would
    /// lose temp tables and transactions between them.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::Whole
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            database_properties: true,
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            blocking: true,
            kill_session: true,
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// `INSERT … SELECT … FROM VALUES` with binds (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
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

    /// Hybrid tables' indexes and every table's foreign keys; no usage
    /// counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(ddl::table_ddl(table, parts))
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> Result<String> {
        Ok(ddl::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(ddl::update_script(target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        Ok(ddl::delete_script(target.schema(), &target.name, keys))
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec())
    }

    /// Never with an owner: it's handed over after the grants
    /// (`schema_owner_script`).
    fn create_schema_script(&self, _database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(name)
    }

    /// `GRANT OWNERSHIP … COPY CURRENT GRANTS`, after the grants: once the
    /// schema is another role's, the creating role can't grant on it.
    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        security::schema_owner(name, owner).map(Some)
    }

    /// Always with its contents (see `security::schema_spec`).
    fn drop_schema_script(&self, _database: Option<&str>, name: &str, _cascade: bool) -> Result<String> {
        security::drop_schema(name)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let account = cfg.option("account").ok_or_else(|| Error::Connect("falta la cuenta".into()))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .user_agent("DBine")
            .build()
            .map_err(|e| Error::Connect(e.to_string()))?;
        let api = Api { http, base: base_url(account), auth: auth(cfg)?, last_error: Default::default() };
        let opt = |k: &str| cfg.option(k).map(|v| v.trim().to_string());
        let ctx = Context {
            database: database.or(Some(cfg.database.as_str())).map(str::trim).filter(|d| !d.is_empty()).map(Into::into),
            schema: opt("schema"),
            warehouse: opt("warehouse"),
            role: opt("role"),
        };
        let s = SnowflakeSession {
            api,
            ctx,
            carry: Default::default(),
            handle: Arc::new(Mutex::new(None)),
            mon: monitor::Cache::default(),
            profiler: None,
        };
        tokio::time::timeout(Duration::from_secs(30), s.statement("SELECT 1", None, 1))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| match e {
                Error::Query(m) => Error::Connect(m),
                other => other,
            })?;
        Ok(Box::new(s))
    }
}

/// One statement's result set.
#[derive(Default, Debug)]
struct ResultSet {
    row_type: Vec<Json>,
    rows: Vec<Json>,
    total: u64,
    more: bool,
    /// Rows a DML statement inserted, updated and deleted (`stats`).
    affected: Option<u64>,
}

impl SnowflakeSession {
    fn body(&self, sql: &str, bindings: Option<Json>, multi: bool) -> Json {
        let mut b = json!({ "statement": sql, "timeout": 0 });
        let c = &self.ctx;
        for (k, v) in [("database", &c.database), ("schema", &c.schema), ("warehouse", &c.warehouse), ("role", &c.role)] {
            if let Some(v) = v {
                b[k] = json!(v);
            }
        }
        if let Some(bnd) = bindings {
            b["bindings"] = bnd;
        }
        if multi {
            b["parameters"] = json!({ "MULTI_STATEMENT_COUNT": "0" });
        }
        b
    }

    fn set_handle(&self, h: Option<String>) {
        if let Ok(mut g) = self.handle.lock() {
            *g = h;
        }
    }

    /// Submit and wait: the final (200) response of `sql`.
    async fn submit(&self, sql: &str, bindings: Option<Json>, multi: bool) -> Result<Json> {
        let (mut status, mut body) = self.api.post("/api/v2/statements", &self.body(sql, bindings, multi)).await?;
        let handle = body.get("statementHandle").and_then(Json::as_str).map(str::to_string);
        self.set_handle(handle.clone());
        let mut delay = Duration::from_millis(250);
        while status == 202 {
            let Some(h) = &handle else { break };
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            match self.api.get(&format!("/api/v2/statements/{h}"), &[]).await {
                Ok((s, b)) => (status, body) = (s, b),
                Err(e) => {
                    self.set_handle(None);
                    return Err(e);
                }
            }
        }
        self.set_handle(None);
        Ok(body)
    }

    /// The rows of one finished statement, following its partitions.
    async fn collect(&self, mut first: Json, max_rows: usize) -> Result<ResultSet> {
        if first.get("data").is_none() {
            // A child of a multi-statement request: fetch its first page.
            let h = first.get("statementHandle").and_then(Json::as_str).unwrap_or_default().to_string();
            first = self.wait_handle(&h).await?;
        }
        let meta = first.get("resultSetMetaData").cloned().unwrap_or(Json::Null);
        let affected = first.get("stats").map(|st| {
            ["numRowsInserted", "numRowsUpdated", "numRowsDeleted"]
                .iter()
                .filter_map(|k| st.get(*k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())))
                .sum()
        });
        let mut rs = ResultSet {
            row_type: meta.get("rowType").and_then(Json::as_array).cloned().unwrap_or_default(),
            total: meta.get("numRows").and_then(Json::as_u64).unwrap_or(0),
            affected,
            ..Default::default()
        };
        let partitions = meta.get("partitionInfo").and_then(Json::as_array).map_or(1, Vec::len).max(1);
        let handle = first.get("statementHandle").and_then(Json::as_str).unwrap_or_default().to_string();
        let mut page = first;
        for p in 0..partitions {
            if p > 0 {
                if rs.rows.len() >= max_rows {
                    rs.more = true;
                    break;
                }
                page = self.api.get(&format!("/api/v2/statements/{handle}"), &[("partition", p.to_string())]).await?.1;
            }
            for row in page.get("data").and_then(Json::as_array).into_iter().flatten() {
                if rs.rows.len() < max_rows {
                    rs.rows.push(row.clone());
                } else {
                    rs.more = true;
                }
            }
        }
        rs.total = rs.total.max(rs.rows.len() as u64);
        Ok(rs)
    }

    async fn wait_handle(&self, h: &str) -> Result<Json> {
        let mut delay = Duration::from_millis(250);
        loop {
            let (status, body) = self.api.get(&format!("/api/v2/statements/{h}"), &[]).await?;
            if status != 202 {
                return Ok(body);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// A single statement's rows (catalog queries).
    async fn statement(&self, sql: &str, bindings: Option<Json>, max_rows: usize) -> Result<ResultSet> {
        let body = self.submit(sql, bindings, false).await?;
        self.collect(body, max_rows).await
    }

    /// Text rows of a catalog query with `?` bindings.
    async fn text_rows(&self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
        let bindings = (!args.is_empty()).then(|| {
            Json::Object(
                args.iter()
                    .enumerate()
                    .map(|(i, a)| ((i + 1).to_string(), json!({ "type": "TEXT", "value": a })))
                    .collect(),
            )
        });
        let rs = self.statement(sql, bindings, 100_000).await?;
        Ok(rs
            .rows
            .iter()
            .map(|r| r.as_array().into_iter().flatten().map(|v| v.as_str().map(str::to_string)).collect())
            .collect())
    }

    /// Rows of a catalog query (or SHOW) by lowercase column name.
    async fn named_rows(&self, sql: &str) -> Result<Vec<ddl::Row>> {
        let rs = self.statement(sql, None, 1_000_000).await?;
        let names: Vec<String> =
            rs.row_type.iter().map(|c| c.get("name").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase()).collect();
        Ok(rs
            .rows
            .iter()
            .map(|r| {
                let vals = r.as_array().map_or(&[][..], Vec::as_slice);
                names.iter().zip(vals).filter_map(|(n, v)| Some((n.clone(), v.as_str()?.to_string()))).collect()
            })
            .collect())
    }

    /// Runs a script in one request, its results into `out`; gives each
    /// statement's query id (statement handle). Nothing is carried in or
    /// out: the catalog and security screens' own scripts.
    async fn run_script(&self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<Vec<String>> {
        let stmts: Vec<String> = script::units(text).into_iter().map(|u| u.text).collect();
        self.run_units(&stmts, &[], &[], max_rows, out).await.map(|(ids, _)| ids)
    }

    /// `pre`, `stmts` and `trail` in one request (one statement when it's
    /// all there is). Only `stmts`' results go to `out`; gives their query
    /// ids and the result sets of `trail`.
    async fn run_units(
        &self,
        stmts: &[String],
        pre: &[String],
        trail: &[&str],
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> Result<(Vec<String>, Vec<ResultSet>)> {
        if stmts.is_empty() {
            return Ok(Default::default());
        }
        let all: Vec<&str> = pre.iter().map(String::as_str).chain(stmts.iter().map(String::as_str)).chain(trail.iter().copied()).collect();
        let multi = all.len() > 1;
        // A statement may end in a `--` comment: the `;` goes on a line of
        // its own, or it would be read as part of the comment.
        let body = self.submit(&all.join("\n;\n"), None, multi).await?;
        let children: Vec<Json> = body
            .get("statementHandles")
            .and_then(Json::as_array)
            .map(|hs| hs.iter().map(|h| json!({ "statementHandle": h })).collect())
            .unwrap_or_default();
        let mut parts = if multi && !children.is_empty() { children } else { vec![body] };
        // The server counted the statements as we did: leave out the
        // preamble's and the trailer's.
        let (skip, keep_tail) = if parts.len() == all.len() { (pre.len(), trail.len()) } else { (0, 0) };
        let tail = parts.split_off(parts.len() - keep_tail);
        let mut ids = Vec::new();
        for part in parts.into_iter().skip(skip) {
            ids.push(part.get("statementHandle").and_then(Json::as_str).unwrap_or_default().to_string());
            let rs = self.collect(part, max_rows).await?;
            out.begin_result(
                rs.row_type
                    .iter()
                    .map(|c| ResultColumn {
                        name: c.get("name").and_then(Json::as_str).unwrap_or("").to_string(),
                        type_name: c.get("type").and_then(Json::as_str).unwrap_or("").to_string(),
                    })
                    .collect(),
            );
            for row in &rs.rows {
                let vals = row.as_array().map_or(&[][..], Vec::as_slice);
                out.push_row(
                    rs.row_type.iter().enumerate().map(|(i, t)| cell(t, vals.get(i).unwrap_or(&Json::Null))).collect(),
                    max_rows,
                );
            }
            if let Some(last) = out.results.last_mut() {
                last.total_rows = last.total_rows.max(rs.total);
                last.truncated |= rs.more || rs.total > last.rows.len() as u64;
                last.rows_affected = rs.affected;
            }
        }
        let mut sets = Vec::new();
        for part in tail {
            sets.push(self.collect(part, 10_000).await?);
        }
        Ok((ids, sets))
    }

    /// An editor run: the carried `ALTER SESSION` and variables first, the
    /// script, then the session's context (and variables, when the script
    /// sets any), which the next run starts from. A script that can't
    /// change the context (the Users and permissions and Backups scripts
    /// among them) goes without the context query.
    async fn run_editor(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<Vec<String>> {
        let units = script::units(text);
        let stmts: Vec<String> = units.iter().map(|u| u.text.clone()).collect();
        let sets_vars = units.iter().any(|u| matches!(script::head(&u.text).0.as_str(), "SET" | "UNSET"));
        let mut trail = Vec::new();
        if script::changes_context(&units) {
            trail.push(script::CONTEXT_QUERY);
        }
        if sets_vars {
            trail.push(script::VARIABLES_QUERY);
        }
        if let Ok(mut g) = self.api.last_error.lock() {
            *g = None;
        }
        let pre = self.carry.preamble();
        match self.run_units(&stmts, &pre, &trail, max_rows, out).await {
            Ok((ids, sets)) => {
                if script::leaves_transaction_open(&units) {
                    out.warning(
                        "La transacción quedó abierta al terminar el script y no pasa a la ejecución siguiente (la API de Snowflake usa una sesión por ejecución): confirmala con COMMIT en el mismo script.",
                    );
                }
                self.carry.absorb_alters(&units);
                let mut sets = sets.iter();
                if trail.first() == Some(&script::CONTEXT_QUERY) {
                    if let Some(c) = sets.next() {
                        self.absorb_context(c, out);
                    }
                }
                if let Some(v) = sets.next() {
                    let names: Vec<String> =
                        v.row_type.iter().map(|c| c.get("name").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase()).collect();
                    self.carry.set_vars(&names, &v.rows);
                }
                Ok(ids)
            }
            Err(Error::Query(m)) => {
                let body = self.api.last_error.lock().ok().and_then(|mut g| g.take());
                match body.as_ref().and_then(|b| script::error(b, &units)) {
                    Some(e) => Err(e.into()),
                    None => Err(Error::Query(m)),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The context query's row becomes the session's; a change of database
    /// or schema is told.
    fn absorb_context(&mut self, rs: &ResultSet, out: &mut QueryOutcome) {
        let Some(row) = rs.rows.first().and_then(Json::as_array) else { return };
        let get = |i: usize| row.get(i).and_then(Json::as_str).map(str::to_string);
        let before = (self.ctx.database.clone(), self.ctx.schema.clone());
        self.ctx.database = get(0);
        self.ctx.schema = get(1);
        if let Some(w) = get(2) {
            self.ctx.warehouse = Some(w);
        }
        if let Some(r) = get(3) {
            self.ctx.role = Some(r);
        }
        if before.0 != self.ctx.database {
            // The tab's database follows it, as with SQL Server's `USE`.
            out.database = self.ctx.database.clone();
        }
        if before != (self.ctx.database.clone(), self.ctx.schema.clone()) {
            let shown = [self.ctx.database.as_deref(), self.ctx.schema.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(".");
            out.info(format!("Contexto: {}", if shown.is_empty() { "(ninguno)" } else { &shown }));
        }
    }

    fn database(&self) -> Result<String> {
        self.ctx.database.clone().ok_or_else(|| Error::Query("no hay una base de datos seleccionada".into()))
    }

    /// One statement with a QUERY_TAG (to tell DBine's own statements
    /// apart in the history), its rows as text by lowercase column name.
    async fn tagged_rows(&self, sql: &str, tag: &str) -> Result<monitor::Set> {
        let mut body = self.body(sql, None, false);
        body["parameters"] = json!({ "QUERY_TAG": tag });
        let (status, mut first) = self.api.post("/api/v2/statements", &body).await?;
        if status == 202 {
            let h = first.get("statementHandle").and_then(Json::as_str).unwrap_or_default().to_string();
            first = self.wait_handle(&h).await?;
        }
        let rs = self.collect(first, 10_000).await?;
        Ok(monitor::Set {
            cols: rs.row_type.iter().map(|c| c.get("name").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase()).collect(),
            rows: rs
                .rows
                .iter()
                .map(|r| r.as_array().into_iter().flatten().map(|v| v.as_str().map(str::to_string)).collect())
                .collect(),
        })
    }
}

#[dbine_driver::async_trait]
impl monitor::Runner for SnowflakeSession {
    /// One statement tagged as the monitor's (QUERY_TAG).
    async fn rows(&self, sql: &str) -> Result<monitor::Set> {
        self.tagged_rows(sql, monitor::TAG).await
    }

    fn warehouse(&self) -> Option<String> {
        self.ctx.warehouse.clone()
    }

    fn database(&self) -> Option<String> {
        self.ctx.database.clone()
    }
}

#[async_trait]
impl Session for SnowflakeSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.text_rows("SELECT CURRENT_VERSION()", &[]).await?;
        let v = rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default();
        Ok(format!("Snowflake {v}"))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rs = self.statement("SHOW DATABASES", None, 100_000).await?;
        let idx = column_index(&rs.row_type, "name").unwrap_or(1);
        Ok(rs.rows.iter().filter_map(|r| r.get(idx).and_then(Json::as_str).map(str::to_string)).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Ok(db) = self.database() else { return Ok(Vec::new()) };
        let is = format!("{}.INFORMATION_SCHEMA", qualified_name(Quote::Double, None, &db));
        let mut out = Vec::new();
        let tables = self
            .text_rows(
                &format!(
                    "SELECT table_schema, table_name, table_type FROM {is}.TABLES
                     WHERE table_schema <> 'INFORMATION_SCHEMA' ORDER BY 1, 2"
                ),
                &[],
            )
            .await?;
        for r in tables {
            let kind = match r.get(2).cloned().flatten().as_deref() {
                Some("VIEW") => kinds::VIEW,
                Some("MATERIALIZED VIEW") => kinds::MATERIALIZED_VIEW,
                _ => kinds::TABLE,
            };
            out.push(obj(kind, &r));
        }
        for (view, kind) in [("FUNCTIONS", kinds::FUNCTION), ("PROCEDURES", kinds::PROCEDURE)] {
            let (schema_col, name_col) = if view == "FUNCTIONS" {
                ("function_schema", "function_name")
            } else {
                ("procedure_schema", "procedure_name")
            };
            if let Ok(rows) = self
                .text_rows(
                    &format!(
                        "SELECT DISTINCT {schema_col}, {name_col} FROM {is}.{view}
                         WHERE {schema_col} <> 'INFORMATION_SCHEMA' ORDER BY 1, 2"
                    ),
                    &[],
                )
                .await
            {
                out.extend(rows.iter().map(|r| obj(kind, r)));
            }
        }
        if let Ok(rows) = self
            .text_rows(
                &format!(
                    "SELECT sequence_schema, sequence_name FROM {is}.SEQUENCES
                     WHERE sequence_schema <> 'INFORMATION_SCHEMA' ORDER BY 1, 2"
                ),
                &[],
            )
            .await
        {
            out.extend(rows.iter().map(|r| obj(kinds::SEQUENCE, r)));
        }
        let dbq = qualified_name(Quote::Double, None, &db);
        for (what, kind) in [("STREAMS", kinds::STREAM), ("TASKS", ddl::TASK)] {
            if let Ok(rows) = self.named_rows(&format!("SHOW {what} IN DATABASE {dbq}")).await {
                let mut objs: Vec<DbObject> = rows
                    .iter()
                    .map(|r| DbObject {
                        kind: kind.into(),
                        schema: r.get("schema_name").cloned(),
                        name: r.get("name").cloned().unwrap_or_default(),
                        parent: None,
                    })
                    .collect();
                objs.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
                out.extend(objs);
            }
        }
        Ok(out)
    }

    /// `INFORMATION_SCHEMA.SCHEMATA` of the session's database (the
    /// schemas the current role can see); INFORMATION_SCHEMA is the system one.
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let Ok(db) = self.database() else { return Ok(None) };
        let rows = self
            .text_rows(&format!("SELECT schema_name FROM {}.INFORMATION_SCHEMA.SCHEMATA ORDER BY 1", qualified_name(Quote::Double, None, &db)), &[])
            .await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .map(|name| SchemaInfo { system: name == "INFORMATION_SCHEMA", name })
                .collect(),
        ))
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let db = self.database()?;
        let schema = o.schema().unwrap_or("PUBLIC");
        let is = format!("{}.INFORMATION_SCHEMA", qualified_name(Quote::Double, None, &db));
        let rows = self
            .text_rows(
                &format!(
                    "SELECT column_name, data_type, is_nullable, column_default, is_identity,
                            character_maximum_length, numeric_precision, numeric_scale
                     FROM {is}.COLUMNS WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position"
                ),
                &[schema, &o.name],
            )
            .await?;
        // Primary keys aren't in INFORMATION_SCHEMA.
        let fq = format!("{}.{}", qualified_name(Quote::Double, None, &db), qualified_name(Quote::Double, Some(schema), &o.name));
        let pk: Vec<String> = match self.statement(&format!("SHOW PRIMARY KEYS IN TABLE {fq}"), None, 10_000).await {
            Ok(rs) => {
                let i = column_index(&rs.row_type, "column_name").unwrap_or(4);
                rs.rows.iter().filter_map(|r| r.get(i).and_then(Json::as_str).map(str::to_string)).collect()
            }
            Err(_) => Vec::new(),
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                let s = |i: usize| r.get(i).cloned().flatten();
                let name = s(0).unwrap_or_default();
                ColumnInfo {
                    primary_key: pk.contains(&name),
                    name,
                    data_type: column_type(s(1).unwrap_or_default(), s(5), s(6), s(7)),
                    nullable: s(2).as_deref() != Some("NO"),
                    default_value: s(3),
                    auto_increment: s(4).as_deref() == Some("YES"),
                }
            })
            .collect())
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
        let db = self.database()?;
        let schema = o.schema().unwrap_or("PUBLIC");
        let is = format!("{}.INFORMATION_SCHEMA", qualified_name(Quote::Double, None, &db));
        // Sequences from the catalog: GET_DDL leaves the schema out, so the
        // sync would make them in the session's schema. GET_DDL if it fails.
        if o.kind == kinds::SEQUENCE {
            let sql = format!("SELECT {} FROM {is}.SEQUENCES WHERE sequence_schema = ? AND sequence_name = ?", search::SEQUENCE_COLS);
            match self.text_rows(&sql, &[schema, &o.name]).await {
                Ok(rows) => return Ok(rows.first().map(|r| search::sequence_ddl(schema, &o.name, r))),
                Err(e) => tracing::debug!("snowflake: sequence {} not read from INFORMATION_SCHEMA: {e}", o.name),
            }
        }
        let rows = match o.kind.as_str() {
            k @ (kinds::FUNCTION | kinds::PROCEDURE) => {
                let r = search::routine_source(k);
                self.text_rows(
                    &format!("SELECT {} FROM {is}.{} WHERE {} = ? AND {} = ?", r.expr, r.view, r.schema_col, r.name_col),
                    &[schema, &o.name],
                )
                .await?
            }
            k => {
                let what = match k {
                    kinds::VIEW | kinds::MATERIALIZED_VIEW => "VIEW",
                    kinds::SEQUENCE => "SEQUENCE",
                    kinds::STREAM => "STREAM",
                    ddl::TASK => "TASK",
                    _ => "TABLE",
                };
                let fq = format!(
                    "{}.{}",
                    qualified_name(Quote::Double, None, &db),
                    qualified_name(Quote::Double, Some(schema), &o.name)
                );
                self.text_rows(&format!("SELECT GET_DDL('{what}', ?)"), &[&fq]).await?
            }
        };
        let defs: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        Ok((!defs.is_empty()).then(|| defs.join("\n\n")))
    }

    /// INFORMATION_SCHEMA for tables and columns; keys aren't there, so
    /// `SHOW … KEYS IN DATABASE` (one each for the whole database).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let Ok(db) = self.database() else { return Ok(Vec::new()) };
        let dbq = qualified_name(Quote::Double, None, &db);
        let tables = self
            .named_rows(&format!(
                "SELECT table_schema, table_name, comment, clustering_key, is_transient FROM {dbq}.INFORMATION_SCHEMA.TABLES
                 WHERE table_type = 'BASE TABLE' AND table_schema <> 'INFORMATION_SCHEMA'"
            ))
            .await?;
        let columns = self
            .named_rows(&format!(
                "SELECT table_schema, table_name, column_name, ordinal_position, data_type, character_maximum_length,
                        numeric_precision, numeric_scale, is_nullable, column_default, is_identity, comment
                 FROM {dbq}.INFORMATION_SCHEMA.COLUMNS WHERE table_schema <> 'INFORMATION_SCHEMA'"
            ))
            .await?;
        let pks = self.named_rows(&format!("SHOW PRIMARY KEYS IN DATABASE {dbq}")).await?;
        let uniques = self.named_rows(&format!("SHOW UNIQUE KEYS IN DATABASE {dbq}")).await?;
        let fks = self.named_rows(&format!("SHOW IMPORTED KEYS IN DATABASE {dbq}")).await?;
        Ok(ddl::assemble(&tables, &columns, &pks, &uniques, &fks))
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.statement(&format!("CREATE DATABASE {}", qualified_name(Quote::Double, None, name)), None, 1).await.map(|_| ())
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    async fn search_code(&mut self, query: &dbine_driver::search::CodeSearch) -> Result<Option<dbine_driver::search::CodeSearchReport>> {
        self.search_code_impl(query).await
    }

    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.ctx.database.as_deref() == Some(name) {
            return Err(Error::Query(format!("no se puede borrar la base «{name}»: es la de la sesión actual")));
        }
        self.statement(&format!("DROP DATABASE {}", qualified_name(Quote::Double, None, name)), None, 1).await.map(|_| ())
    }

    fn browse_query(&self, o: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, o.schema(), &o.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.run_editor(text, max_rows, out).await.map(|_| ())
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    /// Estimated: `EXPLAIN USING JSON` per statement (compiled only, no
    /// warehouse). Actual: the script runs once, as with `execute`, then
    /// `GET_QUERY_OPERATOR_STATS` of each statement's query id.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        use plan::StmtKind;
        let stmts = split_script(text);
        if !analyze {
            for stmt in stmts {
                if plan::classify(&stmt) == StmtKind::Other {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                    continue;
                }
                let rs = self.statement(&format!("EXPLAIN USING JSON {stmt}"), None, 10_000).await?;
                let raw: String = rs
                    .rows
                    .iter()
                    .filter_map(|r| r.get(0).and_then(Json::as_str).map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n");
                out.plans.push(plan::explain_json(&stmt, &raw).map_err(Error::Query)?);
            }
            return Ok(());
        }
        let handles = self.run_editor(text, max_rows, out).await?;
        for (stmt, id) in stmts.iter().zip(handles) {
            if plan::classify(stmt) == StmtKind::Other || id.is_empty() {
                continue;
            }
            let q = "SELECT * FROM TABLE(GET_QUERY_OPERATOR_STATS(?)) ORDER BY step_id, operator_id";
            let bindings = json!({ "1": { "type": "TEXT", "value": id } });
            match self.statement(q, Some(bindings), 100_000).await {
                Ok(rs) => {
                    let names: Vec<String> = rs
                        .row_type
                        .iter()
                        .map(|c| c.get("name").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase())
                        .collect();
                    let rows: Vec<serde_json::Map<String, Json>> = rs
                        .rows
                        .iter()
                        .map(|r| names.iter().cloned().zip(r.as_array().cloned().unwrap_or_default()).collect())
                        .collect();
                    match plan::operator_stats(stmt, &id, &rows) {
                        Some(p) => out.plans.push(p),
                        None => out.messages.push(format!("Snowflake no dio estadísticas de operadores para: {}", plan::short(stmt))),
                    }
                }
                Err(e) => out.messages.push(format!("No se pudieron leer las estadísticas de operadores: {e}")),
            }
        }
        Ok(())
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut cache = std::mem::take(&mut self.mon);
        let snap = monitor::snapshot(&*self, &mut cache).await;
        self.mon = cache;
        Ok(snap)
    }

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

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        blocking::blocking(&*self).await
    }

    /// Aborts the transaction (read-only connections are refused by `ReadOnlySession`).
    /// A transaction id (from `blocking`) aborts that transaction; a query
    /// id (from `processes`) ends the session that runs it.
    async fn kill_session(&mut self, id: &str) -> Result<()> {
        if processes::query_id(id).is_some() {
            processes::abort_session_of(&*self, id).await
        } else {
            blocking::kill(&*self, id).await
        }
    }

    /// The queries in flight (`QUERY_HISTORY`), with a running warehouse.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        processes::processes(&*self).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        processes::cancel(&*self, id).await
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
        let (api, handle) = (self.api.clone(), self.handle.clone());
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let Some(h) = handle.lock().ok().and_then(|g| g.clone()) else { return };
            let api = api.clone();
            rt.spawn(async move {
                if let Err(e) = api.post(&format!("/api/v2/statements/{h}/cancel"), &json!({})).await {
                    tracing::debug!("snowflake cancel failed: {e}");
                }
            });
        }))
    }

    /// One query: the system roles in the session and the database's owner (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

fn obj(kind: &str, r: &[Option<String>]) -> DbObject {
    DbObject {
        kind: kind.into(),
        schema: r.first().cloned().flatten(),
        name: r.get(1).cloned().flatten().unwrap_or_default(),
        parent: None,
    }
}

fn column_index(row_type: &[Json], name: &str) -> Option<usize> {
    row_type.iter().position(|c| c.get("name").and_then(Json::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// A `jsonv2` cell (always text) as JSON, by its column's `rowType`.
fn cell(col: &Json, v: &Json) -> Json {
    let Some(s) = v.as_str() else { return v.clone() };
    let ty = col.get("type").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase();
    let scale = col.get("scale").and_then(Json::as_i64).unwrap_or(0);
    match ty.as_str() {
        "fixed" if scale == 0 => s.parse::<i64>().map_or_else(|_| s.into(), json_i64),
        "real" | "float" | "double" => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => json_f64(f),
            _ => s.into(),
        },
        "boolean" => Json::Bool(matches!(s, "true" | "TRUE" | "1")),
        "date" => s
            .parse::<i64>()
            .ok()
            .and_then(|d| chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(d)))
            .map_or_else(|| s.into(), |d| d.format("%Y-%m-%d").to_string().into()),
        "time" => epoch(s)
            .map(|(secs, nanos)| {
                let t = chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs.rem_euclid(86_400) as u32, nanos)
                    .unwrap_or_default();
                t.format("%H:%M:%S%.f").to_string().into()
            })
            .unwrap_or_else(|| s.into()),
        "timestamp_ntz" | "timestamp_ltz" => epoch(s)
            .and_then(|(secs, nanos)| chrono::DateTime::from_timestamp(secs, nanos))
            .map_or_else(|| s.into(), |t| t.format("%Y-%m-%d %H:%M:%S%.f").to_string().into()),
        "timestamp_tz" => timestamp_tz(s).map_or_else(|| s.into(), Json::String),
        "binary" => (0..s.len())
            .step_by(2)
            .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .map_or_else(|| s.into(), |b| json_bytes(&b)),
        "variant" | "object" | "array" => {
            serde_json::from_str::<Json>(s).map_or_else(|_| s.into(), |j| Json::String(j.to_string()))
        }
        _ => s.into(),
    }
}

/// `"1706708700.123000000"` → (seconds, nanoseconds).
fn epoch(s: &str) -> Option<(i64, u32)> {
    let (sec, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = sec.parse().ok()?;
    let nanos: u32 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").get(..9)?.parse().ok()? };
    // Negative epochs with a fraction count the fraction forward.
    if secs < 0 && nanos > 0 && sec.starts_with('-') {
        return Some((secs - 1, 1_000_000_000 - nanos));
    }
    Some((secs, nanos))
}

/// The statements of a script as Snowflake splits it (see `script::units`).
fn split_script(sql: &str) -> Vec<String> {
    script::units(sql).into_iter().map(|u| u.text).collect()
}

/// `"<epoch> <offset minutes + 1440>"` → local time with its offset.
fn timestamp_tz(s: &str) -> Option<String> {
    let (e, off) = s.split_once(' ')?;
    let (secs, nanos) = epoch(e)?;
    let minutes: i32 = off.parse::<i32>().ok()? - 1440;
    let tz = chrono::FixedOffset::east_opt(minutes * 60)?;
    let t = chrono::DateTime::from_timestamp(secs, nanos)?.with_timezone(&tz);
    Some(t.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())
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
            let grant = |grantable| d.schema_grant_script(Some("DB"), "VENTAS", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    /// "Nuevo esquema…" with an owner: created by the current role, the
    /// grants, then the ownership handed to the role (keeping the grants).
    #[test]
    fn schema_owner_goes_after_the_grants() {
        let d = &drivers()[0];
        assert_eq!(d.schema_spec().unwrap().owner_kinds, dbine_driver::SchemaOwnerKinds::Roles);
        assert_eq!(d.create_schema_script(Some("DB"), "ventas", Some("DUENO")).unwrap(), "CREATE SCHEMA \"ventas\";");
        let o = d.schema_owner_script(Some("DB"), "ventas", "DUENO").unwrap().unwrap();
        assert!(o.ends_with("GRANT OWNERSHIP ON SCHEMA \"ventas\" TO ROLE \"DUENO\" COPY CURRENT GRANTS;"), "{o}");
        assert!(d.schema_owner_script(None, "ventas", " ").is_err());
        let g = d.schema_grant_script(Some("DB"), "ventas", &["USAGE".into()], "LECT", false).unwrap();
        assert!(g.ends_with("GRANT USAGE ON SCHEMA \"ventas\" TO ROLE \"LECT\";"), "{g}");
    }

    fn col(t: &str, scale: i64) -> Json {
        json!({ "name": "c", "type": t, "scale": scale })
    }

    #[test]
    fn scripts_split_outside_quotes_and_dollar_blocks() {
        assert_eq!(split_script("SELECT 1; SELECT ';'"), vec!["SELECT 1", "SELECT ';'"]);
        // A backslash-escaped quote doesn't end the string.
        assert_eq!(split_script("SELECT 'a\\';b'; SELECT 2").len(), 2);
        assert_eq!(split_script("SELECT \"a;b\" FROM t; -- x;\nSELECT 2"), vec!["SELECT \"a;b\" FROM t", "SELECT 2"]);
        let block = "EXECUTE IMMEDIATE $$\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND;\n$$";
        assert_eq!(split_script(&format!("{block};\nSELECT 3;")), vec![block, "SELECT 3"]);
    }

    #[test]
    fn accounts_and_urls() {
        assert_eq!(base_url("myorg-acct"), "https://myorg-acct.snowflakecomputing.com");
        assert_eq!(base_url("xy12345.us-east-1"), "https://xy12345.us-east-1.snowflakecomputing.com");
        assert_eq!(base_url("https://a.snowflakecomputing.com/"), "https://a.snowflakecomputing.com");
        assert_eq!(jwt_account("xy12345.us-east-1"), "XY12345");
        assert_eq!(jwt_account("myorg-acct"), "MYORG-ACCT");
    }

    #[test]
    fn cells_by_row_type() {
        assert_eq!(cell(&col("fixed", 0), &json!("42")), json!(42));
        assert_eq!(cell(&col("fixed", 2), &json!("1.50")), json!("1.50"));
        assert_eq!(cell(&col("fixed", 0), &json!("99999999999999999999")), json!("99999999999999999999"));
        assert_eq!(cell(&col("real", 0), &json!("2.5")), json!(2.5));
        assert_eq!(cell(&col("boolean", 0), &json!("true")), json!(true));
        assert_eq!(cell(&col("date", 0), &json!("19753")), json!("2024-01-31"));
        assert_eq!(cell(&col("time", 9), &json!("49500.250000000")), json!("13:45:00.250"));
        assert_eq!(cell(&col("timestamp_ntz", 9), &json!("1706708700.000000000")), json!("2024-01-31 13:45:00"));
        assert_eq!(
            cell(&col("timestamp_tz", 9), &json!("1706708700.000000000 1260")),
            json!("2024-01-31 10:45:00 -03:00")
        );
        assert_eq!(cell(&col("binary", 0), &json!("CAFE")), json!("0xCAFE"));
        assert_eq!(cell(&col("variant", 0), &json!("{\n  \"a\": 1\n}")), json!("{\"a\":1}"));
        assert_eq!(cell(&col("text", 0), &Json::Null), Json::Null);
    }

    #[test]
    fn keypair_issuer_uses_the_public_key_fingerprint() {
        let pem = include_str!("../tests/test_key.pem");
        let fp = public_key_fingerprint(pem).unwrap();
        assert!(fp.starts_with("SHA256:") && fp.len() == 7 + 44, "{fp}");
        let cfg = ConnectionConfig {
            username: Some("jdoe".into()),
            options: [
                ("account".to_string(), "myorg-acct".to_string()),
                ("auth_mode".to_string(), "keypair".to_string()),
                ("private_key".to_string(), pem.to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let Auth::KeyPair { issuer, subject, .. } = auth(&cfg).unwrap() else { panic!() };
        assert_eq!(subject, "MYORG-ACCT.JDOE");
        assert_eq!(issuer, format!("MYORG-ACCT.JDOE.{fp}"));
        let (h, kind) = auth(&cfg).unwrap().headers().unwrap();
        assert!(h.starts_with("Bearer ey") && kind == "KEYPAIR_JWT");
    }

    #[test]
    fn missing_credentials_fail_as_auth() {
        let cfg = ConnectionConfig { options: [("account".to_string(), "a".to_string())].into(), ..Default::default() };
        assert!(matches!(auth(&cfg), Err(Error::AuthFailed(_))));
        assert!(matches!(pem_der("-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA==\n-----END ENCRYPTED PRIVATE KEY-----"), Err(Error::AuthFailed(_))));
    }

    #[test]
    fn column_types() {
        assert_eq!(column_type("TEXT".into(), Some("16".into()), None, None), "TEXT(16)");
        assert_eq!(column_type("NUMBER".into(), None, Some("38".into()), Some("0".into())), "NUMBER(38,0)");
    }
}

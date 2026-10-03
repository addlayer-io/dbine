//! Databricks SQL over the Statement Execution API 2.0
//! (`/api/2.0/sql/statements`) on a SQL warehouse, with a personal access
//! token or OAuth machine-to-machine (service principal). One statement per
//! request, results inline as JSON arrays, following chunks up to
//! `max_rows`. Unity Catalog catalogs are the databases.
//!
//! "Azure Databricks" is the same API on an `*.azuredatabricks.net`
//! workspace; its form also offers Microsoft Entra ID: a token obtained
//! elsewhere (sent like a PAT) or an Entra ID service principal (client
//! credentials against `login.microsoftonline.com`).

mod backup;
mod blocks;
mod ddl;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod script;
mod security;
mod sync;
mod transfer;

use base64::Engine;
use dbine_driver::sql::{quote_ident, select_top, split_statements, Limit, Quote};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo,
    ObjectRef, QueryOutcome, ResultColumn, Result, RowChange, SchemaInfo, Session, TableSchema,
};
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DatabricksDriver { info: info(false) }), Arc::new(DatabricksDriver { info: info(true) })]
}

/// The Azure Databricks resource id: the audience of Entra ID tokens for
/// any Azure Databricks workspace.
const AZURE_DATABRICKS_SCOPE: &str = "2ff814a6-3304-4ab8-85cb-cd0e6f879c1d/.default";
const ENTRA_LOGIN: &str = "https://login.microsoftonline.com";

fn info(azure: bool) -> DriverInfo {
    let mut fields = vec![
        Field::new("host", "Workspace", FieldKind::Text).required().placeholder(if azure {
            "adb-1234567890123456.7.azuredatabricks.net"
        } else {
            "dbc-a1b2c3d4-e5f6.cloud.databricks.com"
        }),
        Field::new("warehouse", "SQL warehouse", FieldKind::Text)
            .required()
            .placeholder("/sql/1.0/warehouses/abcdef1234567890")
            .help("El HTTP path del warehouse o solo su ID."),
    ];
    if azure {
        fields.extend([
            Field::new(
                "auth_mode",
                "Autenticación",
                FieldKind::Select(vec![
                    ("pat", "Token de acceso personal"),
                    ("entra_token", "Token de Microsoft Entra ID"),
                    ("entra_sp", "Service principal de Microsoft Entra ID"),
                    ("oauth_m2m", "OAuth de service principal de Databricks"),
                ]),
            )
            .default_value("pat"),
            Field::new("token", "Token", FieldKind::Password)
                .secret()
                .help("El token de acceso personal, o un token de Entra ID (p. ej. az account get-access-token --resource 2ff814a6-3304-4ab8-85cb-cd0e6f879c1d).")
                .when("auth_mode", &["pat", "entra_token"]),
            Field::new("tenant_id", "Tenant ID (Entra ID)", FieldKind::Text)
                .placeholder("00000000-0000-0000-0000-000000000000")
                .when("auth_mode", &["entra_sp"]),
            Field::new("client_id", "Client ID (service principal)", FieldKind::Text)
                .help("El service principal tiene que estar agregado al workspace.")
                .when("auth_mode", &["entra_sp", "oauth_m2m"]),
            Field::new("client_secret", "Client secret", FieldKind::Password).secret().when("auth_mode", &["entra_sp", "oauth_m2m"]),
        ]);
    } else {
        fields.extend([
            Field::new(
                "auth_mode",
                "Autenticación",
                FieldKind::Select(vec![("pat", "Token de acceso personal"), ("oauth_m2m", "OAuth de service principal")]),
            )
            .default_value("pat"),
            Field::new("token", "Token de acceso personal", FieldKind::Password).secret().when("auth_mode", &["pat"]),
            Field::new("client_id", "Client ID (service principal)", FieldKind::Text).when("auth_mode", &["oauth_m2m"]),
            Field::new("client_secret", "Client secret", FieldKind::Password).secret().when("auth_mode", &["oauth_m2m"]),
        ]);
    }
    fields.extend([
            Field::database().placeholder("(el catálogo predeterminado)"),
            Field::new("schema", "Esquema predeterminado", FieldKind::Text).placeholder("default"),
            Field::read_only(),
    ]);
    DriverInfo {
        id: if azure { "azure_databricks" } else { "databricks" },
        name: if azure { "Azure Databricks" } else { "Databricks SQL" },
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "databricks",
        default_port: 443,
        fields,
        databases_label: "Catálogos",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::materialized_views(),
            ObjectKindInfo::functions(),
        ],
    }
}

pub struct DatabricksDriver {
    info: DriverInfo,
}

type TokenCache = Arc<tokio::sync::Mutex<Option<(String, Instant)>>>;

#[derive(Clone)]
enum Auth {
    /// A personal access token, or any bearer token (Entra ID).
    Pat(String),
    /// Databricks OAuth M2M: the workspace's `/oidc/v1/token`.
    M2m { client_id: String, client_secret: String, cache: TokenCache },
    /// An Entra ID service principal (Azure): client credentials against
    /// `login.microsoftonline.com`.
    EntraSp { tenant: String, client_id: String, client_secret: String, cache: TokenCache },
}

/// The Entra ID token request for a service principal: URL and form.
fn entra_request<'a>(tenant: &str, client_id: &'a str, client_secret: &'a str) -> (String, [(&'static str, &'a str); 4]) {
    (
        format!("{ENTRA_LOGIN}/{}/oauth2/v2.0/token", tenant.trim()),
        [
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("scope", AZURE_DATABRICKS_SCOPE),
        ],
    )
}

/// An OAuth token response: the token and how long to keep it.
fn token_from(ok: bool, body: &Json) -> Result<(String, Duration)> {
    let token = body.get("access_token").and_then(Json::as_str).filter(|_| ok).ok_or_else(|| {
        Error::AuthFailed(body.get("error_description").and_then(Json::as_str).unwrap_or("OAuth rechazado").to_string())
    })?;
    let ttl = body.get("expires_in").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(3600);
    Ok((token.to_string(), Duration::from_secs(ttl.saturating_sub(60).max(30))))
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    base: String,
    auth: Auth,
}

/// `https://host` from a host name or URL.
fn base_url(host: &str) -> String {
    let h = host.trim().trim_end_matches('/');
    if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        format!("https://{h}")
    }
}

/// `/sql/1.0/warehouses/<id>` (or `…/endpoints/<id>`) → `<id>`.
fn warehouse_id(v: &str) -> String {
    let v = v.trim().trim_end_matches('/');
    v.rsplit('/').next().unwrap_or(v).to_string()
}

impl Api {
    async fn bearer(&self) -> Result<String> {
        let (req, cache) = match &self.auth {
            Auth::Pat(t) => return Ok(format!("Bearer {t}")),
            Auth::M2m { client_id, client_secret, cache } => (
                self.http
                    .post(format!("{}/oidc/v1/token", self.base))
                    .basic_auth(client_id, Some(client_secret))
                    .form(&[("grant_type", "client_credentials"), ("scope", "all-apis")]),
                cache,
            ),
            Auth::EntraSp { tenant, client_id, client_secret, cache } => {
                let (url, form) = entra_request(tenant, client_id, client_secret);
                (self.http.post(url).form(&form), cache)
            }
        };
        let mut c = cache.lock().await;
        if let Some((t, until)) = c.as_ref() {
            if Instant::now() < *until {
                return Ok(format!("Bearer {t}"));
            }
        }
        let resp = req.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let ok = resp.status().is_success();
        let body: Json = resp.json().await.map_err(|e| Error::AuthFailed(e.to_string()))?;
        let (token, ttl) = token_from(ok, &body)?;
        *c = Some((token.clone(), Instant::now() + ttl));
        Ok(format!("Bearer {token}"))
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Json> {
        let resp = req.header("Authorization", self.bearer().await?).send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        let body: Json = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }));
        match status {
            200..=299 => Ok(body),
            401 | 403 => Err(Error::AuthFailed(api_message(&body, status))),
            _ => Err(Error::Query(api_message(&body, status))),
        }
    }

    async fn post(&self, path: &str, body: &Json) -> Result<Json> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body)).await
    }

    async fn get(&self, path: &str) -> Result<Json> {
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }
}

fn api_message(body: &Json, status: u16) -> String {
    body.get("message")
        .and_then(Json::as_str)
        .filter(|m| !m.is_empty())
        .map_or_else(|| format!("HTTP {status}"), str::to_string)
}

pub struct DatabricksSession {
    api: Api,
    warehouse: String,
    catalog: Option<String>,
    schema: Option<String>,
    running: Arc<Mutex<Option<String>>>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

#[derive(Default, Debug)]
struct Statement {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Json>>,
    total: u64,
    more: bool,
}

#[async_trait]
impl Driver for DatabricksDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        script::dialect()
    }

    /// SQL scripting's compound `BEGIN … END` blocks go whole.
    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        script::units(text)
    }

    /// One statement per request, as the API takes them; `USE` carries
    /// over in the session's catalog and schema.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Catalogs are the databases: `CREATE CATALOG` / `DROP CATALOG … CASCADE`.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Batched `INSERT … VALUES` with typed literals, see `transfer.rs`.
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    /// No indexes, but Unity Catalog's foreign keys for the explorer (see
    /// `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn supports_schema_sync(&self) -> bool {
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

    /// Schemas of the session's catalog (Unity Catalog).
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec())
    }

    /// Never with an owner: it's handed over after the grants
    /// (`schema_owner_script`).
    fn create_schema_script(&self, _database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(name)
    }

    /// `ALTER SCHEMA … OWNER TO`, after the grants: once the schema is
    /// someone else's, the creator can't grant on it without MANAGE.
    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        security::schema_owner(name, owner).map(Some)
    }

    /// "Con opción de otorgar" is MANAGE on the schema (Unity Catalog has
    /// no WITH GRANT OPTION).
    fn schema_grant_script(&self, _database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
        security::schema_grant(name, privileges, to, grantable)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        security::drop_schema(name, cascade)
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
        if cfg.host.trim().is_empty() {
            return Err(Error::Connect("falta el host del workspace".into()));
        }
        let warehouse = cfg.option("warehouse").map(warehouse_id).ok_or_else(|| Error::Connect("falta el SQL warehouse".into()))?;
        let auth = match cfg.option("auth_mode").unwrap_or("pat") {
            "oauth_m2m" => match (cfg.option("client_id"), cfg.option("client_secret")) {
                (Some(id), Some(secret)) => Auth::M2m {
                    client_id: id.trim().into(),
                    client_secret: secret.trim().into(),
                    cache: Arc::new(tokio::sync::Mutex::new(None)),
                },
                _ => return Err(Error::AuthFailed("faltan el client ID o el client secret".into())),
            },
            "entra_sp" => match (cfg.option("tenant_id"), cfg.option("client_id"), cfg.option("client_secret")) {
                (Some(tenant), Some(id), Some(secret)) => Auth::EntraSp {
                    tenant: tenant.trim().into(),
                    client_id: id.trim().into(),
                    client_secret: secret.trim().into(),
                    cache: Arc::new(tokio::sync::Mutex::new(None)),
                },
                _ => return Err(Error::AuthFailed("faltan el tenant ID, el client ID o el client secret".into())),
            },
            _ => Auth::Pat(
                cfg.option("token")
                    .or(cfg.password.as_deref().filter(|p| !p.is_empty()))
                    .ok_or_else(|| Error::AuthFailed("falta el token de acceso".into()))?
                    .trim()
                    .into(),
            ),
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .user_agent("DBine")
            .build()
            .map_err(|e| Error::Connect(e.to_string()))?;
        let s = DatabricksSession {
            api: Api { http, base: base_url(&cfg.host), auth },
            warehouse,
            catalog: database.or(Some(cfg.database.as_str())).map(str::trim).filter(|d| !d.is_empty()).map(Into::into),
            schema: cfg.option("schema").map(|s| s.trim().to_string()),
            running: Arc::new(Mutex::new(None)),
            profiler: None,
        };
        // Also wakes a stopped warehouse up.
        tokio::time::timeout(Duration::from_secs(300), s.run("SELECT 1", 1, None))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado (¿el warehouse está iniciando?)".into()))?
            .map_err(|e| match e {
                Error::Query(m) => Error::Connect(m),
                other => other,
            })?;
        Ok(Box::new(s))
    }
}

impl DatabricksSession {
    fn set_running(&self, id: Option<String>) {
        if let Ok(mut r) = self.running.lock() {
            *r = id;
        }
    }

    fn request(&self, sql: &str, params: Option<Json>) -> Json {
        let mut b = json!({
            "statement": sql,
            "warehouse_id": self.warehouse,
            "wait_timeout": "30s",
            "on_wait_timeout": "CONTINUE",
            "disposition": "INLINE",
            "format": "JSON_ARRAY",
        });
        if let Some(c) = &self.catalog {
            b["catalog"] = json!(c);
        }
        if let Some(s) = &self.schema {
            b["schema"] = json!(s);
        }
        if let Some(p) = params {
            b["parameters"] = p;
        }
        b
    }

    async fn run(&self, sql: &str, max_rows: usize, params: Option<Json>) -> Result<Statement> {
        self.run_id(sql, max_rows, params).await.map(|(_, st)| st)
    }

    /// Runs one statement; also gives its statement id.
    async fn run_id(&self, sql: &str, max_rows: usize, params: Option<Json>) -> Result<(String, Statement)> {
        let mut resp = self.api.post("/api/2.0/sql/statements", &self.request(sql, params)).await?;
        let id = resp.get("statement_id").and_then(Json::as_str).unwrap_or_default().to_string();
        self.set_running(Some(id.clone()));
        let result = self.finish(&id, &mut resp, max_rows).await;
        self.set_running(None);
        result.map(|st| (id, st))
    }

    fn push_statement(st: &Statement, max_rows: usize, out: &mut QueryOutcome) {
        if st.columns.is_empty() {
            out.push_affected(0);
            return;
        }
        out.begin_result(st.columns.iter().map(|(n, t)| ResultColumn { name: n.clone(), type_name: t.clone() }).collect());
        for r in &st.rows {
            out.push_row(
                st.columns.iter().enumerate().map(|(i, (_, t))| cell(t, r.get(i).unwrap_or(&Json::Null))).collect(),
                max_rows,
            );
        }
        if let Some(last) = out.results.last_mut() {
            last.total_rows = last.total_rows.max(st.total);
            last.truncated |= st.more || st.total > last.rows.len() as u64;
        }
    }

    /// `EXPLAIN FORMATTED`: planned, not run.
    async fn estimated_plan(&self, stmt: &str) -> Result<dbine_driver::Plan> {
        let st = self.run(&format!("EXPLAIN FORMATTED {stmt}"), 10_000, None).await?;
        let raw: Vec<String> = st.rows.iter().filter_map(|r| r.first().and_then(Json::as_str).map(str::to_string)).collect();
        let raw = raw.join("\n");
        if raw.contains("Error occurred during query planning") {
            return Err(Error::Query(raw));
        }
        Ok(plan::formatted(stmt, &raw))
    }

    /// The query history entry of a finished statement, with its metrics.
    /// The history lags a little behind, so it's asked a few times.
    async fn history(&self, statement_id: &str) -> Result<Option<Json>> {
        let path = format!("/api/2.0/sql/history/queries?filter_by.statement_ids={statement_id}&include_metrics=true");
        for wait in [0u64, 1000, 2000, 3000] {
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            let r = self.api.get(&path).await?;
            if let Some(q) = r.get("res").and_then(Json::as_array).and_then(|a| a.first()) {
                if q.get("metrics").is_some() {
                    return Ok(Some(q.clone()));
                }
            }
        }
        Ok(None)
    }

    async fn finish(&self, id: &str, resp: &mut Json, max_rows: usize) -> Result<Statement> {
        let mut delay = Duration::from_millis(250);
        loop {
            match resp.pointer("/status/state").and_then(Json::as_str) {
                Some("SUCCEEDED") => break,
                Some("FAILED") => {
                    let msg = resp.pointer("/status/error/message").and_then(Json::as_str).unwrap_or("la sentencia falló");
                    return Err(Error::Query(msg.to_string()));
                }
                Some("CANCELED") | Some("CLOSED") => return Err(Error::Cancelled),
                _ => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                    *resp = self.api.get(&format!("/api/2.0/sql/statements/{id}")).await?;
                }
            }
        }
        let mut st = Statement {
            columns: resp
                .pointer("/manifest/schema/columns")
                .and_then(Json::as_array)
                .map(|cols| {
                    cols.iter()
                        .map(|c| {
                            let name = c.get("name").and_then(Json::as_str).unwrap_or("").to_string();
                            let ty = c.get("type_name").and_then(Json::as_str).unwrap_or("").to_string();
                            (name, ty)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            total: resp.pointer("/manifest/total_row_count").and_then(Json::as_u64).unwrap_or(0),
            more: resp.pointer("/manifest/truncated").and_then(Json::as_bool).unwrap_or(false),
            ..Default::default()
        };
        let mut chunk = resp.get("result").cloned().unwrap_or(Json::Null);
        loop {
            for row in chunk.get("data_array").and_then(Json::as_array).into_iter().flatten() {
                if st.rows.len() < max_rows {
                    st.rows.push(row.as_array().cloned().unwrap_or_default());
                } else {
                    st.more = true;
                }
            }
            match chunk.get("next_chunk_index").and_then(Json::as_u64) {
                Some(n) if st.rows.len() < max_rows => {
                    chunk = self.api.get(&format!("/api/2.0/sql/statements/{id}/result/chunks/{n}")).await?;
                }
                Some(_) => {
                    st.more = true;
                    break;
                }
                None => break,
            }
        }
        st.total = st.total.max(st.rows.len() as u64);
        Ok(st)
    }

    /// Text rows of a catalog query with named `:p0, :p1…` parameters.
    async fn text_rows(&self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
        let params = (!args.is_empty()).then(|| {
            Json::Array(args.iter().enumerate().map(|(i, a)| json!({ "name": format!("p{i}"), "value": a })).collect())
        });
        let st = self.run(sql, 100_000, params).await?;
        Ok(st.rows.into_iter().map(|r| r.into_iter().map(|v| v.as_str().map(str::to_string)).collect()).collect())
    }

    /// Rows of a catalog query by lowercase column name (NULLs left out).
    async fn named_rows(&self, sql: &str) -> Result<Vec<ddl::Row>> {
        let st = self.run(sql, 1_000_000, None).await?;
        let names: Vec<String> = st.columns.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        Ok(st
            .rows
            .iter()
            .map(|r| names.iter().zip(r).filter_map(|(n, v)| Some((n.clone(), v.as_str()?.to_string()))).collect())
            .collect())
    }

    fn catalog(&self) -> Result<&str> {
        self.catalog.as_deref().ok_or_else(|| Error::Query("no hay un catálogo seleccionado".into()))
    }

    fn fq(&self, o: &ObjectRef) -> Result<String> {
        let cat = quote_ident(Quote::Backtick, self.catalog()?);
        let schema = o.schema().or(self.schema.as_deref()).unwrap_or("default");
        Ok(format!("{cat}.{}.{}", quote_ident(Quote::Backtick, schema), quote_ident(Quote::Backtick, &o.name)))
    }
}

#[async_trait]
impl Session for DatabricksSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.text_rows("SELECT current_version().dbsql_version", &[]).await.unwrap_or_default();
        let v = rows.first().and_then(|r| r.first().cloned().flatten());
        Ok(match v {
            Some(v) => format!("Databricks SQL {v}"),
            None => "Databricks SQL".into(),
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.text_rows("SHOW CATALOGS", &[]).await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Ok(cat) = self.catalog().map(|c| quote_ident(Quote::Backtick, c)) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        let tables = self
            .text_rows(
                &format!(
                    "SELECT table_schema, table_name, table_type FROM {cat}.information_schema.tables
                     WHERE table_schema <> 'information_schema' ORDER BY 1, 2"
                ),
                &[],
            )
            .await?;
        for r in tables {
            let kind = match r.get(2).cloned().flatten().as_deref() {
                Some("VIEW") => kinds::VIEW,
                Some("MATERIALIZED_VIEW") => kinds::MATERIALIZED_VIEW,
                _ => kinds::TABLE,
            };
            out.push(DbObject {
                kind: kind.into(),
                schema: r.first().cloned().flatten(),
                name: r.get(1).cloned().flatten().unwrap_or_default(),
                parent: None,
            });
        }
        if let Ok(rows) = self
            .text_rows(
                &format!(
                    "SELECT routine_schema, routine_name FROM {cat}.information_schema.routines
                     WHERE routine_schema <> 'information_schema' ORDER BY 1, 2"
                ),
                &[],
            )
            .await
        {
            out.extend(rows.into_iter().map(|r| DbObject {
                kind: kinds::FUNCTION.into(),
                schema: r.first().cloned().flatten(),
                name: r.get(1).cloned().flatten().unwrap_or_default(),
                parent: None,
            }));
        }
        Ok(out)
    }

    /// The catalog's `information_schema.schemata` (what the user can
    /// see); information_schema is the system one.
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let Ok(cat) = self.catalog().map(|c| quote_ident(Quote::Backtick, c)) else { return Ok(None) };
        let rows = self.text_rows(&format!("SELECT schema_name FROM {cat}.information_schema.schemata ORDER BY 1"), &[]).await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .map(|name| SchemaInfo { system: name.eq_ignore_ascii_case("information_schema"), name })
                .collect(),
        ))
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let cat = quote_ident(Quote::Backtick, self.catalog()?);
        let schema = o.schema().or(self.schema.as_deref()).unwrap_or("default").to_string();
        let rows = self
            .text_rows(
                &format!(
                    "SELECT c.column_name, c.full_data_type, c.is_nullable, c.column_default,
                            EXISTS (SELECT 1 FROM {cat}.information_schema.key_column_usage k
                                    JOIN {cat}.information_schema.table_constraints t
                                      ON t.constraint_name = k.constraint_name AND t.constraint_schema = k.constraint_schema
                                    WHERE t.constraint_type = 'PRIMARY KEY' AND k.table_schema = c.table_schema
                                      AND k.table_name = c.table_name AND k.column_name = c.column_name)
                     FROM {cat}.information_schema.columns c
                     WHERE c.table_schema = :p0 AND c.table_name = :p1 ORDER BY c.ordinal_position"
                ),
                &[&schema, &o.name],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let s = |i: usize| r.get(i).cloned().flatten();
                ColumnInfo {
                    name: s(0).unwrap_or_default(),
                    data_type: s(1).unwrap_or_default(),
                    nullable: s(2).as_deref() != Some("NO"),
                    default_value: s(3),
                    primary_key: s(4).as_deref() == Some("true"),
                    auto_increment: false,
                }
            })
            .collect())
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
        if o.kind == kinds::FUNCTION {
            let cat = quote_ident(Quote::Backtick, self.catalog()?);
            let schema = o.schema().or(self.schema.as_deref()).unwrap_or("default").to_string();
            let rows = self
                .text_rows(
                    &format!(
                        "SELECT routine_definition FROM {cat}.information_schema.routines
                         WHERE routine_schema = :p0 AND routine_name = :p1"
                    ),
                    &[&schema, &o.name],
                )
                .await?;
            return Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()));
        }
        let rows = self.text_rows(&format!("SHOW CREATE TABLE {}", self.fq(o)?), &[]).await?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
    }

    /// The catalog's information_schema: tables, columns, primary and
    /// foreign keys (informational in Unity Catalog), four queries in all.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let Ok(cat) = self.catalog().map(|c| quote_ident(Quote::Backtick, c)) else { return Ok(Vec::new()) };
        let is = format!("{cat}.information_schema");
        let tables = self
            .named_rows(&format!(
                "SELECT table_schema, table_name, comment, data_source_format FROM {is}.tables
                 WHERE table_schema <> 'information_schema' AND table_type IN ('MANAGED', 'EXTERNAL')"
            ))
            .await?;
        let columns = self
            .named_rows(&format!(
                "SELECT table_schema, table_name, column_name, ordinal_position, full_data_type, is_nullable, column_default,
                        is_identity, identity_generation, comment, partition_index
                 FROM {is}.columns WHERE table_schema <> 'information_schema'"
            ))
            .await?;
        let pks = self
            .named_rows(&format!(
                "SELECT k.table_schema, k.table_name, k.constraint_name, k.column_name, k.ordinal_position
                 FROM {is}.table_constraints t
                 JOIN {is}.key_column_usage k ON k.constraint_schema = t.constraint_schema AND k.constraint_name = t.constraint_name
                 WHERE t.constraint_type = 'PRIMARY KEY'"
            ))
            .await?;
        let fks = self
            .named_rows(&format!(
                "SELECT k.table_schema, k.table_name, k.constraint_name, k.column_name, k.ordinal_position,
                        u.table_schema AS ref_schema, u.table_name AS ref_table, u.column_name AS ref_column,
                        r.update_rule, r.delete_rule
                 FROM {is}.referential_constraints r
                 JOIN {is}.key_column_usage k ON k.constraint_schema = r.constraint_schema AND k.constraint_name = r.constraint_name
                 JOIN {is}.key_column_usage u ON u.constraint_schema = r.unique_constraint_schema
                  AND u.constraint_name = r.unique_constraint_name AND u.ordinal_position = k.position_in_unique_constraint"
            ))
            .await?;
        let mut out = ddl::assemble(&tables, &columns, &pks, &fks);
        // Delta keeps CHECK constraints as table properties (the
        // information_schema's check_constraints is empty): one query per
        // Delta table.
        let delta: Vec<(String, String)> = tables
            .iter()
            .filter(|r| r.get("data_source_format").is_none_or(|f| f.eq_ignore_ascii_case("DELTA")))
            .filter_map(|r| Some((r.get("table_schema")?.clone(), r.get("table_name")?.clone())))
            .collect();
        for (schema, name) in delta {
            let fq = format!("{cat}.{}.{}", quote_ident(Quote::Backtick, &schema), quote_ident(Quote::Backtick, &name));
            let Ok(rows) = self.text_rows(&format!("SHOW TBLPROPERTIES {fq}"), &[]).await else { continue };
            let props: Vec<(String, String)> = rows
                .into_iter()
                .filter_map(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten().unwrap_or_default())))
                .collect();
            if let Some(t) = out.iter_mut().find(|t| t.schema.as_deref() == Some(schema.as_str()) && t.name == name) {
                t.checks = ddl::checks_from_properties(&props);
            }
        }
        Ok(out)
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

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    /// The warehouse's running and queued queries (its query history): a
    /// SQL warehouse has no sessions to list.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.processes_list().await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_running(id).await
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

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.run(&format!("CREATE CATALOG {}", quote_ident(Quote::Backtick, name)), 1, None).await.map(|_| ())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.catalog.as_deref() == Some(name) {
            return Err(Error::Query(format!("no se puede borrar el catálogo «{name}»: es el de la sesión actual")));
        }
        self.run(&format!("DROP CATALOG {} CASCADE", quote_ident(Quote::Backtick, name)), 1, None).await.map(|_| ())
    }

    fn browse_query(&self, o: &ObjectRef, limit: u32) -> String {
        match self.fq(o) {
            Ok(fq) => format!("SELECT *\nFROM {fq}\nLIMIT {limit}"),
            Err(_) => select_top(Quote::Backtick, Limit::Limit, o.schema(), &o.name, limit),
        }
    }

    /// Each statement as its own request. The API keeps no session, so a
    /// `USE` / `SET CATALOG` that ran becomes the catalog and schema sent
    /// with the next ones.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for unit in script::units(text) {
            let st = match self.run(&unit.text, max_rows, None).await {
                Ok(st) => st,
                Err(Error::Query(m)) => return Err(script::shift(script::error(&m, &unit.text).into(), &unit)),
                Err(e) => return Err(e),
            };
            Self::push_statement(&st, max_rows, out);
            if let Some(n) = st.columns.iter().position(|(c, _)| c == "num_affected_rows") {
                let n = st.rows.first().and_then(|r| r.get(n)).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
                if let Some(last) = out.results.last_mut() {
                    last.rows_affected = n;
                }
            }
            if let Some(u) = script::use_target(&unit.text) {
                let before = self.catalog.clone();
                match u {
                    script::Use::Catalog(c) => {
                        // A new catalog starts at its default schema.
                        self.catalog = Some(c);
                        self.schema = None;
                    }
                    script::Use::Schema(s) => self.schema = Some(s),
                    script::Use::Both(c, s) => {
                        self.catalog = Some(c);
                        self.schema = Some(s);
                    }
                }
                let shown = [self.catalog.as_deref(), self.schema.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(".");
                out.info(format!("Contexto: {shown}"));
                if self.catalog != before {
                    // Catalogs are the databases: the tab follows the USE.
                    out.database = self.catalog.clone();
                }
            }
        }
        Ok(())
    }

    /// Estimated: `EXPLAIN FORMATTED` (the physical plan; Spark gives no
    /// costs there). Actual: each statement is planned the same way and
    /// then runs once; its totals (time, rows, bytes, spill) come from the
    /// query history API and go on the plan's root. Per-operator actuals
    /// only exist in the Spark UI, which this API doesn't reach.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        use plan::StmtKind;
        for stmt in split_statements(text) {
            let plannable = plan::classify(&stmt) == StmtKind::Plannable;
            if !analyze {
                if plannable {
                    out.plans.push(self.estimated_plan(&stmt).await?);
                } else {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                }
                continue;
            }
            let estimated = if plannable { Some(self.estimated_plan(&stmt).await?) } else { None };
            let (id, st) = self.run_id(&stmt, max_rows, None).await?;
            Self::push_statement(&st, max_rows, out);
            let Some(mut p) = estimated else { continue };
            match self.history(&id).await {
                Ok(Some(q)) => plan::add_history(&mut p, &q),
                Ok(None) => out.messages.push(format!(
                    "El historial de consultas todavía no tiene las métricas de: {} (se muestra el plan estimado)",
                    plan::short(&stmt)
                )),
                Err(e) => out.messages.push(format!("No se pudieron leer las métricas del historial de consultas: {e}")),
            }
            out.plans.push(p);
        }
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

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (api, running) = (self.api.clone(), self.running.clone());
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let Some(id) = running.lock().ok().and_then(|r| r.clone()) else { return };
            let api = api.clone();
            rt.spawn(async move {
                if let Err(e) = api.post(&format!("/api/2.0/sql/statements/{id}/cancel"), &json!({})).await {
                    tracing::debug!("databricks cancel failed: {e}");
                }
            });
        }))
    }

    /// Workspace admin, from SCIM `Me` (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::check(self).await)
    }
}

/// A JSON_ARRAY cell (text) by its column's `type_name`.
fn cell(ty: &str, v: &Json) -> Json {
    let Some(s) = v.as_str() else { return v.clone() };
    match ty {
        "BYTE" | "SHORT" | "INT" | "LONG" => s.parse::<i64>().map_or_else(|_| s.into(), json_i64),
        "FLOAT" | "DOUBLE" => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => json_f64(f),
            _ => s.into(),
        },
        "BOOLEAN" => Json::Bool(s == "true"),
        "TIMESTAMP" | "TIMESTAMP_NTZ" => s.replacen('T', " ", 1).trim_end_matches('Z').to_string().into(),
        "BINARY" => base64::engine::general_purpose::STANDARD.decode(s).map_or_else(|_| s.into(), |b| json_bytes(&b)),
        "ARRAY" | "MAP" | "STRUCT" => serde_json::from_str::<Json>(s).map_or_else(|_| s.into(), |j| Json::String(j.to_string())),
        _ => s.into(),
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
            let grant = |grantable| d.schema_grant_script(Some("main"), "ventas", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    /// "Nuevo esquema…" with an owner: created by the user, the grants
    /// ("con opción de otorgar" as MANAGE), then the owner change.
    #[test]
    fn schema_owner_goes_after_the_grants() {
        let d = &drivers()[0];
        assert_eq!(d.create_schema_script(Some("main"), "ventas", Some("ana@x.com")).unwrap(), "CREATE SCHEMA `ventas`;");
        assert_eq!(d.schema_owner_script(Some("main"), "ventas", "ana@x.com").unwrap().as_deref(), Some("ALTER SCHEMA `ventas` OWNER TO `ana@x.com`;"));
        assert!(d.schema_owner_script(None, "ventas", "").is_err());
        let g = d.schema_grant_script(Some("main"), "ventas", &["USE SCHEMA".into()], "grupo", true).unwrap();
        assert!(g.ends_with("\nGRANT USE SCHEMA, MANAGE ON SCHEMA `ventas` TO `grupo`;"), "{g}");
        let g = d.schema_grant_script(None, "ventas", &["SELECT".into()], "grupo", false).unwrap();
        assert_eq!(g, "GRANT SELECT ON SCHEMA `ventas` TO `grupo`;");
    }

    #[test]
    fn hosts_and_warehouses() {
        assert_eq!(base_url("adb-1.2.azuredatabricks.net/"), "https://adb-1.2.azuredatabricks.net");
        assert_eq!(base_url("https://x.cloud.databricks.com"), "https://x.cloud.databricks.com");
        assert_eq!(warehouse_id("/sql/1.0/warehouses/abc123"), "abc123");
        assert_eq!(warehouse_id("abc123"), "abc123");
    }

    #[test]
    fn azure_preset_and_entra_tokens() {
        let d = drivers();
        let ids: Vec<&str> = d.iter().map(|d| d.info().id).collect();
        assert_eq!(ids, vec!["databricks", "azure_databricks"]);
        let az = info(true);
        assert_eq!(az.name, "Azure Databricks");
        assert!(az.fields.iter().any(|f| f.key == "tenant_id"));
        assert!(!info(false).fields.iter().any(|f| f.key == "tenant_id"));
        assert!(d.iter().all(|d| d.capabilities().monitor));
        let (url, form) = entra_request(" t1 ", "cid", "sec");
        assert_eq!(url, "https://login.microsoftonline.com/t1/oauth2/v2.0/token");
        assert!(form.contains(&("scope", "2ff814a6-3304-4ab8-85cb-cd0e6f879c1d/.default")));
        assert!(form.contains(&("grant_type", "client_credentials")) && form.contains(&("client_id", "cid")));
        // Entra ID answers expires_in as a number (v2) or a string (v1).
        let (t, ttl) = token_from(true, &json!({"access_token": "abc", "expires_in": "3599"})).unwrap();
        assert_eq!((t.as_str(), ttl.as_secs()), ("abc", 3539));
        let e = token_from(false, &json!({"error_description": "AADSTS7000215: Invalid client secret"})).unwrap_err();
        assert!(matches!(e, Error::AuthFailed(m) if m.starts_with("AADSTS")));
    }

    #[tokio::test]
    async fn entra_sp_needs_its_three_fields() {
        let mut cfg = ConnectionConfig { driver: "azure_databricks".into(), host: "adb-1.2.azuredatabricks.net".into(), ..Default::default() };
        cfg.options.insert("warehouse".into(), "abc".into());
        cfg.options.insert("auth_mode".into(), "entra_sp".into());
        cfg.options.insert("client_id".into(), "cid".into());
        let e = drivers().pop().unwrap().connect(&cfg, None).await.err().unwrap();
        assert!(matches!(e, Error::AuthFailed(m) if m.contains("tenant")));
    }

    #[test]
    fn cells_by_type() {
        assert_eq!(cell("LONG", &json!("9007199254740993")), json!("9007199254740993"));
        assert_eq!(cell("INT", &json!("7")), json!(7));
        assert_eq!(cell("DOUBLE", &json!("0.25")), json!(0.25));
        assert_eq!(cell("BOOLEAN", &json!("true")), json!(true));
        assert_eq!(cell("DECIMAL", &json!("1.10")), json!("1.10"));
        assert_eq!(cell("TIMESTAMP", &json!("2024-01-31T13:45:00.000Z")), json!("2024-01-31 13:45:00.000"));
        assert_eq!(cell("BINARY", &json!("yv4=")), json!("0xCAFE"));
        assert_eq!(cell("STRUCT", &json!("{\"a\": 1}")), json!("{\"a\":1}"));
        assert_eq!(cell("STRING", &Json::Null), Json::Null);
    }

    #[test]
    fn statement_parsing_follows_the_manifest() {
        // Recorded shape of a SUCCEEDED response.
        let resp = json!({
            "statement_id": "01ef", "status": { "state": "SUCCEEDED" },
            "manifest": { "schema": { "columns": [ { "name": "id", "type_name": "INT" }, { "name": "s", "type_name": "STRING" } ] },
                          "total_row_count": 2, "truncated": false },
            "result": { "chunk_index": 0, "data_array": [["1", "a"], ["2", null]] }
        });
        let cols: Vec<_> = resp.pointer("/manifest/schema/columns").unwrap().as_array().unwrap().iter()
            .map(|c| (c["name"].as_str().unwrap(), c["type_name"].as_str().unwrap())).collect();
        assert_eq!(cols, vec![("id", "INT"), ("s", "STRING")]);
        let rows = resp.pointer("/result/data_array").unwrap().as_array().unwrap();
        assert_eq!(cell(cols[0].1, &rows[1][0]), json!(2));
        assert_eq!(cell(cols[1].1, &rows[1][1]), Json::Null);
    }
}

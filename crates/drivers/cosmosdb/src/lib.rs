//! Azure Cosmos DB for NoSQL over its REST API, with master-key auth.
//!
//! # Query language (`Language::Sql`, dialect `cosmos`)
//!
//! Cosmos SQL (`SELECT … FROM c WHERE …`) runs against **one container**,
//! which the query text must name. In order of precedence:
//!
//! 1. A directive comment: `-- container: <name>` (applies to the
//!    statements after it). Works on read-only connections too.
//! 2. `USE <name>;` (also `USE "name"`), which stays for the session's
//!    later runs. Read-only connections refuse `USE` (the shared SQL
//!    read-only guard only lets reads through): use the directive there.
//! 3. The `FROM` source, when it's the name of a container of the database:
//!    `SELECT * FROM products p WHERE p.price > 10`.
//! 4. The only container of the database, or the last one used.
//!
//! Statements are separated by `;`. Every query is cross-partition; pages
//! are followed through `x-ms-continuation` until `max_rows + 1` items.
//! When the gateway refuses a cross-partition query it can't serve alone
//! (ORDER BY / aggregates over several physical partitions), the driver
//! runs it once per partition key range and says so in the messages:
//! those results are per range, not merged.
//!
//! # Results
//!
//! One row per item; columns are the union of top-level keys, `id` first
//! and the system properties (`_rid`, `_self`, `_etag`, `_attachments`,
//! `_ts`) last; nested values are compact JSON. `SELECT VALUE` scalars go
//! in a `value` column. The request charge (RU) goes to the messages.
//!
//! # Administration and writes
//!
//! Cosmos SQL only queries, so `execute` also takes `CREATE CONTAINER`,
//! `DROP CONTAINER`, `INSERT INTO`, `UPSERT INTO`, `UPDATE` and `DELETE` with JSON bodies
//! (see [`ddl`]); the designer and the script generator write them. Also
//! `CREATE USER`, `DROP USER`, `GRANT ALL|READ ON "c" TO "u"` and `REVOKE`,
//! for the database's resource-token users (see `security`).
//!
//! The Cosmos DB API for MongoDB is served by the `mongodb` driver (with
//! the account's connection string).

pub mod ddl;
mod sync;
mod monitor;
mod permissions;
mod plan;
mod security;
mod transfer;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use dbine_driver::{
    async_trait, json_f64, json_i64, json_u64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig,
    CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, IndexDef,
    KeyDef, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use ddl::Admin;
use std::collections::{BTreeMap, HashMap};
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Syntax help for the editor.
pub const QUERY_HELP: &str = "SQL de Cosmos DB sobre un contenedor. Indicá el contenedor con una directiva:\n\
-- container: productos\n\
SELECT TOP 10 * FROM c WHERE c.precio > 100 ORDER BY c.precio DESC\n\
o con USE productos; o nombrándolo en el FROM: SELECT p.nombre FROM productos p.\n\
Sentencias separadas con «;». Las consultas son entre particiones.\n\
\n\
Además, DBine entiende estas sentencias (el JSON va tal cual lo toma la API REST):\n\
CREATE CONTAINER [IF NOT EXISTS] \"productos\" {\n\
  \"partitionKey\": { \"paths\": [\"/categoria\"], \"kind\": \"Hash\" },\n\
  \"indexingPolicy\": {…}, \"uniqueKeyPolicy\": { \"uniqueKeys\": [{ \"paths\": [\"/sku\"] }] },\n\
  \"defaultTtl\": 3600, \"throughput\": 400 }   (o \"autoscaleMaxThroughput\": 4000)\n\
DROP CONTAINER [IF EXISTS] \"productos\"\n\
INSERT INTO \"productos\" { \"id\": \"1\", \"categoria\": \"a\", \"precio\": 10 }\n\
UPSERT INTO \"productos\" { … }   (inserta o reemplaza por id y clave de partición)\n\
UPDATE \"productos\" SET { \"precio\": 12 } WHERE { \"id\": \"1\" }   (cambia esos campos del documento)\n\
DELETE FROM \"productos\" WHERE { \"id\": \"1\" }   (borra ese documento)\n\
CREATE USER \"ana\" · DROP USER \"ana\"   (usuarios de la base, con tokens de recurso)\n\
GRANT ALL|READ ON \"productos\" TO \"ana\" · REVOKE ALL|READ ON \"productos\" FROM \"ana\"";

const API_VERSION: &str = "2018-12-31";
const TIMEOUT: Duration = Duration::from_secs(15);
const SYSTEM_PROPS: &[&str] = &["_rid", "_self", "_etag", "_attachments", "_ts"];

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(CosmosDriver)]
}

pub struct CosmosDriver;

fn info() -> &'static DriverInfo {
    static INFO: OnceLock<DriverInfo> = OnceLock::new();
    INFO.get_or_init(|| DriverInfo {
        id: "cosmosdb",
        name: "Azure Cosmos DB",
        family: Family::Document,
        language: Language::Sql,
        dialect: "cosmos",
        default_port: 443,
        fields: vec![
            Field::new("host", "Endpoint", FieldKind::Text)
                .required()
                .placeholder("https://cuenta.documents.azure.com:443/")
                .help(
                    "API NoSQL. Para la API de MongoDB de Cosmos DB usá el driver MongoDB con la cadena de \
                     conexión de la cuenta.",
                ),
            Field::new("account_key", "Clave de la cuenta", FieldKind::Password).required().secret(),
            Field::database(),
            Field::trust_cert().help("Necesario para el emulador local (certificado autofirmado)."),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::COLLECTION, "Contenedores", true, true, true)],
    })
}

/// What `encodeURIComponent` leaves alone.
const URI_COMPONENT: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'!').remove(b'~').remove(b'*').remove(b'\'').remove(b'(').remove(b')');

fn enc(s: &str) -> String {
    utf8_percent_encode(s, URI_COMPONENT).to_string()
}

/// The `Authorization` header for a master key, per "Access control on
/// Azure Cosmos DB resources": HMAC-SHA256 over
/// `verb\nresourceType\nresourceLink\ndate\n\n` (verb, type and date in
/// lowercase), base64, URL-encoded.
pub fn auth_header(key_b64: &str, verb: &str, resource_type: &str, resource_link: &str, date: &str) -> Result<String> {
    let key = B64.decode(key_b64.trim()).map_err(|_| Error::AuthFailed("la clave de la cuenta no es base64 válido".into()))?;
    let payload = format!(
        "{}\n{}\n{}\n{}\n\n",
        verb.to_lowercase(),
        resource_type.to_lowercase(),
        resource_link,
        date.to_lowercase()
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|e| Error::AuthFailed(e.to_string()))?;
    mac.update(payload.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());
    Ok(enc(&format!("type=master&ver=1.0&sig={sig}")))
}

/// `x-ms-date`: RFC 1123 in GMT.
fn http_date() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// The endpoint without a trailing slash; `https://` added when missing.
pub fn endpoint(cfg: &ConnectionConfig) -> String {
    let h = cfg.host.trim().trim_end_matches('/');
    if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else if cfg.port != 0 {
        format!("https://{h}:{}", cfg.port)
    } else {
        format!("https://{h}")
    }
}

#[async_trait]
impl Driver for CosmosDriver {
    fn info(&self) -> &DriverInfo {
        info()
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Concurrent point creates, throttling honored (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Between Cosmos DB sessions: the items themselves, so explicit nulls
    /// and missing keys stay apart (see `transfer.rs`).
    fn supports_native_copy(&self, target: &str) -> bool {
        target == "cosmosdb"
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

    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: false, monitor: true, ..Default::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::create_templates()
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(ddl::insert_script(&target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        Ok(ddl::update_script(&target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(&target.name, keys)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        if cfg.host.trim().is_empty() {
            return Err(Error::Connect("Falta el endpoint de la cuenta.".into()));
        }
        let key = cfg
            .option("account_key")
            .or(cfg.password.as_deref().filter(|p| !p.is_empty()))
            .ok_or_else(|| Error::AuthFailed("Falta la clave de la cuenta.".into()))?
            .to_string();
        B64.decode(key.trim()).map_err(|_| Error::AuthFailed("La clave de la cuenta no es base64 válido.".into()))?;
        let http = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .user_agent("DBine")
            .build()
            .map_err(Error::connect)?;
        let mut s = CosmosSession {
            http,
            base: endpoint(cfg),
            key,
            db: database.filter(|d| !d.is_empty()).unwrap_or(&cfg.database).to_string(),
            container: None,
            read_only: cfg.read_only,
            pk_paths: HashMap::new(),
            monitor_cache: None,
        };
        // Proves the endpoint and the key.
        let dbs = s.list_databases().await.map_err(|e| match e {
            Error::Query(m) => Error::Connect(m),
            e => e,
        })?;
        if s.db.is_empty() {
            s.db = dbs.into_iter().next().unwrap_or_default();
        }
        Ok(Box::new(s))
    }
}

pub struct CosmosSession {
    http: reqwest::Client,
    base: String,
    key: String,
    db: String,
    /// The container of the last query (`USE`, directive or inferred).
    container: Option<String>,
    read_only: bool,
    /// Partition key paths per container, for INSERT / UPSERT.
    pk_paths: HashMap<String, Vec<String>>,
    /// The monitor's per-container reads (usage, partitions), refreshed
    /// once a minute: (when, database, container id → stats).
    monitor_cache: Option<(Instant, String, HashMap<String, monitor::ContainerStats>)>,
}

struct Reply {
    body: Value,
    continuation: Option<String>,
    charge: f64,
    /// `x-ms-documentdb-query-metrics`, when asked for.
    metrics: Option<String>,
    /// `x-ms-cosmos-index-utilization`, when asked for.
    index_metrics: Option<String>,
    /// `x-ms-resource-usage` / `x-ms-resource-quota` (`k=v;…`).
    usage: Option<String>,
    quota: Option<String>,
}

/// A query's items and, when asked for, its metrics.
#[derive(Default)]
struct Run {
    items: Vec<Value>,
    more: bool,
    charge: f64,
    /// One metrics string per page (and partition key range).
    metrics: Vec<String>,
    index_metrics: Vec<String>,
    pages: usize,
    by_range: usize,
}

impl CosmosSession {
    /// One signed request. `path` is URL path segments already encoded;
    /// `link` the unencoded resource link it's signed with.
    async fn call(
        &self,
        method: Method,
        rtype: &str,
        link: &str,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, String)],
    ) -> Result<Reply> {
        let date = http_date();
        let auth = auth_header(&self.key, method.as_str(), rtype, link, &date)?;
        let mut rq = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", auth)
            .header("x-ms-date", date)
            .header("x-ms-version", API_VERSION)
            .header("Accept", "application/json");
        for (k, v) in headers {
            rq = rq.header(*k, v);
        }
        if let Some(b) = body {
            rq = rq.body(b.to_string());
        }
        let resp = rq.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status();
        let h = resp.headers();
        let continuation = h.get("x-ms-continuation").and_then(|v| v.to_str().ok()).map(str::to_string);
        let charge = h.get("x-ms-request-charge").and_then(|v| v.to_str().ok()?.parse().ok()).unwrap_or(0.0);
        let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string);
        let metrics = hv("x-ms-documentdb-query-metrics");
        let index_metrics = hv("x-ms-cosmos-index-utilization");
        let usage = hv("x-ms-resource-usage");
        let quota = hv("x-ms-resource-quota");
        let substatus = h.get("x-ms-substatus").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if status.is_success() {
            return Ok(Reply { body, continuation, charge, metrics, index_metrics, usage, quota });
        }
        let msg = error_message(&body);
        Err(match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::AuthFailed(msg),
            StatusCode::BAD_REQUEST if substatus == "1004" || msg.contains("cannot be directly served by the gateway") => {
                Error::Unsupported(msg)
            }
            _ => Error::Query(msg),
        })
    }

    fn db_link(&self) -> Result<String> {
        if self.db.is_empty() {
            return Err(Error::Query("Elegí una base de datos.".into()));
        }
        Ok(format!("dbs/{}", self.db))
    }

    fn db_path(&self) -> Result<String> {
        self.db_link()?;
        Ok(format!("/dbs/{}", enc(&self.db)))
    }

    async fn containers(&self) -> Result<Vec<String>> {
        let items = self.list(&format!("{}/colls", self.db_path()?), "colls", &self.db_link()?, "DocumentCollections").await?;
        Ok(items.iter().filter_map(|c| c.get("id")?.as_str().map(str::to_string)).collect())
    }

    /// A feed (`/dbs`, `/colls`), following continuations.
    async fn list(&self, path: &str, rtype: &str, link: &str, field: &str) -> Result<Vec<Value>> {
        Ok(self.list_charged(path, rtype, link, field).await?.0)
    }

    /// [`Self::list`] with the RUs it cost.
    async fn list_charged(&self, path: &str, rtype: &str, link: &str, field: &str) -> Result<(Vec<Value>, f64)> {
        let mut out = Vec::new();
        let mut charge = 0.0;
        let mut cont: Option<String> = None;
        loop {
            let mut h = Vec::new();
            if let Some(c) = &cont {
                h.push(("x-ms-continuation", c.clone()));
            }
            let r = self.call(Method::GET, rtype, link, path, None, &h).await?;
            charge += r.charge;
            out.extend(r.body.get(field).and_then(Value::as_array).cloned().unwrap_or_default());
            match r.continuation {
                Some(c) if !c.is_empty() => cont = Some(c),
                _ => return Ok((out, charge)),
            }
        }
    }

    /// Run a query on a container, up to `limit` items. Returns the items,
    /// whether more were left, and the total charge.
    async fn query(&self, coll: &str, sql: &str, limit: usize, range: Option<&str>) -> Result<(Vec<Value>, bool, f64)> {
        let r = self.query_run(coll, sql, limit, range, false).await?;
        Ok((r.items, r.more, r.charge))
    }

    /// [`Self::query`], asking for the query and index metrics when `metrics`.
    async fn query_run(&self, coll: &str, sql: &str, limit: usize, range: Option<&str>, metrics: bool) -> Result<Run> {
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}/docs", self.db_path()?, enc(coll));
        let body = json!({ "query": sql, "parameters": [] });
        let mut run = Run::default();
        let mut cont: Option<String> = None;
        loop {
            let page = (limit - run.items.len()).clamp(1, 1000);
            let mut h = vec![
                ("x-ms-documentdb-isquery", "True".to_string()),
                ("Content-Type", "application/query+json".to_string()),
                ("x-ms-documentdb-query-enablecrosspartition", "True".to_string()),
                ("x-ms-max-item-count", page.to_string()),
            ];
            if metrics {
                h.push(("x-ms-documentdb-populatequerymetrics", "True".to_string()));
                h.push(("x-ms-cosmos-populateindexmetrics", "True".to_string()));
            }
            if let Some(r) = range {
                h.push(("x-ms-documentdb-partitionkeyrangeid", r.to_string()));
            }
            if let Some(c) = &cont {
                h.push(("x-ms-continuation", c.clone()));
            }
            let r = self.call(Method::POST, "docs", &link, &path, Some(&body), &h).await?;
            run.charge += r.charge;
            run.pages += 1;
            run.metrics.extend(r.metrics);
            run.index_metrics.extend(r.index_metrics);
            run.items.extend(r.body.get("Documents").and_then(Value::as_array).cloned().unwrap_or_default());
            let more = r.continuation.filter(|c| !c.is_empty());
            if run.items.len() >= limit {
                run.more = run.items.len() > limit || more.is_some();
                run.items.truncate(limit);
                return Ok(run);
            }
            match more {
                Some(c) => cont = Some(c),
                None => return Ok(run),
            }
        }
    }

    /// Per partition key range, when the gateway can't serve the query alone.
    async fn query_by_range(&self, coll: &str, sql: &str, limit: usize, out: &mut QueryOutcome, metrics: bool) -> Result<Run> {
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}/pkranges", self.db_path()?, enc(coll));
        let ranges = self.list(&path, "pkranges", &link, "PartitionKeyRanges").await?;
        out.messages.push(format!(
            "El gateway no resuelve esta consulta entre particiones: se ejecutó en cada uno de los {} rangos de partición y los resultados no se combinan (ORDER BY y agregados son por rango).",
            ranges.len()
        ));
        let mut run = Run { by_range: ranges.len(), ..Default::default() };
        for id in ranges.iter().filter_map(|r| r.get("id")?.as_str()) {
            if run.items.len() >= limit {
                run.more = true;
                break;
            }
            let r = self.query_run(coll, sql, limit - run.items.len(), Some(id), metrics).await?;
            run.items.extend(r.items);
            run.more |= r.more;
            run.charge += r.charge;
            run.pages += r.pages;
            run.metrics.extend(r.metrics);
            run.index_metrics.extend(r.index_metrics);
        }
        Ok(run)
    }

    /// The gateway's query plan (what the SDKs ask for before a
    /// cross-partition query): `queryInfo` and `queryRanges`. Nothing runs.
    async fn query_plan(&self, coll: &str, sql: &str) -> Result<Value> {
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}/docs", self.db_path()?, enc(coll));
        let body = json!({ "query": sql, "parameters": [] });
        let h = vec![
            ("x-ms-documentdb-isquery", "True".to_string()),
            ("Content-Type", "application/query+json".to_string()),
            ("x-ms-documentdb-query-enablecrosspartition", "True".to_string()),
            ("x-ms-cosmos-is-query-plan-request", "True".to_string()),
            ("x-ms-cosmos-supported-query-features", plan::SUPPORTED_QUERY_FEATURES.to_string()),
            ("x-ms-cosmos-query-version", "1.0".to_string()),
        ];
        Ok(self.call(Method::POST, "docs", &link, &path, Some(&body), &h).await?.body)
    }

    /// The container a statement runs on (see the module docs).
    async fn container_for(&mut self, sql: &str, known: &mut Option<Vec<String>>) -> Result<String> {
        let first = sql.split_whitespace().next().unwrap_or_default();
        if !first.eq_ignore_ascii_case("select") {
            return Err(Error::Query(format!(
                "Cosmos DB solo acepta consultas SELECT, `USE <contenedor>` y las sentencias CREATE / DROP CONTAINER, INSERT / UPSERT INTO, UPDATE y DELETE de DBine, no `{first}`."
            )));
        }
        // FROM <container>, when it names one of the database's.
        let mut coll = self.container.clone();
        if let Some(src) = from_source(sql) {
            if known.is_none() {
                *known = Some(self.containers().await?);
            }
            let names = known.as_deref().unwrap_or_default();
            if names.contains(&src) {
                coll = Some(src);
            } else if coll.is_none() && names.len() == 1 {
                coll = names.first().cloned();
            }
        }
        let coll = coll.ok_or_else(|| {
            Error::Query(
                "Falta el contenedor: agregá una línea `-- container: <nombre>` o `USE <nombre>;` antes de la consulta.".into(),
            )
        })?;
        self.container = Some(coll.clone());
        Ok(coll)
    }

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: solo se permiten consultas SELECT.".into()));
        }
        Ok(())
    }

    /// Partition key paths of a container (cached).
    async fn partition_paths(&mut self, coll: &str) -> Result<Vec<String>> {
        if let Some(p) = self.pk_paths.get(coll) {
            return Ok(p.clone());
        }
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}", self.db_path()?, enc(coll));
        let body = self.call(Method::GET, "colls", &link, &path, None, &[]).await?.body;
        let paths: Vec<String> = body["partitionKey"]["paths"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        self.pk_paths.insert(coll.to_string(), paths.clone());
        Ok(paths)
    }

    /// One of the extensions of [`ddl`].
    async fn run_admin(&mut self, admin: Admin, out: &mut QueryOutcome) -> Result<()> {
        self.check_writable()?;
        let db_link = self.db_link()?;
        let db_path = self.db_path()?;
        let json_body = ("Content-Type", "application/json".to_string());
        match admin {
            Admin::CreateContainer { name, if_not_exists, mut body } => {
                if if_not_exists && self.containers().await?.contains(&name) {
                    out.messages.push(format!("El contenedor {name} ya existe; no se creó."));
                    return Ok(());
                }
                let mut headers = vec![json_body];
                if let Some(o) = body.as_object_mut() {
                    let ru = |v: Value| {
                        v.as_u64().ok_or_else(|| Error::Query(format!("Las RU/s tienen que ser un número entero, no {v}.")))
                    };
                    let manual = o.remove(ddl::MANUAL_KEY).map(ru).transpose()?;
                    let auto = o.remove(ddl::AUTOSCALE_KEY).map(ru).transpose()?;
                    match (manual, auto) {
                        (Some(_), Some(_)) => {
                            return Err(Error::Query(format!(
                                "Usá \"{}\" o \"{}\", no los dos.",
                                ddl::MANUAL_KEY,
                                ddl::AUTOSCALE_KEY
                            )))
                        }
                        (Some(n), None) => headers.push(("x-ms-offer-throughput", n.to_string())),
                        (None, Some(n)) => {
                            headers.push(("x-ms-cosmos-offer-autopilot-settings", json!({ "maxThroughput": n }).to_string()))
                        }
                        (None, None) => {}
                    }
                    o.insert("id".into(), name.clone().into());
                }
                self.call(Method::POST, "colls", &db_link, &format!("{db_path}/colls"), Some(&body), &headers).await?;
                out.messages.push(format!("Contenedor {name} creado."));
            }
            Admin::DropContainer { name, if_exists } => {
                if if_exists && !self.containers().await?.contains(&name) {
                    out.messages.push(format!("El contenedor {name} no existe; no se borró nada."));
                    return Ok(());
                }
                let link = format!("{db_link}/colls/{name}");
                let path = format!("{db_path}/colls/{}", enc(&name));
                self.call(Method::DELETE, "colls", &link, &path, None, &[]).await?;
                self.pk_paths.remove(&name);
                if self.container.as_deref() == Some(name.as_str()) {
                    self.container = None;
                }
                out.messages.push(format!("Contenedor {name} borrado."));
            }
            Admin::Insert { container, upsert, doc } => {
                if !doc.get("id").is_some_and(Value::is_string) {
                    return Err(Error::Query("El documento necesita un campo «id» de texto.".into()));
                }
                let pk = ddl::partition_key_header(&doc, &self.partition_paths(&container).await?);
                let link = format!("{db_link}/colls/{container}");
                let path = format!("{db_path}/colls/{}/docs", enc(&container));
                let mut headers = vec![json_body, ("x-ms-documentdb-partitionkey", pk)];
                if upsert {
                    headers.push(("x-ms-documentdb-is-upsert", "True".to_string()));
                }
                self.call(Method::POST, "docs", &link, &path, Some(&doc), &headers).await?;
                out.push_affected(1);
            }
            Admin::Update { container, set, filter } => {
                let where_ = Value::Object(filter.clone());
                let (found, _, _) = self.query(&container, &ddl::update_query(&filter), 2, None).await?;
                let stored = match <[Value; 1]>::try_from(found) {
                    Ok([d]) => d,
                    Err(v) if v.is_empty() => {
                        return Err(Error::Query(format!("No hay ningún documento con {where_} en {container}.")));
                    }
                    Err(_) => {
                        return Err(Error::Query(format!(
                            "Hay más de un documento con {where_} en {container} (en distintas particiones); agregá la clave de partición al WHERE."
                        )))
                    }
                };
                let pk = ddl::partition_key_header(&stored, &self.partition_paths(&container).await?);
                let etag = stored.get("_etag").and_then(Value::as_str).map(str::to_string);
                let doc = ddl::apply_set(stored, &set)?;
                let link = format!("{db_link}/colls/{container}");
                let path = format!("{db_path}/colls/{}/docs", enc(&container));
                // Upsert guarded by the read revision: a concurrent write fails with 412.
                let mut headers = vec![json_body, ("x-ms-documentdb-partitionkey", pk), ("x-ms-documentdb-is-upsert", "True".to_string())];
                if let Some(e) = etag {
                    headers.push(("If-Match", e));
                }
                self.call(Method::POST, "docs", &link, &path, Some(&doc), &headers).await?;
                out.push_affected(1);
            }
            Admin::Delete { container, filter } => {
                let where_ = Value::Object(filter.clone());
                let (found, _, _) = self.query(&container, &ddl::update_query(&filter), 2, None).await?;
                let stored = match <[Value; 1]>::try_from(found) {
                    Ok([d]) => d,
                    Err(v) if v.is_empty() => {
                        return Err(Error::Query(format!("No hay ningún documento con {where_} en {container}.")));
                    }
                    Err(_) => {
                        return Err(Error::Query(format!(
                            "Hay más de un documento con {where_} en {container} (en distintas particiones); agregá la clave de partición al WHERE."
                        )))
                    }
                };
                let id = stored.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let pk = ddl::partition_key_header(&stored, &self.partition_paths(&container).await?);
                let link = format!("{db_link}/colls/{container}/docs/{id}");
                let path = format!("{db_path}/colls/{}/docs/{}", enc(&container), enc(&id));
                let mut headers = vec![("x-ms-documentdb-partitionkey", pk)];
                if let Some(e) = stored.get("_etag").and_then(Value::as_str) {
                    headers.push(("If-Match", e.to_string()));
                }
                self.call(Method::DELETE, "docs", &link, &path, None, &headers).await?;
                out.push_affected(1);
            }
            Admin::CreateUser { name } => {
                self.call(Method::POST, "users", &db_link, &format!("{db_path}/users"), Some(&json!({ "id": name })), &[json_body]).await?;
                out.messages.push(format!("Usuario {name} creado."));
            }
            Admin::DropUser { name } => {
                let link = format!("{db_link}/users/{name}");
                self.call(Method::DELETE, "users", &link, &format!("{db_path}/users/{}", enc(&name)), None, &[]).await?;
                out.messages.push(format!("Usuario {name} borrado."));
            }
            Admin::Grant { mode, container, user } => {
                let link = format!("{db_link}/users/{user}");
                let path = format!("{db_path}/users/{}/permissions", enc(&user));
                let resource = format!("{db_link}/colls/{container}");
                // One permission per container: an existing one is replaced.
                let existing = security::permissions(self, &user).await?.into_iter().find(|p| {
                    p.get("resource").and_then(Value::as_str).is_some_and(|r| r.trim_end_matches('/') == resource)
                });
                let id = existing.as_ref().and_then(|p| p.get("id")?.as_str().map(str::to_string)).unwrap_or_else(|| container.clone());
                let body = json!({ "id": id, "permissionMode": mode, "resource": resource });
                match existing {
                    Some(_) => {
                        let plink = format!("{link}/permissions/{id}");
                        self.call(Method::PUT, "permissions", &plink, &format!("{path}/{}", enc(&id)), Some(&body), &[json_body]).await?;
                    }
                    None => {
                        self.call(Method::POST, "permissions", &link, &path, Some(&body), &[json_body]).await?;
                    }
                }
                out.messages.push(format!("Permiso {mode} sobre {container} otorgado a {user}."));
            }
            Admin::Revoke { mode, container, user } => {
                let resource = format!("{db_link}/colls/{container}");
                let matching: Vec<String> = security::permissions(self, &user)
                    .await?
                    .into_iter()
                    .filter(|p| {
                        p.get("resource").and_then(Value::as_str).is_some_and(|r| r.trim_end_matches('/') == resource)
                            && p.get("permissionMode").and_then(Value::as_str).is_some_and(|m| m.eq_ignore_ascii_case(&mode))
                    })
                    .filter_map(|p| p.get("id")?.as_str().map(str::to_string))
                    .collect();
                if matching.is_empty() {
                    out.messages.push(format!("{user} no tiene el permiso {mode} sobre {container}; no se revocó nada."));
                }
                for id in matching {
                    let link = format!("{db_link}/users/{user}/permissions/{id}");
                    let path = format!("{db_path}/users/{}/permissions/{}", enc(&user), enc(&id));
                    self.call(Method::DELETE, "permissions", &link, &path, None, &[]).await?;
                    out.messages.push(format!("Permiso {mode} sobre {container} revocado a {user}."));
                }
            }
        }
        Ok(())
    }

    /// Run a query into `out`, as `execute` does.
    async fn run_into(&self, coll: &str, sql: &str, max_rows: usize, out: &mut QueryOutcome, metrics: bool) -> Result<Run> {
        let limit = max_rows.saturating_add(1);
        let run = match self.query_run(coll, sql, limit, None, metrics).await {
            Err(Error::Unsupported(_)) => self.query_by_range(coll, sql, limit, out, metrics).await?,
            r => r?,
        };
        push_items(out, &run.items, max_rows);
        if run.more {
            if let Some(r) = out.results.last_mut() {
                r.truncated = true;
            }
        }
        out.messages.push(format!("Costo de la consulta: {:.2} RU", run.charge));
        Ok(run)
    }
}

fn error_message(body: &Value) -> String {
    let raw = match body.get("message").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => match body {
            Value::String(s) if !s.is_empty() => s.clone(),
            other => other.to_string(),
        },
    };
    // "Message: {"Errors":["…"]}\r\nActivityId: …, Request URI: …"
    let head = raw.split("\r\nActivityId").next().unwrap_or(&raw).split(", Request URI").next().unwrap_or(&raw);
    let head = head.trim().trim_start_matches("Message: ");
    if let Ok(v) = serde_json::from_str::<Value>(head) {
        if let Some(errs) = v.get("Errors").and_then(Value::as_array) {
            return errs.iter().map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string)).collect::<Vec<_>>().join(" ");
        }
        if let Some(errs) = v.get("errors").and_then(Value::as_array) {
            return errs.iter().filter_map(|e| e.get("message")?.as_str()).collect::<Vec<_>>().join(" ");
        }
    }
    head.to_string()
}

/// A statement of the script, after directives and `USE`.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Use(String),
    Query { sql: String },
    /// A DBine extension (see [`ddl`]).
    Admin(Admin),
}

/// `-- container: x` lines become `USE` statements, then the script is
/// split on `;` (outside JSON bodies).
pub fn parse_script(text: &str) -> Result<Vec<Stmt>> {
    let mut pre = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim();
        let directive = t.strip_prefix("--").map(str::trim).and_then(|r| {
            let (k, v) = r.split_once(':')?;
            (k.trim().eq_ignore_ascii_case("container") && !v.trim().is_empty()).then(|| v.trim().to_string())
        });
        match directive {
            Some(name) => pre.push_str(&format!(";\nUSE \"{}\";\n", name.replace('"', "\"\""))),
            None => {
                pre.push_str(line);
                pre.push('\n');
            }
        }
    }
    ddl::split_script(&pre)
        .into_iter()
        .map(|s| {
            if let Some(a) = ddl::parse_admin(&s)? {
                return Ok(Stmt::Admin(a));
            }
            let mut words = s.splitn(2, char::is_whitespace);
            Ok(match (words.next(), words.next()) {
                (Some(w), Some(rest)) if w.eq_ignore_ascii_case("use") => Stmt::Use(unquote(rest.trim())),
                _ => Stmt::Query { sql: s },
            })
        })
        .collect()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('[') && s.ends_with(']')) || (s.starts_with('`') && s.ends_with('`'))) {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

/// The identifier after the top-level `FROM`, if any.
pub fn from_source(sql: &str) -> Option<String> {
    let chars: Vec<char> = sql.chars().collect();
    let (mut i, mut depth, mut quote): (usize, i32, Option<char>) = (0, 0, None);
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '(' || c == '[' {
            depth += 1;
        } else if c == ')' || c == ']' {
            depth -= 1;
        } else if depth == 0
            && (i == 0 || !chars[i - 1].is_alphanumeric() && chars[i - 1] != '_')
            && chars.get(i..i + 4).is_some_and(|w| w.iter().collect::<String>().eq_ignore_ascii_case("from"))
            && chars.get(i + 4).is_some_and(|c| c.is_whitespace())
        {
            let rest: String = chars[i + 4..].iter().collect();
            let name: String = rest.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
            return (!name.is_empty()).then_some(name);
        }
        i += 1;
    }
    None
}

/// Top-level keys: `id` first, system properties last.
pub fn union_keys(docs: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for d in docs {
        if let Some(o) = d.as_object() {
            for k in o.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    let (sys, mut user): (Vec<String>, Vec<String>) = keys.into_iter().partition(|k| SYSTEM_PROPS.contains(&k.as_str()));
    if let Some(i) = user.iter().position(|k| k == "id") {
        let id = user.remove(i);
        user.insert(0, id);
    }
    user.extend(sys);
    user
}

pub fn cell(v: &Value) -> Value {
    match v {
        Value::Object(_) | Value::Array(_) => Value::String(v.to_string()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                json_i64(i)
            } else if let Some(u) = n.as_u64() {
                json_u64(u)
            } else {
                json_f64(n.as_f64().unwrap_or(0.0))
            }
        }
        other => other.clone(),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn infer_columns(docs: &[Value]) -> Vec<ColumnInfo> {
    union_keys(docs)
        .into_iter()
        .map(|name| {
            let mut types: Vec<(&'static str, usize)> = Vec::new();
            let mut present = 0;
            let mut null_seen = false;
            for v in docs.iter().filter_map(|d| d.get(&name)) {
                present += 1;
                if v.is_null() {
                    null_seen = true;
                    continue;
                }
                let t = type_name(v);
                match types.iter_mut().find(|(n, _)| *n == t) {
                    Some(e) => e.1 += 1,
                    None => types.push((t, 1)),
                }
            }
            types.sort_by(|a, b| b.1.cmp(&a.1));
            ColumnInfo {
                data_type: if types.is_empty() { "null".into() } else { types.iter().map(|t| t.0).collect::<Vec<_>>().join("|") },
                nullable: present < docs.len() || null_seen,
                primary_key: name == "id",
                auto_increment: false,
                default_value: None,
                name,
            }
        })
        .collect()
}

/// A container as the designer models it: fields inferred from `sample`
/// (`id` as the primary key), unique keys and composite indexes as
/// indexes, and partition key, TTL, indexing policy and throughput (from
/// its `offer`, if any) as options.
pub fn container_schema(coll: &Value, sample: &[Value], offer: Option<&Value>) -> TableSchema {
    let paths = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(ddl::field_of).collect()).unwrap_or_default()
    };
    let mut options = BTreeMap::new();
    let pk: Vec<String> = coll["partitionKey"]["paths"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    options.insert(ddl::PARTITION_KEY.to_string(), pk.join(", "));
    if let Some(ttl) = coll.get("defaultTtl").and_then(Value::as_i64) {
        options.insert(ddl::DEFAULT_TTL.to_string(), ttl.to_string());
    }
    let mut indexes = Vec::new();
    for (n, u) in coll["uniqueKeyPolicy"]["uniqueKeys"].as_array().into_iter().flatten().enumerate() {
        indexes.push(IndexDef { name: format!("unique_{}", n + 1), columns: paths(&u["paths"]), unique: true, ..Default::default() });
    }
    let mut policy = coll.get("indexingPolicy").cloned();
    if let Some(o) = policy.as_mut().and_then(Value::as_object_mut) {
        for (n, c) in o.remove("compositeIndexes").and_then(|c| c.as_array().cloned()).unwrap_or_default().iter().enumerate() {
            let columns = c
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    let f = ddl::field_of(p["path"].as_str().unwrap_or_default());
                    if p["order"].as_str() == Some("descending") {
                        format!("{f} DESC")
                    } else {
                        f
                    }
                })
                .collect();
            indexes.push(IndexDef {
                name: format!("composite_{}", n + 1),
                columns,
                kind: Some(ddl::COMPOSITE.into()),
                ..Default::default()
            });
        }
        // Spatial, full-text and vector indexes.
        for (kind, list) in ddl::POLICY_KINDS {
            for (n, e) in o.remove(*list).and_then(|c| c.as_array().cloned()).unwrap_or_default().iter().enumerate() {
                indexes.push(ddl::policy_index(kind, n + 1, e));
            }
        }
    }
    if let Some(p) = policy {
        options.insert(ddl::INDEXING_POLICY.to_string(), serde_json::to_string_pretty(&p).unwrap_or_default());
    }
    let content = offer.map(|o| &o["content"]);
    match (
        content.and_then(|c| c["offerAutopilotSettings"]["maxThroughput"].as_u64()),
        content.and_then(|c| c["offerThroughput"].as_u64()),
    ) {
        (Some(max), _) => {
            options.insert(ddl::THROUGHPUT_MODE.to_string(), "autoscale".into());
            options.insert(ddl::THROUGHPUT.to_string(), max.to_string());
        }
        (None, Some(ru)) => {
            options.insert(ddl::THROUGHPUT_MODE.to_string(), "manual".into());
            options.insert(ddl::THROUGHPUT.to_string(), ru.to_string());
        }
        _ => {
            options.insert(ddl::THROUGHPUT_MODE.to_string(), "none".into());
        }
    }
    let columns: Vec<ColumnDef> = infer_columns(sample)
        .into_iter()
        .filter(|c| !SYSTEM_PROPS.contains(&c.name.as_str()))
        .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
        .collect();
    TableSchema {
        kind: kinds::COLLECTION.into(),
        name: coll["id"].as_str().unwrap_or_default().to_string(),
        columns,
        primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
        indexes,
        options,
        ..Default::default()
    }
}

/// One row per item; scalars (`SELECT VALUE …`) in a `value` column.
pub fn push_items(out: &mut QueryOutcome, items: &[Value], max_rows: usize) {
    if items.iter().all(Value::is_object) {
        let keys = union_keys(items);
        out.begin_result(keys.iter().map(|k| ResultColumn { name: k.clone(), type_name: String::new() }).collect());
        for d in items {
            out.push_row(keys.iter().map(|k| d.get(k).map_or(Value::Null, cell)).collect(), max_rows);
        }
    } else {
        out.begin_result(vec![ResultColumn { name: "value".into(), type_name: String::new() }]);
        for v in items {
            out.push_row(vec![cell(v)], max_rows);
        }
    }
}

#[async_trait]
impl Session for CosmosSession {
    async fn server_version(&mut self) -> Result<String> {
        let r = self.call(Method::GET, "", "", "/", None, &[]).await?;
        let id = r.body.get("id").and_then(Value::as_str).unwrap_or_default();
        Ok(if id.is_empty() { "Azure Cosmos DB (NoSQL)".into() } else { format!("Azure Cosmos DB (NoSQL) · {id}") })
    }

    /// Account, databases (with the account usage and quota headers),
    /// containers, offers and, once a minute, each container's usage and
    /// partition key ranges.
    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut charge = 0.0;
        let account = match self.call(Method::GET, "", "", "/", None, &[]).await {
            Ok(r) => {
                charge += r.charge;
                Some(r.body)
            }
            Err(e @ (Error::Connect(_) | Error::AuthFailed(_))) => return Err(e),
            Err(_) => None,
        };
        let dbs = self.call(Method::GET, "dbs", "", "/dbs", None, &[("x-ms-populatequotainfo", "true".to_string())]).await?;
        charge += dbs.charge;
        let db_list = dbs.body.get("Databases").and_then(Value::as_array).cloned().unwrap_or_default();
        let db_rid = db_list
            .iter()
            .find(|d| d.get("id").and_then(Value::as_str) == Some(self.db.as_str()))
            .and_then(|d| d.get("_rid").and_then(Value::as_str).map(str::to_string));
        let (containers, c) = if self.db.is_empty() {
            (Vec::new(), 0.0)
        } else {
            self.list_charged(&format!("{}/colls", self.db_path()?), "colls", &self.db_link()?, "DocumentCollections").await?
        };
        charge += c;
        let offers = match self.list_charged("/offers", "offers", "", "Offers").await {
            Ok((o, c)) => {
                charge += c;
                Some(o)
            }
            Err(_) => None,
        };
        let fresh = self
            .monitor_cache
            .as_ref()
            .is_some_and(|(t, db, _)| *db == self.db && t.elapsed() < Duration::from_secs(60));
        if !fresh {
            let mut stats = HashMap::new();
            for c in containers.iter().take(monitor::MAX_CONTAINERS) {
                let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
                let link = format!("{}/colls/{id}", self.db_link()?);
                let path = format!("{}/colls/{}", self.db_path()?, enc(id));
                let quota = [("x-ms-populatequotainfo", "true".to_string())];
                let usage = match self.call(Method::GET, "colls", &link, &path, None, &quota).await {
                    Ok(r) => {
                        charge += r.charge;
                        r.usage.map(|u| monitor::parse_usage(&u))
                    }
                    Err(_) => None,
                };
                let ranges = match self.list_charged(&format!("{path}/pkranges"), "pkranges", &link, "PartitionKeyRanges").await {
                    Ok((r, c)) => {
                        charge += c;
                        Some(r)
                    }
                    Err(_) => None,
                };
                stats.insert(id.to_string(), monitor::ContainerStats { usage, ranges });
            }
            self.monitor_cache = Some((Instant::now(), self.db.clone(), stats));
        }
        let cached = self.monitor_cache.as_ref().map(|c| &c.2);
        let stats: Vec<monitor::ContainerStats> = containers
            .iter()
            .take(monitor::MAX_CONTAINERS)
            .map(|c| {
                let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
                cached.and_then(|m| m.get(id)).cloned().unwrap_or_default()
            })
            .collect();
        Ok(monitor::snapshot(&monitor::Inputs {
            account: account.as_ref(),
            database: &self.db,
            database_rid: db_rid.as_deref(),
            databases: db_list.len(),
            containers: &containers,
            stats: &stats,
            offers: offers.as_deref(),
            account_usage: dbs.usage.as_deref().map(monitor::parse_usage),
            account_quota: dbs.quota.as_deref().map(monitor::parse_usage),
            charge,
        }))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let items = self.list("/dbs", "dbs", "", "Databases").await?;
        let mut v: Vec<String> = items.iter().filter_map(|d| d.get("id")?.as_str().map(str::to_string)).collect();
        v.sort();
        Ok(v)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut v = self.containers().await?;
        v.sort_by_key(|n| n.to_lowercase());
        Ok(v.into_iter().map(|name| DbObject { kind: kinds::COLLECTION.into(), schema: None, name, parent: None }).collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let (items, _, _) = self.query(&obj.name, "SELECT TOP 100 * FROM c", 100, None).await?;
        Ok(infer_columns(&items))
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let link = format!("{}/colls/{}", self.db_link()?, obj.name);
        let path = format!("{}/colls/{}", self.db_path()?, enc(&obj.name));
        let mut r = self.call(Method::GET, "colls", &link, &path, None, &[]).await?.body;
        if let Some(m) = r.as_object_mut() {
            for k in ["_rid", "_self", "_etag", "_docs", "_sprocs", "_triggers", "_udfs", "_conflicts"] {
                m.remove(k);
            }
        }
        Ok(Some(serde_json::to_string_pretty(&r)?))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let colls = self.list(&format!("{}/colls", self.db_path()?), "colls", &self.db_link()?, "DocumentCollections").await?;
        // Serverless accounts (and some emulators) have no offers.
        let offers = self.list("/offers", "offers", "", "Offers").await.unwrap_or_default();
        let mut out = Vec::new();
        for c in &colls {
            let name = c["id"].as_str().unwrap_or_default();
            let sample = self.query(name, "SELECT TOP 100 * FROM c", 100, None).await.map(|r| r.0).unwrap_or_default();
            let offer = offers.iter().find(|o| o["offerResourceId"].as_str().is_some() && o["offerResourceId"] == c["_rid"]);
            out.push(container_schema(c, &sample, offer));
        }
        out.sort_by_key(|t| t.name.to_lowercase());
        Ok(out)
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.check_writable()?;
        let body = json!({ "id": name });
        self.call(Method::POST, "dbs", "", "/dbs", Some(&body), &[("Content-Type", "application/json".to_string())]).await?;
        Ok(())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.check_writable()?;
        self.call(Method::DELETE, "dbs", &format!("dbs/{name}"), &format!("/dbs/{}", enc(name)), None, &[]).await?;
        if self.db == name {
            self.container = None;
            self.pk_paths.clear();
        }
        Ok(())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("-- container: {}\nSELECT TOP {limit} * FROM c", obj.name)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = parse_script(text)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let mut known: Option<Vec<String>> = None;
        for stmt in stmts {
            let sql = match stmt {
                Stmt::Use(c) => {
                    self.container = Some(c);
                    continue;
                }
                Stmt::Admin(a) => {
                    known = None;
                    self.run_admin(a, out).await?;
                    continue;
                }
                Stmt::Query { sql } => sql,
            };
            let coll = self.container_for(&sql, &mut known).await?;
            self.run_into(&coll, &sql, max_rows, out, false).await?;
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

    /// For `copy_native`.
    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    /// Estimated: the gateway's query plan (nothing runs). Actual: the
    /// query runs with query and index metrics, which become the tree.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = parse_script(text)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let mut known: Option<Vec<String>> = None;
        for stmt in stmts {
            let sql = match stmt {
                Stmt::Use(c) => {
                    self.container = Some(c);
                    continue;
                }
                Stmt::Admin(a) if analyze => {
                    known = None;
                    self.run_admin(a, out).await?;
                    continue;
                }
                Stmt::Admin(_) => {
                    out.messages.push("Sin plan para las sentencias de administración o escritura (no se ejecutaron).".into());
                    continue;
                }
                Stmt::Query { sql } => sql,
            };
            let coll = self.container_for(&sql, &mut known).await?;
            if analyze {
                let run = self.run_into(&coll, &sql, max_rows, out, true).await?;
                if run.metrics.is_empty() {
                    out.messages.push("El servidor no devolvió métricas de la consulta (x-ms-documentdb-query-metrics).".into());
                }
                out.plans.push(plan::from_metrics(&sql, &coll, &run.metrics, &run.index_metrics, run.charge, run.pages, run.by_range));
            } else {
                match self.query_plan(&coll, &sql).await {
                    Ok(qp) => out.plans.push(plan::from_query_plan(&sql, &coll, &qp)),
                    Err(e @ (Error::Connect(_) | Error::AuthFailed(_))) => return Err(e),
                    Err(e) => {
                        out.messages.push(format!("El servidor no devolvió el plan de la consulta: {e}"));
                        out.plans.push(plan::from_query_plan(&sql, &coll, &Value::Null));
                    }
                }
            }
        }
        Ok(())
    }

    /// The account key can't be told apart (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::decide())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_microsofts_example() {
        // From "Access control on Azure Cosmos DB resources" (REST docs).
        let key = "dsZQi3KtZmCv1ljt3VNWNm7sQUF1y5rJfC6kv5JiwvW0EndXdDku/dkKBp8/ufDToSxLzR4y+O/0H/t4bQtVNw==";
        let h = auth_header(key, "GET", "dbs", "dbs/ToDoList", "Thu, 27 Apr 2017 00:51:12 GMT").unwrap();
        assert!(h.eq_ignore_ascii_case("type%3dmaster%26ver%3d1.0%26sig%3dc09PEVJrgp2uQRkr934kFbTqhByc7TVr3OHyqlu%2bc%2bc%3d"), "{h}");
    }

    #[test]
    fn container_directive_use_and_split() {
        let s = parse_script("-- container: items\nSELECT * FROM c; USE \"other\";\nSELECT VALUE COUNT(1) FROM c -- note\n").unwrap();
        assert_eq!(
            s,
            vec![
                Stmt::Use("items".into()),
                Stmt::Query { sql: "SELECT * FROM c".into() },
                Stmt::Use("other".into()),
                Stmt::Query { sql: "SELECT VALUE COUNT(1) FROM c".into() },
            ]
        );
    }

    #[test]
    fn from_source_is_top_level() {
        assert_eq!(from_source("SELECT * FROM products p WHERE p.x = 'from y'").as_deref(), Some("products"));
        assert_eq!(from_source("SELECT (SELECT VALUE 1 FROM t IN c.tags) FROM c").as_deref(), Some("c"));
        assert_eq!(from_source("select c.fromage from c").as_deref(), Some("c"));
        assert_eq!(from_source("SELECT 1"), None);
    }

    #[test]
    fn keys_put_id_first_and_system_last() {
        let docs = vec![json!({ "_rid": "x", "name": "a", "id": "1", "_ts": 1 }), json!({ "id": "2", "n": { "a": 1 } })];
        assert_eq!(union_keys(&docs), ["id", "name", "n", "_rid", "_ts"]);
        let mut out = QueryOutcome::default();
        push_items(&mut out, &[json!(3)], 10);
        assert_eq!(out.results[0].columns[0].name, "value");
        assert_eq!(out.results[0].rows[0][0], json!(3));
    }

    #[test]
    fn server_errors_are_trimmed() {
        let body = json!({ "code": "BadRequest", "message": "Message: {\"errors\":[{\"severity\":\"Error\",\"message\":\"Syntax error, incorrect syntax near 'FORM'.\"}]}\r\nActivityId: 1, Microsoft.Azure.Documents.Common/2.14.0" });
        assert_eq!(error_message(&body), "Syntax error, incorrect syntax near 'FORM'.");
    }

    #[test]
    fn container_schema_round_trips_through_the_ddl() {
        let coll = json!({
            "id": "items", "_rid": "abc",
            "partitionKey": { "paths": ["/cat"], "kind": "Hash" },
            "defaultTtl": 60,
            "indexingPolicy": { "indexingMode": "consistent", "includedPaths": [{ "path": "/*" }],
                "compositeIndexes": [[{ "path": "/a", "order": "ascending" }, { "path": "/b", "order": "descending" }]],
                "spatialIndexes": [{ "path": "/loc/*", "types": ["Point", "Polygon"] }],
                "fullTextIndexes": [{ "path": "/text" }],
                "vectorIndexes": [{ "path": "/emb", "type": "quantizedFlat" }] },
            "uniqueKeyPolicy": { "uniqueKeys": [{ "paths": ["/email"] }] }
        });
        let offer = json!({ "offerResourceId": "abc", "content": { "offerThroughput": 400 } });
        let t = container_schema(&coll, &[json!({ "id": "1", "cat": "x", "_ts": 1 })], Some(&offer));
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "cat"]);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id"]);
        assert_eq!(t.options[ddl::PARTITION_KEY], "/cat");
        assert_eq!(t.options[ddl::THROUGHPUT_MODE], "manual");
        assert_eq!(t.indexes[0].columns, ["email"]);
        assert_eq!(t.indexes[1].columns, ["a", "b DESC"]);
        assert!(!t.options[ddl::INDEXING_POLICY].contains("composite"));
        let spatial = &t.indexes[2];
        assert_eq!((spatial.kind.as_deref(), spatial.columns.as_slice()), (Some("SPATIAL"), ["loc/*".to_string()].as_slice()));
        assert_eq!(spatial.options["types"], "[\"Point\",\"Polygon\"]");
        assert_eq!(t.indexes[3].kind.as_deref(), Some("FULLTEXT"));
        assert_eq!(t.indexes[4].options["type"], "quantizedFlat");
        assert!(!t.options[ddl::INDEXING_POLICY].contains("spatial"));
        let ddl = ddl::table_ddl(&t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        let stmts = parse_script(&ddl).unwrap();
        let Stmt::Admin(Admin::CreateContainer { body, .. }) = &stmts[0] else { panic!("{ddl}") };
        let mut expected = coll.clone();
        let e = expected.as_object_mut().unwrap();
        e.remove("id");
        e.remove("_rid");
        let mut body = body.clone();
        assert_eq!(body.as_object_mut().unwrap().remove(ddl::MANUAL_KEY), Some(json!(400)));
        assert_eq!(body, expected);
    }

    #[test]
    fn endpoints() {
        let mut c = ConnectionConfig { host: "acct.documents.azure.com".into(), ..Default::default() };
        assert_eq!(endpoint(&c), "https://acct.documents.azure.com");
        c.host = "https://localhost:8081/".into();
        assert_eq!(endpoint(&c), "https://localhost:8081");
    }
}

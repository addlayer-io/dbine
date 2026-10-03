//! Elasticsearch and OpenSearch over their REST API (plain reqwest).
//!
//! The editor speaks the Kibana Dev Tools console syntax (see [`console`]):
//! `GET /index/_search` plus a JSON body, several requests per script, and
//! `SELECT` / `SHOW` / `DESCRIBE` statements that go to the SQL endpoint.
//! The explorer lists indices, aliases and data streams right under the
//! connection (a cluster has no level in between).
//!
//! [`console`], [`json`], [`flatten`] and [`http`] are public because the
//! Solr driver reuses them.

mod backup;
pub mod console;
pub mod ddl;
pub mod flatten;
pub mod http;
pub mod json;
mod monitor;
mod permissions;
pub mod plan;
mod processes;
mod profiler;
mod security;
mod steps;
mod sync;
mod transfer;

use base64::Engine;
use console::{Command, Request};
use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    Driver, DriverInfo, Error, Family, Field, FieldKind, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef,
    QueryOutcome, ResultColumn, Result, ScriptError, Session, TableSchema,
};
use json::J;
use steps::Step;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const KIND_ALIAS: &str = "alias";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(Es::new(Flavor::Elastic)), Arc::new(Es::new(Flavor::OpenSearch)), Arc::new(Es::new(Flavor::OpenDistro))]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Elastic,
    OpenSearch,
    /// Open Distro for Elasticsearch: Elasticsearch OSS 7.10 plus AWS's
    /// plugins under `/_opendistro/…` (SQL, ISM, k-NN, security). No
    /// data streams, no `flat_object`; the rest is OpenSearch 1.x.
    OpenDistro,
}

struct Es {
    info: DriverInfo,
    /// OpenSearch-style mappings and settings (knn_vector, index.knn):
    /// OpenSearch and Open Distro.
    opensearch: bool,
    flavor: Flavor,
}

impl Es {
    fn security_api(&self) -> security::Api {
        match self.flavor {
            Flavor::Elastic => security::Api::Elastic,
            Flavor::OpenSearch => security::OPENSEARCH,
            Flavor::OpenDistro => security::OPENDISTRO,
        }
    }

    fn new(flavor: Flavor) -> Self {
        let opensearch = flavor != Flavor::Elastic;
        let mut fields = vec![
            Field::host().placeholder("localhost o https://mi-cluster:9200").help(
                "Nombre del servidor o URL completa. Si la URL no trae puerto, se usa el del campo Puerto.",
            ),
            Field::port().default_value("9200").help("En Elastic Cloud es 443 (DBine lo usa solo si queda 9200)."),
            Field::username(),
            Field::password(),
            Field::new("api_key", "API key", FieldKind::Password)
                .secret()
                .help("Clave codificada en Base64 (id:api_key). Si se completa, reemplaza al usuario y la contraseña."),
        ];
        if !opensearch {
            fields.push(
                Field::new("cloud_id", "Cloud ID", FieldKind::Text)
                    .placeholder("mi-deployment:ZXUtd2VzdC0x…")
                    .help("Elastic Cloud: si se completa, reemplaza al servidor y al puerto."),
            );
        }
        fields.extend([
            Field::encrypt(),
            Field::trust_cert(),
            Field::new("show_system", "Mostrar índices del sistema", FieldKind::Bool)
                .help("Incluye los índices, alias y data streams que empiezan con punto.")
                .advanced(),
            Field::read_only(),
        ]);
        let (id, name) = match flavor {
            Flavor::Elastic => ("elasticsearch", "Elasticsearch"),
            Flavor::OpenSearch => ("opensearch", "OpenSearch"),
            Flavor::OpenDistro => ("opendistro", "Open Distro for Elasticsearch"),
        };
        let mut object_kinds = vec![
            ObjectKindInfo::new(kinds::INDEX, "Índices", true, true, true),
            ObjectKindInfo::new(KIND_ALIAS, "Alias", true, true, true),
        ];
        if flavor != Flavor::OpenDistro {
            object_kinds.push(ObjectKindInfo::new(kinds::STREAM, "Data streams", true, true, true));
        }
        Self {
            info: DriverInfo {
                id,
                name,
                family: Family::Search,
                language: Language::Json,
                dialect: "",
                default_port: 9200,
                fields,
                databases_label: "",
                has_schemas: false,
                object_kinds,
            },
            opensearch,
            flavor,
        }
    }
}

/// Elastic Cloud deployments only answer on 443 (and 9243), over HTTPS; the
/// form's default 9200 would just time out. A cloud host with no port of its
/// own and the default one goes to `https://host:443`.
fn elastic_cloud_url(cfg: &ConnectionConfig) -> Option<String> {
    const CLOUD: [&str; 4] = [".elastic-cloud.com", ".cloud.es.io", ".found.io", ".elastic.cloud"];
    let host = cfg.host.trim().trim_end_matches('/');
    let rest = host.split_once("://").map_or(host, |(_, r)| r);
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let name = authority.to_ascii_lowercase();
    let cloud = CLOUD.iter().any(|d| name.ends_with(d));
    if !cloud || authority.contains(':') || !(cfg.port == 0 || cfg.port == 9200) {
        return None;
    }
    let path = if path.is_empty() { String::new() } else { format!("/{path}") };
    Some(format!("https://{authority}:443{path}"))
}

/// `https://<es-uuid>.<domain>[:port]` from an Elastic Cloud ID
/// (`name:base64(domain$es_uuid$kibana_uuid)`).
pub fn cloud_id_url(cloud_id: &str) -> Option<String> {
    let encoded = cloud_id.rsplit_once(':').map_or(cloud_id, |(_, b)| b).trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded.trim_end_matches('=')))
        .ok()?;
    let decoded = String::from_utf8(bytes).ok()?;
    let mut parts = decoded.split('$');
    let domain = parts.next()?.trim_end_matches('/');
    let es = parts.next().filter(|s| !s.is_empty())?;
    let (host, port) = domain.split_once(':').map_or((domain, None), |(h, p)| (h, Some(p)));
    Some(match port {
        Some(p) => format!("https://{es}.{host}:{p}"),
        None => format!("https://{es}.{host}"),
    })
}

/// An Elasticsearch / OpenSearch error body as one line:
/// `type: reason (caused by: …)`.
/// The error type of an error response (`index_not_found_exception`…),
/// or its HTTP status when the body has none: the error's code.
fn es_error_code(status: u16, body: &str) -> String {
    J::parse(body)
        .ok()
        .and_then(|j| j.at(&["error", "type"]).and_then(J::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

/// `line N:M` of a SQL error message: where in the statement the server
/// found the problem (1-based line and column).
fn sql_position(msg: &str) -> Option<(u32, u32)> {
    msg.match_indices("line ").find_map(|(i, _)| {
        let rest = &msg[i + 5..];
        let (l, rest) = rest.split_once(':')?;
        let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some((l.parse().ok().filter(|l| *l > 0)?, c.parse().ok()?))
    })
}

pub fn es_error_message(status: u16, body: &str) -> String {
    let fallback = || format!("HTTP {status}: {}", http::clip(body.trim(), 500));
    let Ok(j) = J::parse(body) else { return fallback() };
    let Some(e) = j.get("error") else { return fallback() };
    if let Some(s) = e.as_str() {
        return s.to_string();
    }
    let ty = e.get("type").and_then(J::as_str).unwrap_or("error");
    let reason = e.get("reason").map(J::text).unwrap_or_default();
    let mut msg = format!("{ty}: {reason}");
    let caused = e.at(&["caused_by", "reason"]).map(J::text);
    if let Some(c) = caused.filter(|c| !c.is_empty() && *c != reason) {
        msg.push_str(&format!(" (causa: {c})"));
    }
    if let Some(d) = e.get("details").map(J::text).filter(|d| !d.is_empty() && *d != reason) {
        msg.push_str(&format!(" — {d}"));
    }
    msg
}

/// Endpoints that only read and accept POST. `/_plugins/…` and
/// `/_opendistro/…` prefixes are skipped before checking.
const READ_POST_ENDPOINTS: &[&str] =
    &["_search", "_count", "_msearch", "_sql", "_mget", "_field_caps", "_validate", "_explain", "_async_search"];

/// Whether a request is allowed on a read-only connection: GET and HEAD
/// always, POST only to the read endpoints.
pub fn read_only_allows(req: &Request) -> bool {
    match req.method.as_str() {
        "GET" | "HEAD" => true,
        "POST" => req
            .segments()
            .into_iter()
            .find(|s| s.starts_with('_') && *s != "_plugins" && *s != "_opendistro")
            .is_some_and(|s| READ_POST_ENDPOINTS.contains(&s)),
        _ => false,
    }
}

/// `_bulk` answers 200 even when items fail (`"errors": true`): how many
/// failed and the first reason.
pub fn bulk_failure(resp: &J) -> Option<String> {
    if resp.get("errors").and_then(J::as_bool) != Some(true) {
        return None;
    }
    let items = resp.get("items").and_then(J::as_arr).unwrap_or(&[]);
    let errors: Vec<&J> = items.iter().filter_map(|i| i.as_obj()?.first()?.1.get("error")).collect();
    let first = errors.first().map(|e| es_error_message(400, &J::Obj(vec![("error".into(), (*e).clone())]).compact()));
    Some(format!(
        "_bulk: fallaron {} de {} operaciones. Primera: {}",
        errors.len(),
        items.len(),
        first.unwrap_or_default()
    ))
}

static OPAQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_opaque_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("dbine-{}-{nanos:x}-{}", std::process::id(), OPAQUE_SEQ.fetch_add(1, Ordering::Relaxed))
}

#[async_trait]
impl Driver for Es {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// As Kibana's console: a failed request doesn't stop the script (the tab's
    /// toggle overrides it).
    fn script_defaults(&self) -> dbine_driver::ScriptDefaults {
        dbine_driver::ScriptDefaults { continue_on_error: true, ..dbine_driver::ScriptDefaults::for_language(self.info().language) }
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// `_bulk` NDJSON sent directly (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// A cluster has no level between it and its indices: no databases to
    /// create or drop, and no foreign keys.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, processes: true, cancel_query: true, ..Capabilities::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        let mut d = ddl::designer(self.opensearch);
        if self.flavor == Flavor::OpenDistro {
            d.data_types.retain(|t| *t != "flat_object");
        }
        Some(d)
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        let mut t = ddl::templates(self.opensearch);
        if self.flavor == Flavor::OpenDistro {
            t.retain(|t| t.kind != kinds::STREAM);
            for x in &mut t {
                x.template = x.template.replace("/_plugins/", "/_opendistro/");
            }
        }
        t
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::index_ddl(table, parts, self.opensearch)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec(self.security_api()))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.security_api(), action)
    }

    /// Snapshots in a registered repository (the same API in all three).
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes, self.opensearch)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        ddl::bulk_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let base = match cfg.option("cloud_id").filter(|_| !self.opensearch) {
            Some(id) => cloud_id_url(id).ok_or_else(|| Error::Connect("El Cloud ID no es válido.".into()))?,
            None => elastic_cloud_url(cfg).unwrap_or_else(|| http::base_url(cfg, 9200)),
        };
        let auth = match cfg.option("api_key") {
            Some(k) => http::Auth::Token("ApiKey", k),
            None => http::auth_from(cfg),
        };
        let client = http::client(cfg, auth)?;
        let (status, body) =
            http::send(client.get(format!("{base}/")).timeout(Duration::from_secs(20))).await?;
        if status == 401 || status == 403 {
            return Err(Error::AuthFailed(es_error_message(status, &body)));
        }
        if status >= 400 {
            return Err(Error::Connect(es_error_message(status, &body)));
        }
        let root = J::parse(&body)
            .map_err(|_| Error::Connect(format!("El servidor no respondió como Elasticsearch: {}", http::clip(&body, 200))))?;
        let number = root.at(&["version", "number"]).map(J::text).unwrap_or_default();
        let is_os = root.at(&["version", "distribution"]).and_then(J::as_str) == Some("opensearch");
        let version = match (is_os, self.flavor) {
            (true, _) => format!("OpenSearch {number}"),
            (false, Flavor::OpenDistro) => format!("Open Distro for Elasticsearch (Elasticsearch {number})"),
            (false, _) => format!("Elasticsearch {number}"),
        };
        Ok(Box::new(EsSession {
            client,
            base,
            read_only: cfg.read_only,
            show_system: cfg.option("show_system").is_some_and(|v| v == "true" || v == "1"),
            opensearch: is_os || self.opensearch,
            opendistro: !is_os && self.flavor == Flavor::OpenDistro,
            version,
            opaque_id: new_opaque_id(),
            profiler: None,
            interrupted: Arc::default(),
        }))
    }
}

struct EsSession {
    client: reqwest::Client,
    base: String,
    read_only: bool,
    show_system: bool,
    opensearch: bool,
    /// SQL under `/_opendistro/_sql` instead of `/_plugins/_sql`.
    opendistro: bool,
    version: String,
    /// Sent as `X-Opaque-Id` so the interrupter can find our tasks.
    opaque_id: String,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// Set by the interrupter: a cancelled task fails its request with an
    /// ordinary HTTP error, so `execute` turns it into a cancel and runs
    /// nothing more, even on a run that continues on errors.
    interrupted: Arc<AtomicBool>,
}

fn rcol(name: &str, ty: &str) -> ResultColumn {
    ResultColumn { name: name.to_string(), type_name: ty.to_string() }
}

impl EsSession {
    fn request(&self, method: &str, path: &str) -> reqwest::RequestBuilder {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        self.client.request(m, format!("{}{path}", self.base)).header("X-Opaque-Id", &self.opaque_id)
    }

    /// Send, failing with the server's error on a 4xx/5xx.
    async fn call(&self, rb: reqwest::RequestBuilder) -> Result<String> {
        let (status, body) = http::send(rb).await?;
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &body)));
        }
        Ok(body)
    }

    async fn get_json(&self, path: &str) -> Result<J> {
        let body = self.call(self.request("GET", path)).await?;
        J::parse(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))
    }

    fn visible(&self, name: &str) -> bool {
        self.show_system || !name.starts_with('.')
    }

    async fn run_request(&self, req: &Request, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.read_only && !read_only_allows(req) {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: se bloqueó {} {}. Solo se permiten GET, HEAD y POST a _search, _count, _msearch, _sql, _mget, _field_caps, _validate y _explain.",
                req.method,
                req.path_only()
            )));
        }
        let segs = req.segments();
        let mut path = req.path.clone();
        let is_cat = segs.first() == Some(&"_cat");
        if is_cat && req.query_param("format").is_none() {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str("format=json");
        }
        let mut rb = self.request(&req.method, &path);
        if let Some(body) = &req.body {
            let ndjson = req.is_ndjson() || segs.iter().any(|s| matches!(*s, "_bulk" | "_msearch"));
            rb = if ndjson {
                let mut b = body.clone();
                b.push('\n');
                rb.header("Content-Type", "application/x-ndjson").body(b)
            } else {
                rb.header("Content-Type", "application/json").body(body.clone())
            };
        }
        let (status, text) = http::send(rb).await?;
        if req.method == "HEAD" {
            out.begin_result(vec![rcol("status", "")]);
            out.push_row(vec![status.into()], max_rows);
            return Ok(());
        }
        if status >= 400 {
            let msg = security::explain_error(req, es_error_message(status, &text));
            return Err(Error::Statement(Box::new(ScriptError::new(msg).with_code(es_error_code(status, &text)))));
        }
        let Ok(resp) = J::parse(&text) else {
            flatten::push_text(out, &text, max_rows);
            return Ok(());
        };
        if segs.contains(&"_bulk") {
            if let Some(msg) = bulk_failure(&resp) {
                return Err(Error::Query(msg));
            }
        }
        if let Some(responses) = resp.get("responses").and_then(J::as_arr) {
            for (i, r) in responses.iter().enumerate() {
                match r.get("error") {
                    Some(_) => out.warning(format!("Búsqueda {}: {}", i + 1, es_error_message(400, &r.compact()))),
                    None => flatten::push_search(out, r, max_rows),
                }
            }
        } else if resp.at(&["hits", "hits"]).is_some() {
            flatten::push_search(out, &resp, max_rows);
        } else if let (Some(docs), true) = (resp.get("docs").and_then(J::as_arr), segs.contains(&"_mget")) {
            flatten::push_docs(out, docs.iter().map(flatten::hit_doc), max_rows);
        } else if resp.get("columns").is_some() || resp.get("schema").is_some() {
            self.push_sql(&resp, max_rows, out).await?;
        } else {
            flatten::push_generic(out, &resp, max_rows);
        }
        Ok(())
    }

    /// `_validate/query?explain&rewrite` of a search's query: the rewritten
    /// Lucene query, without running the search.
    async fn validate(&self, target: &str, body: Option<&J>, q: Option<&str>) -> Result<J> {
        let mut path = if target.is_empty() { "/_validate/query".to_string() } else { format!("/{target}/_validate/query") };
        path.push_str("?explain=true&rewrite=true");
        if let Some(q) = q {
            path.push_str("&q=");
            path.push_str(q);
        }
        let mut rb = self.request("POST", &path);
        if let Some(query) = body.and_then(|b| b.get("query")) {
            let b = J::Obj(vec![("query".into(), query.clone())]);
            rb = rb.header("Content-Type", "application/json").body(b.compact());
        }
        let text = self.call(rb).await?;
        J::parse(&text).map_err(|e| Error::Query(format!("Respuesta inesperada de _validate: {e}")))
    }

    /// Run a search with `"profile": true`.
    async fn profile(&self, method: &str, path: &str, body: Option<&J>) -> Result<J> {
        let mut b = match body {
            Some(J::Obj(o)) => o.clone(),
            _ => Vec::new(),
        };
        b.retain(|(k, _)| k != "profile");
        b.push(("profile".into(), J::Bool(true)));
        let rb = self.request(method, path).header("Content-Type", "application/json").body(J::Obj(b).compact());
        let text = self.call(rb).await?;
        J::parse(&text).map_err(|e| Error::Query(format!("Respuesta inesperada de _search: {e}")))
    }

    /// Plan of a SQL statement. Elasticsearch: `_sql/translate` to Query
    /// DSL, planned as a search on the `FROM` index. OpenSearch: its
    /// `_explain` operator tree, with the pushed-down searches planned
    /// under the index scans.
    async fn explain_sql(&self, stmt: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let select = stmt.trim_start().get(..6).is_some_and(|w| w.eq_ignore_ascii_case("select"));
        if analyze {
            self.run_sql(stmt, max_rows, out).await?;
        }
        if !select {
            out.info(format!("`{}`: solo las consultas SELECT tienen plan de ejecución.", plan::clip(stmt, 80)));
            return Ok(());
        }
        let sql_body = serde_json::json!({ "query": stmt });
        if self.opensearch {
            let path = if self.opendistro { "/_opendistro/_sql/_explain" } else { "/_plugins/_sql/_explain" };
            let text = self.call(self.request("POST", path).json(&sql_body)).await?;
            let e = J::parse(&text).map_err(|e| Error::Query(format!("Respuesta inesperada de _explain: {e}")))?;
            let mut scans = Vec::new();
            plan::os_sql_plan(stmt, &e, &mut |i, d| {
                scans.push((i.to_string(), d.clone()));
                None
            });
            let mut subs = Vec::new();
            for (index, dsl) in &scans {
                subs.push(self.search_plan(stmt, index, "POST", &format!("/{index}/_search"), Some(dsl), analyze).await?.0.root);
            }
            let mut it = subs.into_iter();
            let mut p = plan::os_sql_plan(stmt, &e, &mut |_, _| it.next());
            p.actual = analyze && !scans.is_empty();
            out.plans.push(p);
        } else {
            let text = self.call(self.request("POST", "/_sql/translate").json(&sql_body)).await?;
            let dsl = J::parse(&text).map_err(|e| Error::Query(format!("Respuesta inesperada de _sql/translate: {e}")))?;
            let search = match plan::sql_from(stmt) {
                Some(index) => {
                    Some(self.search_plan(stmt, &index, "POST", &format!("/{index}/_search"), Some(&dsl), analyze).await?.0)
                }
                None => None,
            };
            out.plans.push(plan::sql_plan(stmt, &dsl, search));
        }
        Ok(())
    }

    /// The plan of a search: estimated (validate) or actual (profiled run,
    /// whose response comes back too).
    async fn search_plan(
        &self,
        statement: &str,
        target: &str,
        method: &str,
        path: &str,
        body: Option<&J>,
        analyze: bool,
    ) -> Result<(dbine_driver::Plan, Option<J>)> {
        if analyze {
            let resp = self.profile(method, path, body).await?;
            Ok((plan::profiled_search(statement, target, body, &resp), Some(resp)))
        } else {
            let q = path.split_once('?').and_then(|(_, qs)| {
                qs.split('&').find_map(|kv| kv.strip_prefix("q="))
            });
            let v = self.validate(target, body, q).await?;
            Ok((plan::estimated_search(statement, target, body, &v), None))
        }
    }

    fn sql_path(&self) -> (&'static str, &'static str) {
        if self.opendistro {
            ("/_opendistro/_sql", "/_opendistro/_sql/close")
        } else if self.opensearch {
            ("/_plugins/_sql", "/_plugins/_sql/close")
        } else {
            ("/_sql?format=json", "/_sql/close")
        }
    }

    async fn run_sql(&self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let (path, _) = self.sql_path();
        let fetch = (max_rows + 1).clamp(1, 1000);
        // OpenSearch wants a pattern after SHOW TABLES; Elasticsearch doesn't.
        let bare_show = stmt.split_whitespace().map(str::to_ascii_lowercase).collect::<Vec<_>>() == ["show", "tables"];
        let stmt = if self.opensearch && bare_show { "SHOW TABLES LIKE %" } else { stmt };
        let mut body = serde_json::json!({ "query": stmt });
        // OpenSearch only pages SELECTs; its SHOW / DESCRIBE reject fetch_size.
        if !self.opensearch || stmt.trim_start().get(..6).is_some_and(|w| w.eq_ignore_ascii_case("select")) {
            body["fetch_size"] = fetch.into();
        }
        let (status, text) = http::send(self.request("POST", path).json(&body)).await?;
        if status >= 400 {
            let msg = es_error_message(status, &text);
            let mut se = ScriptError::new(msg.clone()).with_code(es_error_code(status, &text));
            if let Some((line, col)) = sql_position(&msg) {
                se = se.at_line(line).at_offset(steps::offset_of(stmt, line, col));
            }
            return Err(Error::Statement(Box::new(se)));
        }
        let resp = J::parse(&text).map_err(|e| Error::Query(format!("Respuesta SQL inesperada: {e}")))?;
        self.push_sql(&resp, max_rows, out).await
    }

    /// A SQL response (Elasticsearch `columns`/`rows` or OpenSearch
    /// `schema`/`datarows`), following the cursor until past `max_rows`.
    async fn push_sql(&self, first: &J, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let cols = first.get("columns").or_else(|| first.get("schema")).and_then(J::as_arr).unwrap_or(&[]);
        out.begin_result(
            cols.iter()
                .map(|c| {
                    let name = c.get("alias").or_else(|| c.get("name")).map(J::text).unwrap_or_default();
                    rcol(&name, &c.get("type").map(J::text).unwrap_or_default())
                })
                .collect(),
        );
        let (path, close) = self.sql_path();
        let mut page = first.clone();
        loop {
            let rows = page.get("rows").or_else(|| page.get("datarows")).and_then(J::as_arr).unwrap_or(&[]);
            for r in rows {
                out.push_row(r.as_arr().unwrap_or(&[]).iter().map(J::cell).collect(), max_rows);
            }
            let Some(cursor) = page.get("cursor").and_then(J::as_str).filter(|c| !c.is_empty()).map(str::to_string) else {
                break;
            };
            let done = out.results.last().is_some_and(|r| r.truncated) || rows.is_empty();
            if done {
                // Free the server-side cursor; failing to is harmless.
                let _ = http::send(self.request("POST", close).json(&serde_json::json!({ "cursor": cursor }))).await;
                break;
            }
            let text = self.call(self.request("POST", path).json(&serde_json::json!({ "cursor": cursor }))).await?;
            page = J::parse(&text).map_err(|e| Error::Query(format!("Respuesta SQL inesperada: {e}")))?;
        }
        Ok(())
    }
}

#[async_trait]
impl Session for EsSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(self.version.clone())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let obj = |kind: &str, name: String| DbObject { kind: kind.into(), schema: None, name, parent: None };
        let mut out = Vec::new();
        let indices = self.get_json("/_cat/indices?format=json&h=index&s=index").await?;
        for i in indices.as_arr().unwrap_or(&[]) {
            if let Some(n) = i.get("index").and_then(J::as_str).filter(|n| self.visible(n)) {
                out.push(obj(kinds::INDEX, n.to_string()));
            }
        }
        if let Ok(aliases) = self.get_json("/_cat/aliases?format=json&h=alias&s=alias").await {
            let mut seen: Vec<&str> = Vec::new();
            for a in aliases.as_arr().unwrap_or(&[]) {
                if let Some(n) = a.get("alias").and_then(J::as_str).filter(|n| self.visible(n) && !seen.contains(n)) {
                    seen.push(n);
                    out.push(obj(KIND_ALIAS, n.to_string()));
                }
            }
        }
        // Data streams: absent on old clusters, may be forbidden; skip then.
        if let Ok(ds) = self.get_json("/_data_stream").await {
            let mut names: Vec<String> = ds
                .get("data_streams")
                .and_then(J::as_arr)
                .unwrap_or(&[])
                .iter()
                .filter_map(|d| d.get("name").and_then(J::as_str))
                .filter(|n| self.visible(n))
                .map(str::to_string)
                .collect();
            names.sort();
            out.extend(names.into_iter().map(|n| obj(kinds::STREAM, n)));
        }
        Ok(out)
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let m = self.get_json(&format!("/{}/_mapping", o.name)).await?;
        let mut fields: Vec<(String, String)> = Vec::new();
        for (_, idx) in m.as_obj().into_iter().flatten() {
            for f in flatten::mapping_fields(idx.get("mappings").unwrap_or(&J::Null)) {
                if !fields.iter().any(|(n, _)| *n == f.0) {
                    fields.push(f);
                }
            }
        }
        Ok(fields
            .into_iter()
            .map(|(name, data_type)| ColumnInfo {
                name,
                data_type,
                nullable: true,
                primary_key: false,
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
        let path = if o.kind == kinds::STREAM { format!("/_data_stream/{}", o.name) } else { format!("/{}", o.name) };
        Ok(Some(self.get_json(&path).await?.pretty()))
    }

    /// Every visible index with its mapping (dotted sub-fields), shards,
    /// replicas, refresh interval, aliases and `dynamic` as designer
    /// options, and its `_meta.description` as comment. Data streams'
    /// backing indices are hidden (they start with a dot).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let path = if self.show_system { "/_all?flat_settings=true&expand_wildcards=all" } else { "/_all?flat_settings=true" };
        let all = self.get_json(path).await?;
        let mut out: Vec<TableSchema> = all
            .as_obj()
            .into_iter()
            .flatten()
            .filter(|(name, _)| self.visible(name))
            .map(|(name, idx)| ddl::index_schema(name, idx, self.opensearch))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn browse_query(&self, o: &ObjectRef, limit: u32) -> String {
        format!("GET /{}/_search\n{{\n  \"size\": {limit},\n  \"query\": {{ \"match_all\": {{}} }}\n}}", o.name)
    }

    /// Requests and SQL statements one after another, as the Dev Tools
    /// console sends a selection: a failing one stops the script unless the
    /// editor run continues on errors, as the console does (see
    /// `Step::end`). A line that's neither runs nothing.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let cmds =
            console::parse_located(text).map_err(|e| Error::from(ScriptError::new(e.message).at_offset(e.offset).at_line(e.line)))?;
        if cmds.is_empty() {
            return Err(Error::Query("No hay ninguna petición para ejecutar.".into()));
        }
        let own = out.current_statement.is_none();
        self.interrupted.store(false, Ordering::SeqCst);
        for (i, c) in cmds.iter().enumerate() {
            if self.interrupted.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let step = Step::start(out, own, i, c.offset, c.line);
            let r = match &c.command {
                Command::Http(r) => self.run_request(r, max_rows, out).await,
                Command::Sql(s) => self.run_sql(s, max_rows, out).await,
            };
            let r = r.map_err(|e| if self.interrupted.load(Ordering::SeqCst) { Error::Cancelled } else { e });
            step.end(out, r)?;
        }
        Ok(())
    }

    /// `_search` requests and SQL `SELECT`s get plans (see [`plan`]); other
    /// requests have none: with `analyze` they run as in `execute`,
    /// without it they're skipped.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let cmds = console::parse(text).map_err(Error::Query)?;
        if cmds.is_empty() {
            return Err(Error::Query("No hay ninguna petición para ejecutar.".into()));
        }
        for c in &cmds {
            let r = match c {
                Command::Sql(s) => {
                    self.explain_sql(s, analyze, max_rows, out).await?;
                    continue;
                }
                Command::Http(r) => r,
            };
            let label = format!("{} {}", r.method, r.path);
            let segs = r.segments();
            let is_search = matches!(r.method.as_str(), "GET" | "POST") && segs.last() == Some(&"_search");
            if !is_search {
                out.info(format!("`{label}`: solo las búsquedas (_search) y las consultas SQL tienen plan de ejecución."));
                if analyze {
                    self.run_request(r, max_rows, out).await?;
                }
                continue;
            }
            let target = segs[..segs.len() - 1].join("/");
            let body = match r.body.as_deref() {
                Some(b) => Some(J::parse(b).map_err(|e| Error::Query(format!("El cuerpo de {label} no es JSON válido: {e}")))?),
                None => None,
            };
            let statement = match &r.body {
                Some(b) => format!("{label}\n{b}"),
                None => label.clone(),
            };
            let (p, resp) = self.search_plan(&statement, &target, &r.method, &r.path, body.as_ref(), analyze).await?;
            if let Some(resp) = resp {
                flatten::push_search(out, &resp, max_rows);
            }
            out.plans.push(p);
        }
        Ok(())
    }

    /// Every snapshot of every registered repository, newest first.
    async fn backups(&mut self, _database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        let parse = |body: String| {
            serde_json::from_str::<serde_json::Value>(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))
        };
        let repos = backup::repositories(&parse(self.call(self.request("GET", "/_snapshot")).await?)?);
        let mut all = Vec::new();
        for (repo, ty) in repos {
            let path = format!("/_snapshot/{}/_all", ddl::path_segment(&repo));
            let body = parse(self.call(self.request("GET", &path)).await?)?;
            all.extend(backup::entries(&repo, &ty, &body));
        }
        all.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        Ok(all.into_iter().map(|(_, e)| e).collect())
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        s.info.push(("Versión".into(), self.version.clone()));
        let mut refused = Vec::new();
        match self.get_json("/_cluster/health").await {
            Ok(h) => monitor::from_health(&mut s, &h),
            Err(e) => refused.push(format!("_cluster/health ({e})")),
        }
        match self.get_json("/_nodes/stats/os,process,jvm,indices,fs,transport,http,thread_pool").await {
            Ok(st) => monitor::from_node_stats(&mut s, &st),
            Err(e) => refused.push(format!("_nodes/stats ({e})")),
        }
        match self.get_json("/_tasks?detailed=true").await {
            Ok(t) => s.tables.insert(0, monitor::tasks(&t)),
            Err(e) => refused.push(format!("_tasks ({e})")),
        }
        let cat = "/_cat/indices?format=json&bytes=b&h=index,health,status,pri,rep,docs.count,docs.deleted,store.size,pri.store.size";
        match self.get_json(cat).await {
            Ok(c) => monitor::indices(&mut s, &c, self.show_system),
            Err(e) => refused.push(format!("_cat/indices ({e})")),
        }
        match self.get_json("/_cluster/pending_tasks").await {
            Ok(p) => s.tables.push(monitor::pending(&p)),
            Err(e) => refused.push(format!("_cluster/pending_tasks ({e})")),
        }
        if !refused.is_empty() {
            s.notes.push(format!(
                "No se pudieron leer (hace falta el privilegio de cluster «monitor»): {}.",
                refused.join("; ")
            ));
        }
        s.notes.push("La red que se informa es la de transporte entre nodos; las conexiones HTTP no informan bytes.".into());
        if !s.metrics.iter().any(|m| m.key == "disk_read" || m.key == "disk_write") {
            s.notes.push("El servidor no informa la E/S de disco (fs.io_stats solo existe en Linux con acceso a /proc/diskstats).".into());
        }
        Ok(s)
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        let rb = self.request("GET", processes::LIST_PATH).timeout(processes::QUERY_LIMIT);
        let body = self.call(rb).await?;
        let tasks = J::parse(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))?;
        Ok(processes::rows(&tasks, &self.opaque_id))
    }

    /// Requests are tasks: cancelling one is `POST _tasks/<id>/_cancel`.
    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cancelar tareas.".into()));
        }
        let id = id.trim();
        if !processes::valid_task_id(id) {
            return Err(Error::Query(format!("«{id}» no es un id de tarea (nodo:número)")));
        }
        let (status, body) = http::send(self.request("GET", &format!("/_tasks/{id}"))).await?;
        if status >= 400 && status != 404 {
            return Err(Error::Query(es_error_message(status, &body)));
        }
        processes::check_cancel(id, status, &body, &self.opaque_id)?;
        let body = self.call(self.request("POST", &format!("/_tasks/{id}/_cancel"))).await?;
        match processes::cancel_failure(&body) {
            Some(why) => Err(Error::Query(format!("no se pudo cancelar la tarea {id}: {why}"))),
            None => Ok(()),
        }
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (sampler, started) = profiler::start(self, opts).await?;
        self.profiler = Some(sampler);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut sampler = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::poll(self, &mut sampler).await;
        self.profiler = Some(sampler);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
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

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (client, base, id) = (self.client.clone(), self.base.clone(), self.opaque_id.clone());
        let interrupted = self.interrupted.clone();
        Some(Arc::new(move || {
            interrupted.store(true, Ordering::SeqCst);
            let (client, base, id) = (client.clone(), base.clone(), id.clone());
            // Called from any thread, maybe outside a runtime: use our own.
            std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                rt.block_on(cancel_tasks(&client, &base, &id));
            });
        }))
    }

    /// Cluster privileges (see `permissions`); a cluster has no databases.
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self).await
    }
}

/// Cancel the running tasks that carry our `X-Opaque-Id`.
async fn cancel_tasks(client: &reqwest::Client, base: &str, opaque_id: &str) {
    let Ok((200, body)) = http::send(client.get(format!("{base}/_tasks?detailed=false"))).await else { return };
    let Ok(tasks) = J::parse(&body) else { return };
    for (_, node) in tasks.get("nodes").and_then(J::as_obj).into_iter().flatten() {
        for (task_id, t) in node.get("tasks").and_then(J::as_obj).into_iter().flatten() {
            let ours = t.at(&["headers", "X-Opaque-Id"]).and_then(J::as_str) == Some(opaque_id);
            if ours && t.get("cancellable").and_then(J::as_bool).unwrap_or(true) {
                let r = http::send(client.post(format!("{base}/_tasks/{task_id}/_cancel"))).await;
                tracing::debug!(task_id, ok = r.is_ok(), "cancel search task");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_error_positions() {
        let m = "verification_exception: Found 1 problem\nline 2:8: Unknown column [nope]";
        assert_eq!(sql_position(m), Some((2, 8)));
        assert_eq!(sql_position("parsing_exception: line 1:15: mismatched input"), Some((1, 15)));
        assert_eq!(sql_position("no position here"), None);
        assert_eq!(steps::offset_of("SELECT a\nFROM t WHERE", 2, 6), 14);
    }

    #[test]
    fn error_codes() {
        assert_eq!(es_error_code(404, r#"{"error":{"type":"index_not_found_exception","reason":"x"},"status":404}"#), "index_not_found_exception");
        assert_eq!(es_error_code(502, "Bad gateway"), "HTTP 502");
    }

    fn req(method: &str, path: &str) -> Request {
        Request { method: method.into(), path: path.into(), body: None }
    }

    #[test]
    fn read_only_whitelist() {
        assert!(read_only_allows(&req("GET", "/idx/_doc/1")));
        assert!(read_only_allows(&req("POST", "/idx/_search?size=1")));
        assert!(read_only_allows(&req("POST", "/_sql?format=json")));
        assert!(read_only_allows(&req("POST", "/_plugins/_sql")));
        assert!(read_only_allows(&req("POST", "/_search/scroll")));
        assert!(!read_only_allows(&req("POST", "/idx/_doc")));
        assert!(!read_only_allows(&req("POST", "/idx/_doc/_search")));
        assert!(!read_only_allows(&req("POST", "/_bulk")));
        assert!(!read_only_allows(&req("PUT", "/idx")));
        assert!(!read_only_allows(&req("DELETE", "/idx")));
        assert!(!read_only_allows(&req("POST", "/idx/_delete_by_query")));
        assert!(!read_only_allows(&req("POST", "/idx")));
    }

    #[test]
    fn errors_are_one_line() {
        let body = r#"{"error":{"root_cause":[],"type":"index_not_found_exception","reason":"no such index [x]"},"status":404}"#;
        assert_eq!(es_error_message(404, body), "index_not_found_exception: no such index [x]");
        let body = r#"{"error":{"type":"parsing_exception","reason":"bad","caused_by":{"reason":"deeper"}}}"#;
        assert_eq!(es_error_message(400, body), "parsing_exception: bad (causa: deeper)");
        assert_eq!(es_error_message(502, "Bad gateway"), "HTTP 502: Bad gateway");
    }

    #[test]
    fn bulk_failures_are_errors() {
        let ok = J::parse(r#"{"errors":false,"items":[{"index":{"status":201}}]}"#).unwrap();
        assert_eq!(bulk_failure(&ok), None);
        let bad = J::parse(
            r#"{"errors":true,"items":[{"index":{"status":201}},{"index":{"status":400,"error":{"type":"mapper_parsing_exception","reason":"bad year"}}}]}"#,
        )
        .unwrap();
        assert_eq!(bulk_failure(&bad).unwrap(), "_bulk: fallaron 1 de 2 operaciones. Primera: mapper_parsing_exception: bad year");
    }

    #[test]
    fn cloud_id_decodes() {
        let enc = base64::engine::general_purpose::STANDARD.encode("us-east-1.aws.found.io:443$abc123$kib456");
        assert_eq!(cloud_id_url(&format!("my-dep:{enc}")).as_deref(), Some("https://abc123.us-east-1.aws.found.io:443"));
        let enc = base64::engine::general_purpose::STANDARD.encode("eu.example.io$es1$kb1");
        assert_eq!(cloud_id_url(&format!("x:{enc}")).as_deref(), Some("https://es1.eu.example.io"));
        assert_eq!(cloud_id_url("garbage"), None);
    }

    #[test]
    fn open_distro_variant() {
        let ids: Vec<&str> = drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["elasticsearch", "opensearch", "opendistro"]);
        let d = drivers().into_iter().find(|d| d.info().id == "opendistro").unwrap();
        assert!(d.capabilities().monitor);
        assert!(!d.info().object_kinds.iter().any(|k| k.id == kinds::STREAM));
        let t = d.create_templates();
        assert!(t.iter().all(|t| !t.template.contains("/_plugins/") && t.kind != kinds::STREAM));
        assert!(t.iter().any(|t| t.template.contains("/_opendistro/_ism/")));
        assert!(!d.designer().unwrap().data_types.contains(&"flat_object"));
    }

    #[test]
    fn elastic_cloud_uses_https_443() {
        let mut cfg = ConnectionConfig { host: "mi-deploy.es.eastus.azure.elastic-cloud.com".into(), port: 9200, ..Default::default() };
        assert_eq!(elastic_cloud_url(&cfg).as_deref(), Some("https://mi-deploy.es.eastus.azure.elastic-cloud.com:443"));
        cfg.host = "https://x.us-east-1.aws.found.io/".into();
        cfg.port = 0;
        assert_eq!(elastic_cloud_url(&cfg).as_deref(), Some("https://x.us-east-1.aws.found.io:443"));
        // A port of its own, another port, or not the cloud: left alone.
        cfg.host = "https://x.cloud.es.io:9243".into();
        assert_eq!(elastic_cloud_url(&cfg), None);
        cfg.host = "x.cloud.es.io".into();
        cfg.port = 9243;
        assert_eq!(elastic_cloud_url(&cfg), None);
        cfg.host = "es.local".into();
        cfg.port = 9200;
        assert_eq!(elastic_cloud_url(&cfg), None);
    }
}

//! Apache Solr over its REST API (plain reqwest).
//!
//! Same console syntax as Elasticsearch (`GET /solr/<core>/select?q=*:*`;
//! the `/solr` prefix may be left out). `SELECT` statements go to Solr's
//! parallel SQL handler, which needs SolrCloud (and the `sql` module).
//! The explorer lists the cores (standalone) or collections (SolrCloud)
//! right under the connection.
//!
//! DBine adds two requests of its own, `PUT /solr/<name>` and
//! `DELETE /solr/<name>` (see [`QUERY_HELP`]), so the designer's scripts
//! create and drop a collection the same way on both modes.

mod ddl;
mod sync;
mod monitor;
mod plan;
mod permissions;
mod security;
mod backup;
pub mod transfer;

use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    Driver, DriverInfo, Error, Family, Field, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef, QueryOutcome, ResultColumn, Result,
    Session, TableSchema,
};
use dbine_driver_elasticsearch::console::{self, Command, Request};
use dbine_driver_elasticsearch::flatten;
use dbine_driver_elasticsearch::http;
use dbine_driver_elasticsearch::json::{Obj, J};
use std::sync::Arc;
use std::time::Duration;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(Solr::new())]
}

struct Solr {
    info: DriverInfo,
}

impl Solr {
    fn new() -> Self {
        Self {
            info: DriverInfo {
                id: "solr",
                name: "Apache Solr",
                family: Family::Search,
                language: Language::Json,
                dialect: "",
                default_port: 8983,
                fields: vec![
                    Field::host().placeholder("localhost o https://mi-solr:8983").help(
                        "Nombre del servidor o URL completa. Si la URL no trae puerto, se usa el del campo Puerto.",
                    ),
                    Field::port().default_value("8983"),
                    Field::username(),
                    Field::password(),
                    Field::encrypt(),
                    Field::trust_cert(),
                    Field::read_only(),
                ],
                databases_label: "",
                has_schemas: false,
                object_kinds: vec![ObjectKindInfo::new(kinds::COLLECTION, "Colecciones", true, true, true)],
            },
        }
    }
}

pub const QUERY_HELP: &str = "Sintaxis de consola: una petición por bloque, `MÉTODO /ruta` y un cuerpo JSON opcional; \
los bloques se separan con una línea en blanco. El prefijo /solr se puede omitir.\n\
  GET /solr/<colección>/select?q=*:*&rows=10\n\
  POST /solr/<colección>/update?commit=true   + [ {\"id\": \"1\", …} ]\n\
  POST /solr/<colección>/schema               + {\"add-field\": {\"name\": \"x\", \"type\": \"string\"}}\n\
  SELECT … FROM <colección>                   (Solr SQL, solo SolrCloud)\n\
Extensiones de DBine (funcionan igual en standalone y en SolrCloud):\n\
  PUT /solr/<nombre>   + {\"configSet\": \"_default\", \"numShards\": 1, \"replicationFactor\": 1} (cuerpo opcional)\n\
      crea un core (standalone, CoreAdmin CREATE) o una colección (SolrCloud, Collections API CREATE).\n\
      Con ?if_not_exists=true no hace nada si ya existe.\n\
  DELETE /solr/<nombre>\n\
      borra el core con sus datos (UNLOAD) o la colección (DELETE). Con ?if_exists=true no falla si no existe.";

/// A Solr error body as one line.
pub fn solr_error_message(status: u16, body: &str) -> String {
    let fallback = || format!("HTTP {status}: {}", http::clip(body.trim(), 500));
    let Ok(j) = J::parse(body) else { return fallback() };
    let Some(e) = j.get("error") else { return fallback() };
    if let Some(m) = e.get("msg").map(J::text).filter(|m| !m.is_empty()) {
        return m;
    }
    e.get("trace").map(J::text).and_then(|t| t.lines().next().map(str::to_string)).unwrap_or_else(fallback)
}

/// `/solr/…` for a console path; `/api/…` (v2) as is.
pub fn normalize_path(path: &str) -> String {
    let p = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let head = p.split(['/', '?']).nth(1).unwrap_or("");
    if head == "solr" || head == "api" {
        p
    } else {
        format!("/solr{p}")
    }
}

fn param_is(req: &Request, key: &str, allowed: &[&str], default_ok: bool) -> bool {
    match req.query_param(key) {
        None => default_ok,
        Some(v) => allowed.iter().any(|a| a.eq_ignore_ascii_case(v)),
    }
}

/// Handlers of a core / collection that only read. `select`, `query` and
/// `sql` also accept POST (JSON Request API, form bodies).
const READ_HANDLERS: &[&str] = &["select", "query", "get", "terms", "suggest", "spell", "tvrh", "browse", "export", "sql"];
const READ_CORE_ADMIN: &[&str] = &["luke", "ping", "system", "mbeans", "file", "segments", "plugins"];
const READ_ADMIN: &[&str] = &["info", "metrics", "zookeeper", "health"];

/// Whether a request (with a normalized path) is allowed on a read-only
/// connection: search handlers, and admin endpoints only for their read
/// actions. Solr writes through GET too, so the method alone isn't enough.
pub fn read_only_allows(req: &Request) -> bool {
    let get = matches!(req.method.as_str(), "GET" | "HEAD");
    let post = req.method == "POST";
    let segs = req.segments();
    match segs.as_slice() {
        ["api", ..] => get,
        ["solr", "admin", "cores", ..] => (get || post) && param_is(req, "action", &["STATUS"], true),
        ["solr", "admin", "collections", ..] => {
            (get || post)
                && param_is(
                    req,
                    "action",
                    &["LIST", "CLUSTERSTATUS", "LISTALIASES", "COLSTATUS", "REQUESTSTATUS", "OVERSEERSTATUS", "LISTSNAPSHOTS"],
                    false,
                )
        }
        ["solr", "admin", "configs", ..] => get && param_is(req, "action", &["LIST"], false),
        ["solr", "admin", what, ..] => get && READ_ADMIN.contains(what),
        ["solr", _core, "admin", what, ..] => get && READ_CORE_ADMIN.contains(what),
        ["solr", _core, "schema", ..] | ["solr", _core, "config", ..] => get,
        ["solr", _core, handler, ..] => (get || post) && READ_HANDLERS.contains(handler),
        _ => false,
    }
}

/// The collection a SQL statement reads (`… FROM <name> …`).
pub fn sql_collection(stmt: &str) -> Option<String> {
    let mut words = stmt.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '(' || c == ')');
    while let Some(w) = words.next() {
        if w.eq_ignore_ascii_case("from") {
            let name = words.find(|w| !w.is_empty())?;
            let name = name.trim_matches(|c| c == '"' || c == '`' || c == '\'');
            return (!name.is_empty()).then(|| name.to_string());
        }
    }
    None
}

fn has_limit(stmt: &str) -> bool {
    stmt.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| w.eq_ignore_ascii_case("limit"))
}

/// Search handlers whose requests get a plan.
const PLAN_HANDLERS: &[&str] = &["select", "query", "browse"];

/// `(core, handler)` of a search request (normalized path) that gets a plan.
pub fn plan_target(req: &Request) -> Option<(String, String)> {
    if !matches!(req.method.as_str(), "GET" | "POST") {
        return None;
    }
    match req.segments().as_slice() {
        ["solr", core, handler] if PLAN_HANDLERS.contains(handler) => Some((core.to_string(), handler.to_string())),
        _ => None,
    }
}

/// The request with the `debug` parameters of a plan: estimated asks for
/// no rows (`rows=0`, JSON `limit: 0`) and the parsed query; actual keeps
/// the rows and adds the component timing.
pub fn debug_request(req: &Request, analyze: bool) -> Request {
    let (path, qs) = req.path.split_once('?').unwrap_or((&req.path, ""));
    let mut params: Vec<&str> = qs.split('&').filter(|p| !p.is_empty()).collect();
    if !analyze {
        params.retain(|p| p.split('=').next() != Some("rows"));
    }
    params.extend(if analyze { &["debug=query", "debug=timing", "wt=json"][..] } else { &["rows=0", "debug=query", "wt=json"][..] });
    let mut body = req.body.clone();
    if !analyze {
        if let Some(J::Obj(mut o)) = body.as_deref().and_then(|b| J::parse(b).ok()) {
            if let Some(l) = o.iter_mut().find(|(k, _)| k == "limit") {
                l.1 = J::Num(0.into());
                body = Some(J::Obj(o).compact());
            }
        }
    }
    Request { path: format!("{path}?{}", params.join("&")), body, method: req.method.clone() }
}

fn rcol(name: &str) -> ResultColumn {
    ResultColumn { name: name.to_string(), type_name: String::new() }
}

#[async_trait]
impl Driver for Solr {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Cores / collections live right under the server: no databases to
    /// create or drop, and no foreign keys.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Capabilities::default() }
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

    /// `/update` with JSON document arrays sent directly (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    /// Basic authentication and rule-based authorization (security.json).
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Collections API backups (SolrCloud) and replication-handler
    /// snapshots (standalone).
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::collection_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        ddl::update_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::atomic_update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let base = http::base_url(cfg, 8983);
        let client = http::client(cfg, http::auth_from(cfg))?;
        let url = format!("{base}/solr/admin/info/system?wt=json");
        let (status, body) = http::send(client.get(url).timeout(Duration::from_secs(20))).await?;
        if status == 401 || status == 403 {
            return Err(Error::AuthFailed(solr_error_message(status, &body)));
        }
        if status >= 400 {
            return Err(Error::Connect(solr_error_message(status, &body)));
        }
        let info = J::parse(&body)
            .map_err(|_| Error::Connect(format!("El servidor no respondió como Solr: {}", http::clip(&body, 200))))?;
        let cloud = info.get("mode").and_then(J::as_str) == Some("solrcloud");
        let number = info.at(&["lucene", "solr-spec-version"]).map(J::text).unwrap_or_default();
        let version = format!("Apache Solr {number}{}", if cloud { " (SolrCloud)" } else { "" });
        Ok(Box::new(SolrSession { client, base, read_only: cfg.read_only, cloud, version }))
    }
}

struct SolrSession {
    client: reqwest::Client,
    base: String,
    read_only: bool,
    cloud: bool,
    version: String,
}

impl SolrSession {
    async fn call(&self, rb: reqwest::RequestBuilder) -> Result<String> {
        let (status, body) = http::send(rb).await?;
        if status >= 400 {
            return Err(Error::Query(solr_error_message(status, &body)));
        }
        Ok(body)
    }

    async fn get_json(&self, path: &str) -> Result<J> {
        let body = self.call(self.client.get(format!("{}{path}", self.base))).await?;
        J::parse(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))
    }

    async fn run_request(&self, req: &Request, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let req = Request { path: normalize_path(&req.path), ..req.clone() };
        if self.read_only && !read_only_allows(&req) {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: se bloqueó {} {}. Solo se permiten búsquedas (select, query, get, sql…) y consultas de administración.",
                req.method,
                req.path_only()
            )));
        }
        if let Some(name) = collection_command(&req) {
            return self.manage_collection(&req, name, out).await;
        }
        let m = reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rb = self.client.request(m, format!("{}{}", self.base, req.path));
        if let Some(body) = &req.body {
            let ct = if body.starts_with('{') || body.starts_with('[') {
                "application/json"
            } else {
                "application/x-www-form-urlencoded"
            };
            rb = rb.header("Content-Type", ct).body(body.clone());
        }
        let (status, text) = http::send(rb).await?;
        if req.method == "HEAD" {
            out.begin_result(vec![rcol("status")]);
            out.push_row(vec![status.into()], max_rows);
            return Ok(());
        }
        if status >= 400 {
            return Err(Error::Query(solr_error_message(status, &text)));
        }
        match J::parse(&text) {
            Ok(resp) => push_response(&resp, max_rows, out),
            Err(_) => {
                flatten::push_text(out, &text, max_rows);
                Ok(())
            }
        }
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        if self.cloud {
            let r = self.get_json("/solr/admin/collections?action=LIST&wt=json").await?;
            return Ok(r.get("collections").and_then(J::as_arr).unwrap_or(&[]).iter().any(|c| c.as_str() == Some(name)));
        }
        let r = self.get_json(&format!("/solr/admin/cores?action=STATUS&core={name}&indexInfo=false&wt=json")).await?;
        let loaded = r.at(&["status", name]).and_then(J::as_obj).is_some_and(|o| !o.is_empty());
        Ok(loaded || r.at(&["initFailures", name]).is_some())
    }

    /// DBine's `PUT /solr/<name>` (create) and `DELETE /solr/<name>`
    /// (drop): a CoreAdmin call on a standalone server, a Collections API
    /// call on SolrCloud.
    async fn manage_collection(&self, req: &Request, name: &str, out: &mut QueryOutcome) -> Result<()> {
        let what = if self.cloud { "La colección" } else { "El core" };
        let exists = self.exists(name).await?;
        if req.method == "DELETE" {
            if !exists {
                if req.query_param("if_exists") == Some("true") {
                    out.messages.push(format!("{what} {name} no existe; no se borró nada."));
                    return Ok(());
                }
                return Err(Error::Query(format!("{what} {name} no existe.")));
            }
            let path = if self.cloud {
                format!("/solr/admin/collections?action=DELETE&name={name}&wt=json")
            } else {
                format!("/solr/admin/cores?action=UNLOAD&core={name}&deleteIndex=true&deleteDataDir=true&deleteInstanceDir=true&wt=json")
            };
            self.get_json(&path).await?;
            out.messages.push(format!("{what} {name} se borró."));
            return Ok(());
        }
        if exists {
            if req.query_param("if_not_exists") == Some("true") {
                out.messages.push(format!("{what} {name} ya existe; no se creó."));
                return Ok(());
            }
            return Err(Error::Query(format!("{what} {name} ya existe.")));
        }
        let body = match req.body.as_deref() {
            None => J::Obj(Vec::new()),
            Some(b) => match J::parse(b) {
                Ok(j @ J::Obj(_)) => j,
                _ => return Err(Error::Query(format!("El cuerpo de PUT /solr/{name} tiene que ser un objeto JSON."))),
            },
        };
        let mut params: Vec<(String, String)> = Vec::new();
        let mut config = "_default".to_string();
        for (k, v) in body.as_obj().into_iter().flatten() {
            match k.as_str() {
                "configSet" | "collection.configName" => config = v.text(),
                "numShards" | "replicationFactor" if !self.cloud => {}
                _ => params.push((k.clone(), v.text())),
            }
        }
        let path = if self.cloud {
            for (k, v) in [("numShards", "1"), ("replicationFactor", "1")] {
                if !params.iter().any(|(p, _)| p == k) {
                    params.push((k.into(), v.into()));
                }
            }
            // Without a configset Solr copies _default into <name>.AUTOCREATED,
            // so schema changes stay in this collection.
            if !config.is_empty() && config != "_default" {
                params.push(("collection.configName".into(), config));
            }
            "/solr/admin/collections"
        } else {
            params.push(("configSet".into(), if config.is_empty() { "_default".into() } else { config }));
            "/solr/admin/cores"
        };
        params.push(("action".into(), "CREATE".into()));
        params.push(("name".into(), name.to_string()));
        params.push(("wt".into(), "json".into()));
        let url = format!("{}{path}", self.base);
        if let Err(e) = self.call(self.client.get(url).query(&params)).await {
            if self.cloud {
                return Err(e);
            }
            // A core that failed to load stays registered: unload it.
            let _ = self
                .call(self.client.get(format!("{}/solr/admin/cores?action=UNLOAD&core={name}&deleteInstanceDir=true", self.base)))
                .await;
            let hint = if e.to_string().contains("Could not load conf") {
                ". En modo standalone el configset tiene que estar en <SOLR_HOME>/configsets (en la imagen de Docker, \
                 copiá /opt/solr/server/solr/configsets a /var/solr/data/configsets)."
            } else {
                ""
            };
            let e = e.to_string();
            return Err(Error::Query(format!("No se pudo crear el core {name}: {}{hint}", if hint.is_empty() { &e } else { e.trim_end_matches('.') })));
        }
        out.messages.push(format!("{what} {name} se creó."));
        Ok(())
    }

    /// Send a (normalized) request and parse its JSON reply.
    async fn fetch(&self, req: &Request) -> Result<J> {
        let m = reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rb = self.client.request(m, format!("{}{}", self.base, req.path));
        if let Some(body) = &req.body {
            let ct = if body.starts_with('{') || body.starts_with('[') { "application/json" } else { "application/x-www-form-urlencoded" };
            rb = rb.header("Content-Type", ct).body(body.clone());
        }
        let text = self.call(rb).await?;
        J::parse(&text).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))
    }

    async fn run_sql(&self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if !self.cloud {
            return Err(Error::Unsupported(
                "Solr SQL solo está disponible en SolrCloud. En modo standalone usá GET /solr/<core>/select?q=…".into(),
            ));
        }
        let coll = sql_collection(stmt)
            .ok_or_else(|| Error::Query("No se encontró la colección de la consulta (FROM <colección>).".into()))?;
        // Without LIMIT Solr streams the whole result through /export.
        let stmt = if has_limit(stmt) { stmt.to_string() } else { format!("{stmt} LIMIT {}", max_rows + 1) };
        let url = format!("{}/solr/{coll}/sql", self.base);
        let text = self.call(self.client.post(url).form(&[("stmt", stmt.as_str())])).await?;
        let resp = J::parse(&text).map_err(|e| Error::Query(format!("Respuesta SQL inesperada: {e}")))?;
        push_response(&resp, max_rows, out)
    }
}

/// The name in DBine's `PUT /solr/<name>` / `DELETE /solr/<name>` (a
/// normalized path with nothing after the name).
pub fn collection_command(req: &Request) -> Option<&str> {
    if !matches!(req.method.as_str(), "PUT" | "DELETE") {
        return None;
    }
    match req.segments().as_slice() {
        ["solr", name] if *name != "admin" => Some(name),
        _ => None,
    }
}

/// Turn a Solr JSON response into result sets.
fn push_response(resp: &J, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    // Streaming / SQL: {"result-set": {"docs": [..., {"EOF": true}]}}.
    if let Some(docs) = resp.at(&["result-set", "docs"]).and_then(J::as_arr) {
        if let Some(e) = docs.iter().find_map(|d| d.get("EXCEPTION")) {
            return Err(Error::Query(e.text()));
        }
        let rows = docs.iter().filter(|d| d.get("EOF").is_none()).filter_map(|d| d.as_obj().cloned());
        flatten::push_docs(out, rows, max_rows);
        return Ok(());
    }
    let mut shown = false;
    if let Some(r) = resp.get("response") {
        let docs = r.get("docs").and_then(J::as_arr).unwrap_or(&[]);
        flatten::push_docs(out, docs.iter().filter_map(|d| d.as_obj().cloned()), max_rows);
        if let Some(n) = r.get("numFound") {
            let qtime = resp.at(&["responseHeader", "QTime"]).map(J::text).unwrap_or_default();
            out.messages.push(format!("{} documentos coinciden ({qtime} ms).", n.text()));
        }
        shown = true;
    } else if let Some(doc) = resp.get("doc") {
        // Real-time get of a single id.
        flatten::push_docs(out, doc.as_obj().cloned(), max_rows);
        shown = true;
    }
    if let Some(fc) = resp.get("facet_counts") {
        push_facet_counts(fc, max_rows, out);
        shown = true;
    }
    if let Some(f) = resp.get("facets") {
        push_json_facets(f, max_rows, out);
        shown = true;
    }
    if let Some(g) = resp.get("grouped") {
        flatten::push_text(out, &g.pretty(), max_rows);
        shown = true;
    }
    if !shown {
        // Admin and other responses: drop the header, flatten what's left.
        let rest = match resp {
            J::Obj(o) => {
                let o: Obj = o.iter().filter(|(k, _)| k != "responseHeader").cloned().collect();
                match o.as_slice() {
                    [(_, only)] if !only.is_scalar() => only.clone(),
                    _ => J::Obj(o),
                }
            }
            other => other.clone(),
        };
        flatten::push_generic(out, &rest, max_rows);
    }
    Ok(())
}

/// Pairs from Solr's flat `[value, count, value, count…]` lists.
fn pairs(list: &J) -> Vec<(J, J)> {
    list.as_arr().unwrap_or(&[]).chunks(2).map(|c| (c[0].clone(), c.get(1).cloned().unwrap_or(J::Null))).collect()
}

/// Classic faceting: one `field`/`value`/`count` table for the field
/// facets, one `query`/`count` for the facet queries, ranges like fields.
fn push_facet_counts(fc: &J, max_rows: usize, out: &mut QueryOutcome) {
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    for (field, list) in fc.get("facet_fields").and_then(J::as_obj).into_iter().flatten() {
        for (v, c) in pairs(list) {
            rows.push(vec![field.clone().into(), v.cell(), c.cell()]);
        }
    }
    for (field, r) in fc.get("facet_ranges").and_then(J::as_obj).into_iter().flatten() {
        for (v, c) in pairs(r.get("counts").unwrap_or(&J::Null)) {
            rows.push(vec![field.clone().into(), v.cell(), c.cell()]);
        }
    }
    if !rows.is_empty() {
        out.begin_result(vec![rcol("field"), rcol("value"), rcol("count")]);
        for r in rows {
            out.push_row(r, max_rows);
        }
    }
    if let Some(q) = fc.get("facet_queries").and_then(J::as_obj).filter(|q| !q.is_empty()) {
        out.begin_result(vec![rcol("query"), rcol("count")]);
        for (k, v) in q {
            out.push_row(vec![k.clone().into(), v.cell()], max_rows);
        }
    }
    if let Some(p) = fc.get("facet_pivot").filter(|p| p.as_obj().is_some_and(|o| !o.is_empty())) {
        flatten::push_text(out, &p.pretty(), max_rows);
    }
}

/// JSON Facet API: a table per bucket facet (`val`, `count`, sub-facets),
/// the rest (`count`, metrics) in a `facet`/`value` table.
fn push_json_facets(f: &J, max_rows: usize, out: &mut QueryOutcome) {
    let mut metrics: Vec<(String, J)> = Vec::new();
    for (name, v) in f.as_obj().into_iter().flatten() {
        match v.get("buckets").and_then(J::as_arr) {
            Some(b) => {
                out.messages.push(format!("Faceta {name}: {} buckets.", b.len()));
                flatten::push_docs(out, b.iter().filter_map(|x| x.as_obj().cloned()), max_rows);
            }
            None => metrics.push((name.clone(), v.clone())),
        }
    }
    if !metrics.is_empty() {
        out.begin_result(vec![rcol("facet"), rcol("value")]);
        for (k, v) in metrics {
            out.push_row(vec![k.into(), v.cell()], max_rows);
        }
    }
}

#[async_trait]
impl Session for SolrSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(self.version.clone())
    }

    /// Solr has no sessions, and its task API only sees queries sent with
    /// `canCancel=true`, core by core: no list of what's running.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        Err(Error::Unsupported("Solr no lleva una lista de las consultas en curso ni de sesiones: su gestión de tareas (/tasks/list) solo ve, núcleo por núcleo, las consultas que se enviaron con canCancel=true".into()))
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        let mut refused = Vec::new();
        match self.get_json("/solr/admin/info/system?wt=json").await {
            Ok(sys) => monitor::from_system(&mut s, &sys),
            Err(e) => refused.push(format!("admin/info/system ({e})")),
        }
        match self.get_json(monitor::NODE_METRICS).await {
            Ok(m) => monitor::from_node_metrics(&mut s, &m),
            Err(e) => refused.push(format!("admin/metrics ({e})")),
        }
        match self.get_json(monitor::CORE_METRICS).await {
            Ok(m) => monitor::from_core_metrics(&mut s, &m),
            Err(e) => refused.push(format!("admin/metrics de los cores ({e})")),
        }
        match self.get_json("/solr/admin/cores?action=STATUS&wt=json").await {
            Ok(st) => monitor::cores(&mut s, &st),
            Err(e) => refused.push(format!("admin/cores ({e})")),
        }
        if self.cloud {
            match self.get_json("/solr/admin/collections?action=CLUSTERSTATUS&wt=json").await {
                Ok(cs) => monitor::cluster(&mut s, &cs),
                Err(e) => refused.push(format!("CLUSTERSTATUS ({e})")),
            }
        }
        if !refused.is_empty() {
            s.notes.push(format!("No se pudieron leer: {}.", refused.join("; ")));
        }
        s.notes.push(
            "Solr no lista las consultas en curso ni las conexiones abiertas; se muestran los pedidos que Jetty está atendiendo."
                .into(),
        );
        Ok(s)
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

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut names: Vec<String> = if self.cloud {
            let r = self.get_json("/solr/admin/collections?action=LIST&wt=json").await?;
            r.get("collections").and_then(J::as_arr).unwrap_or(&[]).iter().filter_map(|c| c.as_str().map(str::to_string)).collect()
        } else {
            let r = self.get_json("/solr/admin/cores?action=STATUS&indexInfo=false&wt=json").await?;
            r.get("status").and_then(J::as_obj).into_iter().flatten().map(|(k, _)| k.clone()).collect()
        };
        names.retain(|n| !n.starts_with('.'));
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| DbObject { kind: kinds::COLLECTION.into(), schema: None, name, parent: None })
            .collect())
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let c = &o.name;
        let fields = self.get_json(&format!("/solr/{c}/schema/fields?wt=json")).await?;
        let key = self
            .get_json(&format!("/solr/{c}/schema/uniquekey?wt=json"))
            .await
            .ok()
            .and_then(|k| k.get("uniqueKey").and_then(J::as_str).map(str::to_string));
        let internal = |n: &str| n.len() > 1 && n.starts_with('_') && n.ends_with('_');
        let mut out: Vec<ColumnInfo> = Vec::new();
        for f in fields.get("fields").and_then(J::as_arr).unwrap_or(&[]) {
            let Some(name) = f.get("name").and_then(J::as_str).filter(|n| !internal(n)) else { continue };
            let mut ty = f.get("type").map(J::text).unwrap_or_default();
            if f.get("multiValued").and_then(J::as_bool) == Some(true) {
                ty.push_str("[]");
            }
            out.push(ColumnInfo {
                name: name.to_string(),
                data_type: ty,
                nullable: f.get("required").and_then(J::as_bool) != Some(true),
                primary_key: key.as_deref() == Some(name),
                auto_increment: false,
                default_value: f.get("default").map(J::text),
            });
        }
        // Fields actually in the index, which adds dynamic-field instances.
        if let Ok(luke) = self.get_json(&format!("/solr/{c}/admin/luke?numTerms=0&wt=json")).await {
            for (name, f) in luke.get("fields").and_then(J::as_obj).into_iter().flatten() {
                if internal(name) || out.iter().any(|x| x.name == *name) {
                    continue;
                }
                out.push(ColumnInfo {
                    name: name.clone(),
                    data_type: f.get("type").map(J::text).unwrap_or_default(),
                    nullable: true,
                    primary_key: false,
                    auto_increment: false,
                    default_value: None,
                });
            }
        }
        Ok(out)
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
        let s = self.get_json(&format!("/solr/{}/schema?wt=json", o.name)).await?;
        Ok(Some(s.get("schema").unwrap_or(&s).pretty()))
    }

    /// Every core / collection with its schema fields (explicit properties
    /// only, so they round-trip into `add-field`), the uniqueKey as primary
    /// key and, on SolrCloud, shards, replication factor and configset.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let cluster = if self.cloud {
            Some(self.get_json("/solr/admin/collections?action=CLUSTERSTATUS&wt=json").await?)
        } else {
            None
        };
        let mut out = Vec::new();
        for o in self.list_objects().await? {
            let c = &o.name;
            let fields = self.get_json(&format!("/solr/{c}/schema/fields?showDefaults=false&wt=json")).await?;
            let key = self.get_json(&format!("/solr/{c}/schema/uniquekey?wt=json")).await.ok();
            let key = key.as_ref().and_then(|k| k.get("uniqueKey")).and_then(J::as_str);
            let status = cluster.as_ref().and_then(|s| s.at(&["cluster", "collections", c]));
            out.push(ddl::collection_schema(c, &fields, key, status));
        }
        Ok(out)
    }

    /// `cursorMark` paging sorted by the uniqueKey, typed by the schema.
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, columns, source, progress).await
    }

    fn browse_query(&self, o: &ObjectRef, limit: u32) -> String {
        format!("GET /solr/{}/select?q=*:*&rows={limit}", o.name)
    }

    /// Searches (`select`, `query`, `browse`) get plans from Solr's debug
    /// output; other requests and SQL have none: with `analyze` they run
    /// as in `execute`, without it they're skipped. Solr has no plan
    /// without running the search: the estimated plan asks for `rows=0`
    /// (the search still matches and counts, but returns no documents).
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let cmds = console::parse(text).map_err(Error::Query)?;
        if cmds.is_empty() {
            return Err(Error::Query("No hay ninguna petición para ejecutar.".into()));
        }
        for c in &cmds {
            let r = match c {
                Command::Sql(s) => {
                    out.messages.push(format!("`{}`: Solr SQL no tiene plan de ejecución.", http::clip(s, 80)));
                    if analyze {
                        self.run_sql(s, max_rows, out).await?;
                    }
                    continue;
                }
                Command::Http(r) => r,
            };
            let req = Request { path: normalize_path(&r.path), ..r.clone() };
            let label = format!("{} {}", req.method, req.path);
            let Some((core, handler)) = plan_target(&req) else {
                out.messages.push(format!("`{label}`: solo las búsquedas (select, query) tienen plan de ejecución."));
                if analyze {
                    self.run_request(r, max_rows, out).await?;
                }
                continue;
            };
            let resp = self.fetch(&debug_request(&req, analyze)).await?;
            if analyze {
                push_response(&resp, max_rows, out)?;
            }
            let statement = match &req.body {
                Some(b) => format!("{label}\n{b}"),
                None => label,
            };
            out.plans.push(plan::from_debug(&statement, &core, &handler, &resp, analyze));
        }
        Ok(())
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let cmds = console::parse(text).map_err(Error::Query)?;
        if cmds.is_empty() {
            return Err(Error::Query("No hay ninguna petición para ejecutar.".into()));
        }
        for c in &cmds {
            match c {
                Command::Http(r) => self.run_request(r, max_rows, out).await?,
                Command::Sql(s) => self.run_sql(s, max_rows, out).await?,
            }
        }
        Ok(())
    }

    /// security.json's rule-based authorization (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self).await
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn req(method: &str, path: &str, body: Option<&str>) -> Request {
        Request { method: method.into(), path: normalize_path(path), body: body.map(str::to_string) }
    }

    #[test]
    fn plan_requests() {
        assert_eq!(plan_target(&req("GET", "/films/select?q=*:*", None)), Some(("films".into(), "select".into())));
        assert_eq!(plan_target(&req("POST", "/solr/films/query", Some("{}"))), Some(("films".into(), "query".into())));
        assert_eq!(plan_target(&req("GET", "/films/update?commit=true", None)), None);
        assert_eq!(plan_target(&req("GET", "/admin/cores", None)), None);
        let e = debug_request(&req("GET", "/films/select?q=a:b&rows=10", None), false);
        assert_eq!(e.path, "/solr/films/select?q=a:b&rows=0&debug=query&wt=json");
        let a = debug_request(&req("GET", "/films/select", None), true);
        assert_eq!(a.path, "/solr/films/select?debug=query&debug=timing&wt=json");
        let j = debug_request(&req("POST", "/films/query", Some(r#"{"query":"*:*","limit":5}"#)), false);
        assert_eq!(j.body.as_deref(), Some(r#"{"query":"*:*","limit":0}"#));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(method: &str, path: &str) -> Request {
        Request { method: method.into(), path: normalize_path(path), body: None }
    }

    #[test]
    fn collection_commands() {
        assert_eq!(collection_command(&req("PUT", "/books")), Some("books"));
        assert_eq!(collection_command(&req("DELETE", "/solr/books?if_exists=true")), Some("books"));
        assert_eq!(collection_command(&req("PUT", "/books/schema")), None);
        assert_eq!(collection_command(&req("GET", "/books")), None);
        assert_eq!(collection_command(&req("DELETE", "/admin")), None);
        assert!(!read_only_allows(&req("PUT", "/books")));
        assert!(!read_only_allows(&req("DELETE", "/books")));
    }

    #[test]
    fn paths_get_the_solr_prefix() {
        assert_eq!(normalize_path("/core1/select?q=*:*"), "/solr/core1/select?q=*:*");
        assert_eq!(normalize_path("/solr/core1/select"), "/solr/core1/select");
        assert_eq!(normalize_path("/api/cores"), "/api/cores");
        assert_eq!(normalize_path("/solrx/select"), "/solr/solrx/select");
    }

    #[test]
    fn read_only_whitelist() {
        assert!(read_only_allows(&req("GET", "/c/select?q=*:*")));
        assert!(read_only_allows(&req("POST", "/c/query")));
        assert!(read_only_allows(&req("GET", "/c/schema/fields")));
        assert!(read_only_allows(&req("GET", "/c/admin/luke")));
        assert!(read_only_allows(&req("GET", "/admin/cores?action=STATUS")));
        assert!(read_only_allows(&req("GET", "/admin/collections?action=list")));
        assert!(read_only_allows(&req("GET", "/admin/info/system")));
        assert!(!read_only_allows(&req("GET", "/c/update?commit=true")));
        assert!(!read_only_allows(&req("POST", "/c/update")));
        assert!(!read_only_allows(&req("POST", "/c/schema")));
        assert!(!read_only_allows(&req("GET", "/admin/cores?action=UNLOAD&core=c")));
        assert!(!read_only_allows(&req("GET", "/admin/collections?action=DELETE&name=c")));
        assert!(!read_only_allows(&req("GET", "/c/stream?expr=update(x)")));
        assert!(!read_only_allows(&req("DELETE", "/api/collections/c")));
    }

    #[test]
    fn sql_helpers() {
        assert_eq!(sql_collection("SELECT a FROM books WHERE x=1").as_deref(), Some("books"));
        assert_eq!(sql_collection("select count(*) from \"my-coll\"").as_deref(), Some("my-coll"));
        assert_eq!(sql_collection("select 1"), None);
        assert!(has_limit("select a from b LIMIT 5"));
        assert!(!has_limit("select limited from b"));
    }

    #[test]
    fn select_response_and_facets() {
        let resp = J::parse(
            r#"{"responseHeader":{"status":0,"QTime":2},
                "response":{"numFound":2,"start":0,"docs":[{"id":"1","title":["a"]},{"id":"2","price":9.5}]},
                "facet_counts":{"facet_queries":{},"facet_fields":{"cat":["x",3,"y",1]},"facet_ranges":{},"facet_pivot":{}}}"#,
        )
        .unwrap();
        let mut out = QueryOutcome::default();
        push_response(&resp, 100, &mut out).unwrap();
        assert_eq!(out.results.len(), 2);
        let names: Vec<_> = out.results[0].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "title", "price"]);
        assert_eq!(out.results[0].rows[0], vec![json!("1"), json!("[\"a\"]"), json!(null)]);
        assert_eq!(out.results[1].rows[1], vec![json!("cat"), json!("y"), json!(1)]);
        assert!(out.messages[0].starts_with("2 documentos"));
    }

    #[test]
    fn sql_result_set() {
        let ok = J::parse(r#"{"result-set":{"docs":[{"id":"1","n":2},{"EOF":true,"RESPONSE_TIME":3}]}}"#).unwrap();
        let mut out = QueryOutcome::default();
        push_response(&ok, 10, &mut out).unwrap();
        assert_eq!(out.results[0].rows, vec![vec![json!("1"), json!(2)]]);
        let err = J::parse(r#"{"result-set":{"docs":[{"EXCEPTION":"bad column","EOF":true}]}}"#).unwrap();
        assert_eq!(push_response(&err, 10, &mut out).unwrap_err().to_string(), "bad column");
    }

    #[test]
    fn errors() {
        let body = r#"{"responseHeader":{"status":400},"error":{"msg":"undefined field foo","code":400}}"#;
        assert_eq!(solr_error_message(400, body), "undefined field foo");
        assert_eq!(solr_error_message(404, "<html>nope</html>"), "HTTP 404: <html>nope</html>");
    }
}

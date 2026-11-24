//! What an object uses (`Session::references`).
//!
//! - A table, view or materialized view: Unity Catalog's lineage
//!   (`GET /api/2.0/lineage-tracking/table-lineage`, `table_name` =
//!   `catalog.schema.table`, `include_entity_lineage`), its `upstreams`.
//!   An upstream table of this catalog is an item, Confirmed, with the
//!   notebooks and jobs that carried it as its detail; one of another
//!   catalog is `external`. Upstream files (`fileInfo`) and the notebooks,
//!   jobs, pipelines, queries and dashboards that wrote the table are
//!   `external` too, with a link to them in the workspace (jobs and
//!   pipelines with their names). Lineage is captured from what ran (a
//!   query, a job, a pipeline), and kept since September 2024.
//! - Its own definition (views, materialized views, functions), read as the
//!   contract's default does (`used_names`, then a lookup by name): what
//!   lineage hasn't seen stays Probable. A name qualified with another
//!   catalog (`other.schema.t`) is `external` instead of a same-named
//!   object here.
//! - A table's foreign keys (Unity Catalog's informational ones): the
//!   default.
//! - No lineage (the legacy `hive_metastore`, no BROWSE or SELECT on the
//!   table, a workspace without Unity Catalog): the rest, and a note.

use crate::catalog_api::HIVE_METASTORE;
use crate::health::escape;
use crate::DatabricksSession;
use dbine_driver::dependencies::{
    references_scan, used_names, used_objects, Confidence, DependencyReport, DependencyScan,
    DependencyTarget, Dependent, ExternalRef, Mention, Relation, UsedName, LINE_CHARS, LOOKUP_CAP,
    MAX_MENTIONS,
};
use dbine_driver::{kinds, DbObject, Error, ObjectQuery, Result, Session};
use serde_json::{json, Value as Json};

pub(crate) const LINEAGE_PATH: &str = "/api/2.0/lineage-tracking/table-lineage";
/// Names looked up for one report, at most.
const NAME_LOOKUPS: usize = 100;
/// Name lookups in flight at once.
const NAME_PARALLEL: usize = 8;
/// How long a name found stays valid in memory.
const NAME_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// Pages of one lineage answer followed, at most.
const LINEAGE_PAGES: usize = 20;
/// The lineage API's cap on what one level of a table returns is paged by `next_page_token`.
const PAGE_TOKEN: &str = "page_token";

/// Lineage couldn't be read.
pub(crate) const NO_LINEAGE: &str = "El linaje de Unity Catalog no está disponible para este objeto (hace falta BROWSE o SELECT sobre la tabla y un catálogo de Unity Catalog): lo que usa sale de su definición, sin confirmar.";

/// The query of a lineage request for `catalog.schema.table`.
pub(crate) fn lineage_query(
    catalog: &str,
    schema: &str,
    table: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("table_name", format!("{catalog}.{schema}.{table}")),
        ("include_entity_lineage", "true".into()),
    ]
}

/// A notebook, job, pipeline, query or dashboard of the lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entity {
    /// `notebook`, `job`, `pipeline`, `query`, `dashboard`…
    pub kind: String,
    pub id: String,
    /// A Lakeview dashboard (`dashboardV3Infos`).
    pub v3: bool,
}

/// An upstream table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpTable {
    pub catalog: String,
    pub schema: String,
    pub name: String,
    pub table_type: String,
    pub via: Vec<Entity>,
}

/// The `upstreams` of a lineage answer.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Lineage {
    pub tables: Vec<UpTable>,
    /// A path, its securable (an external location, a volume…).
    pub files: Vec<(String, Option<String>)>,
    /// Every notebook, job… that wrote the table, once each.
    pub entities: Vec<Entity>,
}

/// `notebookInfos` → `notebook`; the id is the field ending in `_id` that
/// names it (`notebook_id`, `job_id`…).
fn entities(entry: &Json) -> Vec<Entity> {
    let mut out = Vec::new();
    let Some(obj) = entry.as_object() else {
        return out;
    };
    // By key: a JSON object's order is serde_json's (sorted, or as written
    // when a crate in the build turns on `preserve_order`).
    let mut keys: Vec<_> = obj.iter().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    for (key, list) in keys {
        let Some(stem) = key.strip_suffix("Infos") else {
            continue;
        };
        let v3 = stem == "dashboardV3";
        let kind = if v3 { "dashboard" } else { stem };
        let id_key = format!("{kind}_id");
        for info in list.as_array().into_iter().flatten() {
            let id = info.get(&id_key).or_else(|| {
                let mut fields: Vec<_> = info.as_object()?.iter().collect();
                fields.sort_by(|a, b| a.0.cmp(b.0));
                fields
                    .into_iter()
                    .find(|(k, _)| {
                        k.ends_with("_id")
                            && *k != "workspace_id"
                            && !k.ends_with("run_id")
                            && !k.ends_with("update_id")
                    })
                    .map(|(_, v)| v)
            });
            let id = match id {
                Some(Json::String(s)) => s.clone(),
                Some(Json::Number(n)) => n.to_string(),
                _ => continue,
            };
            let e = Entity {
                kind: kind.to_string(),
                id,
                v3,
            };
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    out
}

pub(crate) fn parse_lineage(resp: &Json) -> Lineage {
    parse_side(resp, "upstreams")
}

/// One side of a lineage answer: `upstreams` (what the table reads) or `downstreams` (what reads it).
pub(crate) fn parse_side(resp: &Json, side: &str) -> Lineage {
    let mut l = Lineage::default();
    for entry in resp
        .get(side)
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let via = entities(entry);
        for e in &via {
            if !l.entities.contains(e) {
                l.entities.push(e.clone());
            }
        }
        let s = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).map(str::to_string);
        if let Some(t) = entry.get("tableInfo") {
            // A table the login can't see comes without its parts.
            if let (Some(catalog), Some(schema), Some(name)) =
                (s(t, "catalog_name"), s(t, "schema_name"), s(t, "name"))
            {
                l.tables.push(UpTable {
                    catalog,
                    schema,
                    name,
                    table_type: s(t, "table_type").unwrap_or_default(),
                    via,
                });
            }
        } else if let Some(f) = entry.get("fileInfo") {
            if let Some(path) = s(f, "path") {
                l.files.push((path, s(f, "securable_name")));
            }
        }
    }
    l
}

/// The entities as an item's detail: `job 774 · notebook 111`.
fn via_detail(via: &[Entity]) -> Option<String> {
    (!via.is_empty()).then(|| {
        via.iter()
            .map(|e| format!("{} {}", e.kind, e.id))
            .collect::<Vec<_>>()
            .join(" · ")
    })
}

/// Where an entity opens in the workspace at `base`.
pub(crate) fn entity_url(base: &str, e: &Entity) -> Option<String> {
    let path = match e.kind.as_str() {
        "notebook" => format!("/#notebook/{}", e.id),
        "job" => format!("/jobs/{}", e.id),
        "pipeline" => format!("/pipelines/{}", e.id),
        "query" => format!("/sql/editor/{}", e.id),
        "dashboard" if e.v3 => format!("/dashboardsv3/{}", e.id),
        "dashboard" => format!("/sql/dashboards/{}", e.id),
        _ => return None,
    };
    Some(format!("{}{path}", base.trim_end_matches('/')))
}

/// Which service a path is in.
pub(crate) fn storage_system(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("s3://") || lower.starts_with("s3a://") {
        "Amazon S3"
    } else if lower.starts_with("abfss://")
        || lower.starts_with("abfs://")
        || lower.starts_with("wasbs://")
    {
        "Azure Data Lake Storage"
    } else if lower.starts_with("gs://") {
        "Google Cloud Storage"
    } else if lower.starts_with("/volumes/") {
        "Unity Catalog volume"
    } else if lower.starts_with("dbfs:") {
        "DBFS"
    } else {
        "File"
    }
}

/// A path or URL as shown: without user info, query string or fragment.
pub(crate) fn clean_location(v: &str) -> String {
    let v = v.split(['?', '#']).next().unwrap_or(v);
    match v.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.find('/') {
                Some(p) => rest.split_at(p),
                None => (rest, ""),
            };
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            format!("{scheme}://{host}{path}")
        }
        None => v.to_string(),
    }
}

/// Adds what lineage says to `report`: this catalog's tables as confirmed
/// items, the rest as sources outside. `names`: jobs' and pipelines'
/// names by id.
pub(crate) fn merge_lineage(
    report: &mut DependencyReport,
    l: &Lineage,
    catalog: &str,
    target: &dbine_driver::ObjectRef,
    base: &str,
    names: &[(String, String)],
) {
    for t in &l.tables {
        if t.catalog.eq_ignore_ascii_case(catalog) {
            let same = t.name.eq_ignore_ascii_case(&target.name)
                && target
                    .schema()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&t.schema));
            if same {
                continue;
            }
            add(
                &mut report.items,
                Dependent {
                    kind: crate::stats::kind_of(&t.table_type).to_string(),
                    schema: Some(t.schema.clone()),
                    name: t.name.clone(),
                    parent: None,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: via_detail(&t.via),
                    mentions: Vec::new(),
                },
            );
        } else {
            add_external(
                &mut report.external,
                ExternalRef {
                    system: Some("Databricks".into()),
                    database: Some(t.catalog.clone()),
                    schema: Some(t.schema.clone()),
                    name: Some(t.name.clone()),
                    detail: Some(t.table_type.clone()).filter(|x| !x.is_empty()),
                    ..Default::default()
                },
            );
        }
    }
    for (path, securable) in &l.files {
        add_external(
            &mut report.external,
            ExternalRef {
                system: Some(storage_system(path).into()),
                name: Some(clean_location(path)),
                detail: securable.clone(),
                ..Default::default()
            },
        );
    }
    for e in &l.entities {
        let name = names
            .iter()
            .find(|(id, _)| *id == e.id)
            .map_or_else(|| e.id.clone(), |(_, n)| n.clone());
        add_external(
            &mut report.external,
            ExternalRef {
                system: Some(format!("Databricks {}", e.kind)),
                name: Some(name),
                detail: entity_url(base, e),
                ..Default::default()
            },
        );
    }
}

impl DatabricksSession {
    pub(crate) async fn references_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = target.object.clone();
        let Ok(catalog) = self.catalog().map(str::to_string) else {
            return references_scan(self, target, scan).await;
        };
        let mut report = if scan.written_kinds.contains(&t.kind) {
            let mut r = DependencyReport::default();
            code_refs(self, target, scan, &catalog, "Databricks", &mut r).await?;
            r
        } else {
            // A table: its own foreign keys (one query on the warehouse, never
            // the whole catalog's). Lineage doesn't need it: a failure only
            // leaves the keys out.
            match self.own_foreign_keys(&t, scan).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("databricks foreign keys of {}: {e}", t.name);
                    DependencyReport {
                        unreadable: vec![match t.schema() {
                            Some(sc) => format!("{sc}.{}", t.name),
                            None => t.name.clone(),
                        }],
                        ..Default::default()
                    }
                }
            }
        };
        if matches!(
            t.kind.as_str(),
            kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW
        ) {
            let schema = t
                .schema()
                .or(self.schema.as_deref())
                .unwrap_or("default")
                .to_string();
            let lineage = if catalog.eq_ignore_ascii_case(HIVE_METASTORE) {
                Err(Error::Unsupported(HIVE_METASTORE.into()))
            } else {
                self.lineage_pair(&catalog, &schema, &t.name).await
            };
            match lineage {
                Ok((l, _)) => {
                    let names = self.entity_names(&l.entities).await;
                    let base = self.api.base.clone();
                    merge_lineage(&mut report, &l, &catalog, &t, &base, &names);
                }
                Err(e) => {
                    tracing::debug!("databricks lineage of {catalog}.{schema}.{}: {e}", t.name);
                    report.note = Some(NO_LINEAGE.into());
                }
            }
        }
        sort(&mut report.items);
        Ok(report)
    }

    /// What feeds one table and what reads it, from `system.access.table_lineage` (one statement,
    /// this table only, both directions); when that table can't be read (no SELECT on it, no
    /// `system` schema), from the lineage API. An error is that neither answered.
    pub(crate) async fn lineage_pair(
        &mut self,
        catalog: &str,
        schema: &str,
        table: &str,
    ) -> Result<(Lineage, Lineage)> {
        let full = format!("{catalog}.{schema}.{table}").to_lowercase();
        match self.text_rows(&table_lineage_sql(), &[&full]).await {
            Ok(rows) => Ok(lineage_from_rows(&rows, &full)),
            Err(e) => {
                tracing::debug!("databricks system.access.table_lineage of {full}: {e}");
                let r = self.table_lineage(catalog, schema, table).await?;
                Ok((parse_side(&r, "upstreams"), parse_side(&r, "downstreams")))
            }
        }
    }

    /// The lineage of one table from the API, every page of it: `upstreams` and `downstreams`.
    pub(crate) async fn table_lineage(
        &self,
        catalog: &str,
        schema: &str,
        table: &str,
    ) -> Result<Json> {
        let mut ups: Vec<Json> = Vec::new();
        let mut downs: Vec<Json> = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..LINEAGE_PAGES {
            let mut q = lineage_query(catalog, schema, table);
            if let Some(t) = token.take() {
                q.push((PAGE_TOKEN, t));
            }
            let r = self.api.get_query(LINEAGE_PATH, &q).await?;
            for (side, out) in [("upstreams", &mut ups), ("downstreams", &mut downs)] {
                if let Some(a) = r.get(side).and_then(Json::as_array) {
                    out.extend(a.iter().cloned());
                }
            }
            token = r
                .get("next_page_token")
                .and_then(Json::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        Ok(json!({ "upstreams": ups, "downstreams": downs }))
    }

    /// A table's own foreign keys, as what it uses (one query).
    async fn own_foreign_keys(
        &mut self,
        t: &dbine_driver::ObjectRef,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let mut report = DependencyReport::default();
        if scan.foreign_keys && t.kind == kinds::TABLE {
            let schema = t
                .schema()
                .or(self.schema.as_deref())
                .unwrap_or("default")
                .to_string();
            let sql = crate::index_usage::foreign_keys_sql(self.catalog()?);
            let rows = self.text_rows(&sql, &[&schema, &t.name]).await?;
            report
                .items
                .extend(dbine_driver::dependencies::foreign_key_parents(
                    &crate::index_usage::foreign_keys(&rows),
                    t,
                ));
        }
        Ok(report)
    }

    /// What reads one table or view (`Session::dependents`), the native way: the lineage API's
    /// `downstreams` (confirmed, with the jobs and notebooks that carried each line), the views
    /// whose definition names it (one query over the catalog's `information_schema.views`,
    /// probable) and the tables whose foreign keys point at it. Never every definition of the
    /// catalog. Without lineage (no access, no Unity Catalog, `hive_metastore`) or for anything
    /// else (a function, a column) it is the contract's scan, as before.
    pub(crate) async fn dependents_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = target.object.clone();
        let catalog = self.catalog().map(str::to_string).ok();
        let object = matches!(
            t.kind.as_str(),
            kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW
        ) && target.column.is_none();
        let Some(catalog) = catalog.filter(|c| object && !c.eq_ignore_ascii_case(HIVE_METASTORE))
        else {
            return dbine_driver::dependencies::scan(self, target, scan).await;
        };
        let schema = t
            .schema()
            .or(self.schema.as_deref())
            .unwrap_or("default")
            .to_string();
        let l = match self.lineage_pair(&catalog, &schema, &t.name).await {
            Ok((_, down)) => down,
            Err(e) => {
                tracing::debug!("databricks lineage of {catalog}.{schema}.{}: {e}", t.name);
                let mut r = dbine_driver::dependencies::scan(self, target, scan).await?;
                r.note = Some(NO_LINEAGE.into());
                return Ok(r);
            }
        };
        let names = self.entity_names(&l.entities).await;
        let base = self.api.base.clone();
        let mut report = DependencyReport::default();
        merge_downstream(&mut report, &l, &catalog, &t, &base, &names);
        // What lineage hasn't seen: the views that name it, and the foreign keys that point at it.
        match self.view_mentions(&catalog, &t, target, scan).await {
            Ok(items) => {
                for d in items {
                    add(&mut report.items, d);
                }
            }
            Err(e) => tracing::debug!("databricks views naming {}: {e}", t.name),
        }
        if scan.foreign_keys && t.kind == kinds::TABLE {
            match self.foreign_key_children(&catalog, &schema, &t.name).await {
                Ok(items) => {
                    for d in items {
                        add(&mut report.items, d);
                    }
                }
                Err(e) => {
                    tracing::debug!("databricks foreign keys to {}: {e}", t.name);
                    report.unreadable.push(format!("{schema}.{}", t.name));
                }
            }
        }
        sort(&mut report.items);
        Ok(report)
    }

    /// The views of the catalog whose definition contains the table's name: one filtered query,
    /// then the contract's own check of each body.
    async fn view_mentions(
        &mut self,
        catalog: &str,
        t: &dbine_driver::ObjectRef,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<Vec<Dependent>> {
        let sql = views_naming_sql(catalog);
        let like = format!("%{}%", t.name.to_ascii_lowercase());
        let rows = self.text_rows(&sql, &[&like]).await?;
        let mut out = Vec::new();
        for r in rows {
            let c = |i: usize| r.get(i).cloned().flatten();
            let (Some(schema), Some(name), Some(body)) = (c(0), c(1), c(2)) else {
                continue;
            };
            if name.eq_ignore_ascii_case(&t.name)
                && t.schema().is_some_and(|s| s.eq_ignore_ascii_case(&schema))
            {
                continue;
            }
            if let Some((confidence, mentions)) =
                dbine_driver::dependencies::find_mentions(&body, &scan.dialect, target)
            {
                out.push(Dependent {
                    kind: kinds::VIEW.to_string(),
                    schema: Some(schema),
                    name,
                    parent: None,
                    relation: Relation::Code,
                    confidence,
                    detail: None,
                    mentions,
                });
            }
        }
        Ok(out)
    }

    /// The tables whose foreign keys point at `schema.table` (confirmed).
    async fn foreign_key_children(
        &mut self,
        catalog: &str,
        schema: &str,
        table: &str,
    ) -> Result<Vec<Dependent>> {
        let rows = self
            .text_rows(&foreign_key_children_sql(catalog), &[schema, table])
            .await?;
        let mut out: Vec<Dependent> = Vec::new();
        for r in rows {
            let c = |i: usize| r.get(i).cloned().flatten();
            let (Some(name), Some(sch), Some(tab)) = (c(0), c(1), c(2)) else {
                continue;
            };
            let d = Dependent {
                kind: kinds::TABLE.to_string(),
                schema: Some(sch),
                name: tab,
                parent: None,
                relation: Relation::ForeignKey,
                confidence: Confidence::Confirmed,
                detail: Some(name),
                mentions: Vec::new(),
            };
            add(&mut out, d);
        }
        Ok(out)
    }

    /// The names of jobs, pipelines, queries and dashboards by id, at most [`NAME_LOOKUPS`] and
    /// [`NAME_PARALLEL`] at a time; a name found is kept for [`NAME_TTL`]. One that can't be read
    /// keeps its id (the notebooks too: Databricks has no call that gives a notebook's path from
    /// its object id).
    pub(crate) async fn entity_names(&self, entities: &[Entity]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut todo: Vec<Entity> = Vec::new();
        for e in entities
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "job" | "pipeline" | "query" | "dashboard"))
        {
            if out.iter().any(|(id, _)| *id == e.id) || todo.iter().any(|t| t.id == e.id) {
                continue;
            }
            match cached_name(&self.api.base, e) {
                Some(n) => out.push((e.id.clone(), n)),
                None => todo.push(e.clone()),
            }
        }
        todo.truncate(NAME_LOOKUPS);
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(NAME_PARALLEL));
        let mut set = tokio::task::JoinSet::new();
        for e in todo {
            let (api, gate) = (self.api.clone(), gate.clone());
            set.spawn(async move {
                let _slot = gate.acquire().await.ok();
                let name = lookup_name(&api, &e).await;
                (api.base.clone(), e, name)
            });
        }
        while let Some(done) = set.join_next().await {
            if let Ok((base, e, Some(n))) = done {
                keep_name(&base, &e, &n);
                out.push((e.id, n));
            }
        }
        out
    }
}

type NameKey = (String, String, String);
type NameCache = std::collections::HashMap<NameKey, (std::time::Instant, String)>;
static NAMES: std::sync::Mutex<Option<NameCache>> = std::sync::Mutex::new(None);

fn name_key(base: &str, e: &Entity) -> NameKey {
    (base.to_string(), e.kind.clone(), e.id.clone())
}

fn cached_name(base: &str, e: &Entity) -> Option<String> {
    let g = NAMES.lock().ok()?;
    let (at, n) = g.as_ref()?.get(&name_key(base, e))?;
    (at.elapsed() < NAME_TTL).then(|| n.clone())
}

fn keep_name(base: &str, e: &Entity, n: &str) {
    if let Ok(mut g) = NAMES.lock() {
        g.get_or_insert_with(Default::default).insert(
            name_key(base, e),
            (std::time::Instant::now(), n.to_string()),
        );
    }
}

/// What names `e` in the workspace: the field of the answer that holds the name.
async fn lookup_name(api: &crate::Api, e: &Entity) -> Option<String> {
    let id = escape(&e.id);
    let text = |j: &Json, p: &str| j.pointer(p)?.as_str().map(str::to_string);
    match (e.kind.as_str(), e.v3) {
        ("job", _) => text(
            &api.get_query("/api/2.1/jobs/get", &[("job_id", e.id.clone())])
                .await
                .ok()?,
            "/settings/name",
        ),
        ("pipeline", _) => text(
            &api.get(&format!("/api/2.0/pipelines/{id}")).await.ok()?,
            "/name",
        ),
        ("query", _) => {
            if let Ok(q) = api.get(&format!("/api/2.0/sql/queries/{id}")).await {
                if let Some(n) = text(&q, "/display_name") {
                    return Some(n);
                }
            }
            text(
                &api.get(&format!("/api/2.0/preview/sql/queries/{id}"))
                    .await
                    .ok()?,
                "/name",
            )
        }
        ("dashboard", true) => text(
            &api.get(&format!("/api/2.0/lakeview/dashboards/{id}"))
                .await
                .ok()?,
            "/display_name",
        ),
        ("dashboard", false) => text(
            &api.get(&format!("/api/2.0/preview/sql/dashboards/{id}"))
                .await
                .ok()?,
            "/name",
        ),
        _ => None,
    }
}

/// Rows the one-table lineage query returns at most.
const TABLE_LINEAGE_ROWS: u32 = 5000;

/// One table's lineage in both directions (`:p0` its lower-cased full name): the rows where it
/// is the target (what feeds it) and where it is the source (what reads it), over the last
/// [`crate::lineage::LINEAGE_DAYS`] days so the table's `event_date` partitions are pruned, with
/// the notebook, job, pipeline, query or dashboard of each row.
pub(crate) fn table_lineage_sql() -> String {
    format!(
        "SELECT source_table_full_name, source_table_catalog, source_table_schema, source_table_name, source_type, source_path,
       target_table_full_name, target_table_catalog, target_table_schema, target_table_name, target_type, target_path,
       entity_type, entity_id
  FROM system.access.table_lineage
 WHERE (target_table_full_name = :p0 OR source_table_full_name = :p0)
   AND event_date >= date_sub(current_date(), {})
 GROUP BY ALL
 LIMIT {TABLE_LINEAGE_ROWS}",
        crate::lineage::LINEAGE_DAYS
    )
}

/// `table_lineage_sql`'s rows as the two sides: what feeds `full` and what reads it.
pub(crate) fn lineage_from_rows(rows: &[Vec<Option<String>>], full: &str) -> (Lineage, Lineage) {
    let mut up = Lineage::default();
    let mut down = Lineage::default();
    for r in rows {
        let c = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
        let me_target = c(6).is_some_and(|n| n.eq_ignore_ascii_case(full));
        let me_source = c(0).is_some_and(|n| n.eq_ignore_ascii_case(full));
        if me_target == me_source {
            continue;
        }
        let entity = c(12).zip(c(13)).map(|(t, id)| Entity {
            kind: crate::lineage::entity_kind(&t),
            id,
            v3: crate::lineage::is_v3(&t),
        });
        let (side, cols, kind_col) = if me_target {
            (&mut up, (1, 2, 3), 4)
        } else {
            (&mut down, (7, 8, 9), 10)
        };
        if let Some(e) = &entity {
            if !side.entities.contains(e) {
                side.entities.push(e.clone());
            }
        }
        let via: Vec<Entity> = entity.into_iter().collect();
        match (c(cols.0), c(cols.1), c(cols.2)) {
            (Some(catalog), Some(schema), Some(name)) => {
                match side.tables.iter_mut().find(|t| {
                    t.catalog.eq_ignore_ascii_case(&catalog)
                        && t.schema.eq_ignore_ascii_case(&schema)
                        && t.name.eq_ignore_ascii_case(&name)
                }) {
                    Some(t) => {
                        for e in via {
                            if !t.via.contains(&e) {
                                t.via.push(e);
                            }
                        }
                    }
                    None => side.tables.push(UpTable {
                        catalog,
                        schema,
                        name,
                        table_type: c(kind_col).unwrap_or_default(),
                        via,
                    }),
                }
            }
            _ if me_target => {
                if let Some(path) = c(5) {
                    if !side.files.iter().any(|(p, _)| *p == path) {
                        side.files.push((path, None));
                    }
                }
            }
            _ => {}
        }
    }
    (up, down)
}

/// The views of the catalog whose lower-cased definition contains `:p0` (a `LIKE` pattern).
fn views_naming_sql(catalog: &str) -> String {
    format!(
        "SELECT table_schema, table_name, view_definition FROM {}.information_schema.views \
         WHERE table_schema <> 'information_schema' AND lower(view_definition) LIKE :p0",
        dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, catalog)
    )
}

/// The foreign keys that reference table `:p1` of schema `:p0`: constraint, schema, table.
fn foreign_key_children_sql(catalog: &str) -> String {
    let is = format!(
        "{}.information_schema",
        dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, catalog)
    );
    format!(
        "SELECT DISTINCT k.constraint_name, k.table_schema, k.table_name \
         FROM {is}.referential_constraints r \
         JOIN {is}.key_column_usage k ON k.constraint_schema = r.constraint_schema AND k.constraint_name = r.constraint_name \
         JOIN {is}.key_column_usage u ON u.constraint_schema = r.unique_constraint_schema AND u.constraint_name = r.unique_constraint_name \
         WHERE u.table_schema = :p0 AND u.table_name = :p1"
    )
}

/// Adds what reads the table, from lineage's `downstreams`: this catalog's tables as confirmed
/// items; the notebooks, jobs, pipelines, queries and dashboards that read it, and the tables
/// of other catalogs, as readers outside (`external_users`).
pub(crate) fn merge_downstream(
    report: &mut DependencyReport,
    l: &Lineage,
    catalog: &str,
    target: &dbine_driver::ObjectRef,
    base: &str,
    names: &[(String, String)],
) {
    for t in &l.tables {
        let same = t.catalog.eq_ignore_ascii_case(catalog)
            && t.name.eq_ignore_ascii_case(&target.name)
            && target
                .schema()
                .is_some_and(|s| s.eq_ignore_ascii_case(&t.schema));
        if same {
            continue;
        }
        if t.catalog.eq_ignore_ascii_case(catalog) {
            add(
                &mut report.items,
                Dependent {
                    kind: crate::stats::kind_of(&t.table_type).to_string(),
                    schema: Some(t.schema.clone()),
                    name: t.name.clone(),
                    parent: None,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: via_detail(&t.via),
                    mentions: Vec::new(),
                },
            );
        } else {
            add_external(
                &mut report.external_users,
                ExternalRef {
                    system: Some("Databricks".into()),
                    database: Some(t.catalog.clone()),
                    schema: Some(t.schema.clone()),
                    name: Some(t.name.clone()),
                    detail: Some(t.table_type.clone()).filter(|x| !x.is_empty()),
                    ..Default::default()
                },
            );
        }
    }
    for e in &l.entities {
        let name = names
            .iter()
            .find(|(id, _)| *id == e.id)
            .map_or_else(|| e.id.clone(), |(_, n)| n.clone());
        add_external(
            &mut report.external_users,
            ExternalRef {
                system: Some(format!("Databricks {}", e.kind)),
                name: Some(name),
                detail: entity_url(base, e),
                ..Default::default()
            },
        );
    }
}

/// The names of `body` split: those in `catalog` (the catalog qualifier
/// dropped), and those qualified with another one, as sources outside.
pub(crate) fn split_names(
    body: &str,
    used: Vec<UsedName>,
    catalog: &str,
    system: &str,
) -> (Vec<UsedName>, Vec<ExternalRef>) {
    let mut here = Vec::new();
    let mut outside: Vec<ExternalRef> = Vec::new();
    for mut u in used {
        if u.quals.len() >= 2 {
            if !u.quals[1].eq_ignore_ascii_case(catalog) || u.quals.len() > 2 {
                let x = ExternalRef {
                    system: Some(system.to_string()),
                    database: u.quals.get(1).cloned(),
                    schema: u.quals.first().cloned(),
                    name: Some(u.name.clone()),
                    mentions: mentions_at(body, &[u.at]),
                    ..Default::default()
                };
                add_external(&mut outside, x);
                continue;
            }
            u.quals.truncate(1);
        }
        here.push(u);
    }
    (here, outside)
}

/// What the contract's default does with a definition, except that names
/// of another catalog are sources outside: the objects `body` names in
/// `catalog` (one lookup per name), Probable, with their lines.
pub(crate) async fn code_refs<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    scan: &DependencyScan,
    catalog: &str,
    system: &str,
    report: &mut DependencyReport,
) -> Result<()> {
    let t = &target.object;
    let body = match s.definition(t).await {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(()),
        Err(_) => {
            report.unreadable.push(match t.schema() {
                Some(sc) => format!("{sc}.{}", t.name),
                None => t.name.clone(),
            });
            return Ok(());
        }
    };
    report.scanned = 1;
    let (here, outside) = split_names(&body, used_names(&body, &scan.dialect), catalog, system);
    let objects = resolve_names(s, &here).await?;
    for d in used_objects(&body, &here, &objects, t) {
        add(&mut report.items, d);
    }
    for x in outside {
        add_external(&mut report.external, x);
    }
    Ok(())
}

/// The objects named like `used`: one lookup each, or the whole list when
/// there are many (as the contract's default does).
async fn resolve_names<S: Session + ?Sized>(s: &mut S, used: &[UsedName]) -> Result<Vec<DbObject>> {
    let mut names: Vec<&str> = Vec::new();
    for u in used {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&u.name)) {
            names.push(&u.name);
        }
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() > LOOKUP_CAP {
        return s.list_objects().await;
    }
    let mut found = Vec::new();
    for name in names {
        let q = ObjectQuery {
            name: Some(name.to_string()),
            limit: 50,
            ..Default::default()
        };
        match s.list_objects_page(&q).await {
            Ok(page) => found.extend(page.objects),
            Err(Error::Unsupported(_)) => return s.list_objects().await,
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

/// One mention per line of `body` at these offsets, as the contract keeps
/// them (at most [`MAX_MENTIONS`], each line cut at [`LINE_CHARS`]).
pub(crate) fn mentions_at(body: &str, at: &[usize]) -> Vec<Mention> {
    let mut at: Vec<usize> = at.iter().copied().filter(|&a| a < body.len()).collect();
    at.sort_unstable();
    let mut out: Vec<Mention> = Vec::new();
    for a in at {
        let line = body[..a].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
        if out.last().is_some_and(|m| m.line == line) {
            continue;
        }
        if out.len() == MAX_MENTIONS {
            break;
        }
        let from = body[..a].rfind('\n').map_or(0, |p| p + 1);
        let to = body[a..].find('\n').map_or(body.len(), |p| a + p);
        out.push(Mention {
            line,
            text: body[from..to].trim().chars().take(LINE_CHARS).collect(),
            dynamic: false,
        });
    }
    out
}

fn same(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    }
}

/// Adds `d`, or merges it into the same object already there: the surer
/// confidence, the detail and the mentions of both.
pub(crate) fn add(items: &mut Vec<Dependent>, d: Dependent) {
    match items.iter_mut().find(|x| {
        x.relation == d.relation
            && same(&x.schema, &d.schema)
            && x.name.eq_ignore_ascii_case(&d.name)
    }) {
        Some(x) => {
            if d.confidence < x.confidence {
                x.confidence = d.confidence;
                x.kind = d.kind;
            }
            if x.detail.is_none() {
                x.detail = d.detail;
            }
            for m in d.mentions {
                if !x.mentions.iter().any(|y| y.line == m.line) {
                    x.mentions.push(m);
                }
            }
            x.mentions.sort_by_key(|m| m.line);
        }
        None => items.push(d),
    }
}

/// Adds `x`, or merges it into the same source already there: lineage's
/// detail, the mentions of both.
pub(crate) fn add_external(list: &mut Vec<ExternalRef>, x: ExternalRef) {
    match list.iter_mut().find(|e| {
        same(&e.system, &x.system)
            && same(&e.server, &x.server)
            && same(&e.database, &x.database)
            && same(&e.schema, &x.schema)
            && same(&e.name, &x.name)
    }) {
        Some(e) => {
            if e.detail.is_none() {
                e.detail = x.detail;
            }
            e.mentions.extend(x.mentions);
            e.mentions.sort_by_key(|m| m.line);
            e.mentions.dedup_by_key(|m| m.line);
        }
        None => list.push(x),
    }
}

/// Confirmed first, then by relation and name (as the contract sorts).
pub(crate) fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::tests::{serve, session};
    use dbine_driver::ObjectRef;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const FIXTURE: &str = include_str!("../fixtures/table_lineage.json");

    fn target(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    #[test]
    fn the_request() {
        assert_eq!(LINEAGE_PATH, "/api/2.0/lineage-tracking/table-lineage");
        assert_eq!(
            lineage_query("main", "sales", "dinner"),
            [
                ("table_name", "main.sales.dinner".to_string()),
                ("include_entity_lineage", "true".to_string())
            ]
        );
    }

    #[test]
    fn upstreams_of_the_fixture() {
        let l = parse_lineage(&serde_json::from_str(FIXTURE).unwrap());
        let tables: Vec<_> = l
            .tables
            .iter()
            .map(|t| {
                (
                    t.catalog.as_str(),
                    t.schema.as_str(),
                    t.name.as_str(),
                    t.table_type.as_str(),
                    t.via.len(),
                )
            })
            .collect();
        // The masked table (no catalog) is left out; downstreams too.
        assert_eq!(
            tables,
            [
                ("main", "sales", "menu", "TABLE", 2),
                ("ref", "fx", "rates", "VIEW", 1)
            ]
        );
        assert_eq!(
            l.files,
            [(
                "s3://landing/orders/2026/?X-Amz-Signature=abc".to_string(),
                Some("landing".to_string())
            )]
        );
        let entities: Vec<_> = l
            .entities
            .iter()
            .map(|e| (e.kind.as_str(), e.id.as_str()))
            .collect();
        assert_eq!(
            entities,
            [
                ("job", "774"),
                ("notebook", "1111169262439324"),
                ("query", "a1b2c3d4-0000-4000-8000-000000000001"),
                ("pipeline", "0f3e5d2a-1111-4222-8333-944455556666"),
                ("notebook", "2222"),
            ]
        );
    }

    #[test]
    fn lineage_becomes_confirmed_items_and_sources_outside() {
        let l = parse_lineage(&serde_json::from_str(FIXTURE).unwrap());
        let mut report = DependencyReport::default();
        // What the definition found: the same table, Probable, with its line.
        add(
            &mut report.items,
            Dependent {
                kind: kinds::TABLE.into(),
                schema: Some("sales".into()),
                name: "menu".into(),
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: vec![Mention {
                    line: 2,
                    text: "FROM main.sales.menu".into(),
                    dynamic: false,
                }],
            },
        );
        let names = [("774".to_string(), "nightly load".to_string())];
        merge_lineage(
            &mut report,
            &l,
            "MAIN",
            &target(kinds::VIEW, "sales", "dinner"),
            "https://adb-1.azuredatabricks.net/",
            &names,
        );
        assert_eq!(report.items.len(), 1);
        let menu = &report.items[0];
        assert_eq!(
            (menu.confidence, menu.mentions.len(), menu.detail.as_deref()),
            (
                Confidence::Confirmed,
                1,
                Some("job 774 · notebook 1111169262439324")
            )
        );
        let outside: Vec<_> = report
            .external
            .iter()
            .map(|x| {
                (
                    x.system.as_deref().unwrap_or(""),
                    x.database.as_deref(),
                    x.name.as_deref().unwrap_or(""),
                    x.detail.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            outside,
            [
                ("Databricks", Some("ref"), "rates", Some("VIEW")),
                ("Amazon S3", None, "s3://landing/orders/2026/", Some("landing")),
                ("Databricks job", None, "nightly load", Some("https://adb-1.azuredatabricks.net/jobs/774")),
                (
                    "Databricks notebook",
                    None,
                    "1111169262439324",
                    Some("https://adb-1.azuredatabricks.net/#notebook/1111169262439324")
                ),
                (
                    "Databricks query",
                    None,
                    "a1b2c3d4-0000-4000-8000-000000000001",
                    Some("https://adb-1.azuredatabricks.net/sql/editor/a1b2c3d4-0000-4000-8000-000000000001")
                ),
                (
                    "Databricks pipeline",
                    None,
                    "0f3e5d2a-1111-4222-8333-944455556666",
                    Some("https://adb-1.azuredatabricks.net/pipelines/0f3e5d2a-1111-4222-8333-944455556666")
                ),
                ("Databricks notebook", None, "2222", Some("https://adb-1.azuredatabricks.net/#notebook/2222")),
            ]
        );
        // The target itself never comes back as what it uses.
        let mut r = DependencyReport::default();
        merge_lineage(
            &mut r,
            &l,
            "main",
            &target(kinds::TABLE, "sales", "menu"),
            "https://h",
            &[],
        );
        assert!(r.items.is_empty());
    }

    #[test]
    fn entities_of_every_kind() {
        let e = entities(&json!({
            "dashboardV3Infos": [{ "workspace_id": 1, "dashboard_id": "d3" }],
            "dashboardInfos": [{ "workspace_id": 1, "dashboard_id": "d1" }],
            "modelVersionInfos": [{ "workspace_id": 1, "model_name": "m", "version": 2 }],
            "fooInfos": [{ "workspace_id": 1, "foo_run_id": 3, "foo_id": 7 }]
        }));
        let got: Vec<_> = e
            .iter()
            .map(|x| (x.kind.as_str(), x.id.as_str(), x.v3))
            .collect();
        // By key, as serde_json keeps an object.
        assert_eq!(
            got,
            [
                ("dashboard", "d1", false),
                ("dashboard", "d3", true),
                ("foo", "7", false)
            ]
        );
        assert_eq!(
            entity_url("https://h", &e[0]).as_deref(),
            Some("https://h/sql/dashboards/d1")
        );
        assert_eq!(
            entity_url("https://h", &e[1]).as_deref(),
            Some("https://h/dashboardsv3/d3")
        );
        assert_eq!(entity_url("https://h", &e[2]), None);
    }

    #[test]
    fn names_of_another_catalog_are_sources_outside() {
        let body = "CREATE VIEW main.sales.v AS\nSELECT * FROM main.sales.orders o\nJOIN `ref`.`fx`.`rates` r ON r.c = o.c\nJOIN sales.lines l USING (id)";
        let used = used_names(body, &crate::script::dialect());
        let (here, outside) = split_names(body, used, "main", "Databricks");
        let here: Vec<_> = here
            .iter()
            .map(|u| (u.quals.clone(), u.name.as_str()))
            .collect();
        assert_eq!(
            here,
            [
                (vec!["sales".to_string()], "orders"),
                (vec!["sales".to_string()], "lines")
            ]
        );
        assert_eq!(
            (
                outside[0].database.as_deref(),
                outside[0].schema.as_deref(),
                outside[0].name.as_deref()
            ),
            (Some("ref"), Some("fx"), Some("rates"))
        );
        assert_eq!(outside[0].mentions[0].line, 3);
    }

    #[test]
    fn locations() {
        assert_eq!(
            storage_system("abfss://c@acct.dfs.core.windows.net/x"),
            "Azure Data Lake Storage"
        );
        assert_eq!(
            storage_system("/Volumes/main/raw/files/a.csv"),
            "Unity Catalog volume"
        );
        assert_eq!(
            clean_location("abfss://c@acct.dfs.core.windows.net/x?sig=1"),
            "abfss://acct.dfs.core.windows.net/x"
        );
    }

    /// A workspace whose lineage API answers with the fixture (or `status`),
    /// a job and a pipeline by id; SQL statements fail (no warehouse here).
    async fn workspace(status: u16, seen: Arc<Mutex<Vec<String>>>) -> String {
        serve(Arc::new(move |m, p, _| {
            seen.lock().unwrap().push(format!("{m} {p}"));
            let (path, _) = p.split_once('?').unwrap_or((p, ""));
            match (m, path) {
                ("GET", LINEAGE_PATH) if status == 200 => (200, FIXTURE.to_string()),
                ("GET", LINEAGE_PATH) => (
                    status,
                    json!({ "error_code": "PERMISSION_DENIED", "message": "no" }).to_string(),
                ),
                ("GET", "/api/2.1/jobs/get") => (
                    200,
                    json!({ "job_id": 774, "settings": { "name": "nightly load" } }).to_string(),
                ),
                ("GET", "/api/2.0/pipelines/0f3e5d2a-1111-4222-8333-944455556666") => {
                    (200, json!({ "name": "orders bronze" }).to_string())
                }
                _ => (
                    404,
                    json!({ "error_code": "NOT_FOUND", "message": "no" }).to_string(),
                ),
            }
        }))
        .await
    }

    fn scan() -> DependencyScan {
        DependencyScan {
            source_kinds: vec![kinds::VIEW.into()],
            dialect: crate::script::dialect(),
            foreign_keys: true,
            written_kinds: vec![
                kinds::VIEW.into(),
                kinds::MATERIALIZED_VIEW.into(),
                kinds::FUNCTION.into(),
            ],
            has_schemas: true,
        }
    }

    #[tokio::test]
    async fn a_tables_lineage_from_the_api() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = session(workspace(200, seen.clone()).await);
        s.catalog = Some("main".into());
        let t = DependencyTarget {
            object: target(kinds::TABLE, "sales", "dinner"),
            column: None,
        };
        let r = s.references_of(&t, &scan()).await.unwrap();
        assert_eq!(r.note, None);
        let items: Vec<_> = r
            .items
            .iter()
            .map(|d| (d.name.as_str(), d.confidence))
            .collect();
        assert_eq!(items, [("menu", Confidence::Confirmed)]);
        let names: Vec<_> = r
            .external
            .iter()
            .filter_map(|x| x.name.as_deref())
            .collect();
        assert!(
            names.contains(&"nightly load") && names.contains(&"orders bronze"),
            "{names:?}"
        );
        // The foreign keys need the warehouse: left out, said so.
        assert_eq!(r.unreadable, ["sales.dinner"]);
        let asked = seen.lock().unwrap().clone();
        assert!(
            asked.iter().any(|a| a == "GET /api/2.0/lineage-tracking/table-lineage?table_name=main.sales.dinner&include_entity_lineage=true"),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn without_lineage_a_note() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = session(workspace(403, seen.clone()).await);
        s.catalog = Some("main".into());
        let t = DependencyTarget {
            object: target(kinds::TABLE, "sales", "dinner"),
            column: None,
        };
        let r = s.references_of(&t, &scan()).await.unwrap();
        assert_eq!(r.note.as_deref(), Some(NO_LINEAGE));
        assert!(r.items.is_empty() && r.external.is_empty());
        // hive_metastore has none: not even asked.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = session(workspace(200, seen.clone()).await);
        s.catalog = Some(HIVE_METASTORE.into());
        let r = s.references_of(&t, &scan()).await.unwrap();
        assert_eq!(r.note.as_deref(), Some(NO_LINEAGE));
        assert!(!seen.lock().unwrap().iter().any(|a| a.contains("lineage")));
    }

    // --- What reads a table: the lineage API, not the catalog's definitions.

    const VIEW_BODY: &str = "CREATE VIEW sales.v_dinner AS SELECT * FROM sales.dinner";

    /// A workspace for `dependents_of`: the table-lineage API (`pages` of it), the names of what
    /// ran, and the two catalog queries it may ask (views naming the table, foreign keys to it).
    /// Gives the base URL and what was asked (`METHOD path?query`, statements by their text).
    async fn readers_workspace(
        status: u16,
        pages: bool,
        system: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let base = serve(Arc::new(move |m, p, body| {
            let (path, query) = p.split_once('?').unwrap_or((p, ""));
            log.lock().unwrap().push(format!("{m} {p}"));
            let rows = |r: Json| {
                json!({ "statement_id": "s", "status": { "state": "SUCCEEDED" }, "result": { "data_array": r } })
                    .to_string()
            };
            match (m, path) {
                ("GET", LINEAGE_PATH) if status != 200 => (
                    status,
                    json!({ "error_code": "PERMISSION_DENIED", "message": "no" }).to_string(),
                ),
                ("GET", LINEAGE_PATH) if pages && !query.contains("page_token") => (
                    200,
                    json!({
                        "downstreams": [{ "tableInfo": { "name": "page_one", "catalog_name": "main", "schema_name": "sales", "table_type": "TABLE" } }],
                        "next_page_token": "t2"
                    })
                    .to_string(),
                ),
                ("GET", LINEAGE_PATH) if pages => (
                    200,
                    json!({ "downstreams": [{ "tableInfo": { "name": "page_two", "catalog_name": "main", "schema_name": "sales", "table_type": "TABLE" } }] })
                        .to_string(),
                ),
                ("GET", LINEAGE_PATH) => (200, FIXTURE.to_string()),
                ("GET", "/api/2.1/jobs/get") => (
                    200,
                    json!({ "job_id": 774, "settings": { "name": "nightly load" } }).to_string(),
                ),
                // The catalog's listing, for the contract's scan the fallback runs.
                ("GET", "/api/2.1/unity-catalog/catalogs/main") => (200, r#"{"name":"main"}"#.into()),
                ("GET", "/api/2.1/unity-catalog/table-summaries") => (
                    200,
                    r#"{"tables":[{"full_name":"main.sales.dinner","table_type":"MANAGED"},{"full_name":"main.sales.v_dinner","table_type":"VIEW"}]}"#.into(),
                ),
                ("GET", "/api/2.1/unity-catalog/schemas") => (200, r#"{"schemas":[{"name":"sales"}]}"#.into()),
                ("POST", "/api/2.0/sql/statements") => {
                    let st: Json = serde_json::from_str(body).unwrap();
                    let text = st["statement"].as_str().unwrap_or("");
                    if text.contains("system.access.table_lineage") {
                        if system {
                            (200, rows(json!([
                                ["main.sales.menu", "main", "sales", "menu", "TABLE", null, "main.sales.dinner", "main", "sales", "dinner", "TABLE", null, "JOB", "774"],
                                ["main.sales.dinner", "main", "sales", "dinner", "TABLE", null, "main.sales.dinner_price", "main", "sales", "dinner_price", "TABLE", null, "NOTEBOOK", "3333"],
                                ["main.sales.dinner", "main", "sales", "dinner", "TABLE", null, "ref.fx.rates_copy", "ref", "fx", "rates_copy", "TABLE", null, null, null],
                                [null, null, null, null, "PATH", "s3://landing/orders/", "main.sales.dinner", "main", "sales", "dinner", "TABLE", null, null, null]
                            ])))
                        } else {
                            (200, json!({ "statement_id": "s", "status": { "state": "FAILED", "error": { "message": "no SELECT on system.access.table_lineage" } } }).to_string())
                        }
                    } else if text.contains("information_schema.views") {
                        (200, rows(json!([["sales", "v_dinner", VIEW_BODY], ["sales", "other", "SELECT 1"]])))
                    } else if text.contains("referential_constraints") {
                        (200, rows(json!([["fk_side", "sales", "side"]])))
                    } else {
                        (200, rows(json!([])))
                    }
                }
                _ => (404, json!({ "error_code": "NOT_FOUND", "message": "no" }).to_string()),
            }
        }))
        .await;
        (base, seen)
    }

    fn dinner() -> DependencyTarget {
        DependencyTarget {
            object: target(kinds::TABLE, "sales", "dinner"),
            column: None,
        }
    }

    #[tokio::test]
    async fn what_reads_a_table_comes_from_its_lineage_not_from_the_catalog() {
        let (base, seen) = readers_workspace(200, false, true).await;
        let mut s = session(base.clone());
        s.catalog = Some("main".into());
        let r = s.dependents_of(&dinner(), &scan()).await.unwrap();
        assert_eq!(r.items.len(), 3, "{:?}", r.items);
        let by = |n: &str| {
            r.items
                .iter()
                .find(|d| d.name == n)
                .unwrap_or_else(|| panic!("{n} in {:?}", r.items))
        };
        assert_eq!(by("dinner_price").confidence, Confidence::Confirmed);
        assert_eq!(by("dinner_price").detail.as_deref(), Some("notebook 3333"));
        assert_eq!(by("v_dinner").relation, Relation::Code);
        assert_eq!(by("side").relation, Relation::ForeignKey);
        assert_eq!(by("side").confidence, Confidence::Confirmed);
        // The notebook that read it is a reader outside, with its link.
        let nb = r
            .external_users
            .iter()
            .find(|x| x.system.as_deref() == Some("Databricks notebook"))
            .expect("the notebook");
        assert_eq!(
            nb.detail.as_deref(),
            Some(format!("{base}/#notebook/3333").as_str())
        );
        // A reader in another catalog is outside.
        assert!(r.external_users.iter().any(
            |x| x.database.as_deref() == Some("ref") && x.name.as_deref() == Some("rates_copy")
        ));
        // The system table (this table only, both directions, the window) and the two filtered
        // catalog queries; the API is not asked, nothing is listed or read definition by definition.
        let asked = seen.lock().unwrap().clone();
        assert!(
            !asked.iter().any(|a| a.contains("table-lineage")),
            "{asked:?}"
        );
        assert_eq!(asked.iter().filter(|a| a.starts_with("POST")).count(), 3);
        assert!(
            !asked
                .iter()
                .any(|a| a.contains("table-summaries") || a.contains("/schemas")),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn the_system_table_is_asked_for_one_table_in_both_directions() {
        let (base, _) = readers_workspace(200, false, true).await;
        let sql = table_lineage_sql();
        assert!(sql.contains("target_table_full_name = :p0 OR source_table_full_name = :p0"));
        assert!(sql.contains(&format!(
            "date_sub(current_date(), {})",
            crate::lineage::LINEAGE_DAYS
        )));
        assert!(sql.contains("GROUP BY ALL") && sql.contains("LIMIT 5000"));
        // What feeds it, from the same rows.
        let mut s = session(base);
        s.catalog = Some("main".into());
        let r = s.references_of(&dinner(), &scan()).await.unwrap();
        assert_eq!(r.note, None);
        let menu = r.items.iter().find(|d| d.name == "menu").expect("menu");
        assert_eq!(menu.confidence, Confidence::Confirmed);
        assert_eq!(menu.detail.as_deref(), Some("job 774"));
        let names: Vec<_> = r
            .external
            .iter()
            .filter_map(|x| x.name.as_deref())
            .collect();
        assert!(names.contains(&"nightly load"), "{names:?}");
        assert!(names.contains(&"s3://landing/orders/"), "{names:?}");
    }

    #[tokio::test]
    async fn the_lineage_pages_are_followed() {
        let (base, seen) = readers_workspace(200, true, false).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let r = s.dependents_of(&dinner(), &scan()).await.unwrap();
        for n in ["page_one", "page_two"] {
            assert!(r.items.iter().any(|d| d.name == n), "{n}: {:?}", r.items);
        }
        let asked = seen.lock().unwrap().clone();
        assert!(
            asked.iter().any(|a| a.contains("page_token=t2")),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn without_lineage_access_the_contracts_scan_runs_with_the_note() {
        let (base, seen) = readers_workspace(403, false, false).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let r = s.dependents_of(&dinner(), &scan()).await.unwrap();
        assert_eq!(r.note.as_deref(), Some(NO_LINEAGE));
        let asked = seen.lock().unwrap().clone();
        assert!(asked.iter().any(|a| a.contains("table-lineage")));
        // The contract's scan listed the catalog.
        assert!(
            asked.iter().any(|a| a.contains("table-summaries")),
            "{asked:?}"
        );
        // hive_metastore has no lineage: not even asked.
        let (base, seen) = readers_workspace(200, false, true).await;
        let mut s = session(base);
        s.catalog = Some(HIVE_METASTORE.into());
        let _ = s.dependents_of(&dinner(), &scan()).await;
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.contains("table-lineage")));
    }

    #[tokio::test]
    async fn names_are_looked_up_once_and_dashboards_and_queries_get_theirs() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let base = serve(Arc::new(move |m, p, _| {
            log.lock().unwrap().push(format!("{m} {p}"));
            let (path, _) = p.split_once('?').unwrap_or((p, ""));
            match path {
                "/api/2.1/jobs/get" => (
                    200,
                    json!({ "settings": { "name": "nightly load" } }).to_string(),
                ),
                "/api/2.0/sql/queries/q1" => {
                    (200, json!({ "display_name": "Top orders" }).to_string())
                }
                "/api/2.0/preview/sql/queries/q2" => {
                    (200, json!({ "name": "Legacy query" }).to_string())
                }
                "/api/2.0/lakeview/dashboards/d3" => {
                    (200, json!({ "display_name": "Sales board" }).to_string())
                }
                "/api/2.0/preview/sql/dashboards/d1" => {
                    (200, json!({ "name": "Old board" }).to_string())
                }
                _ => (404, json!({ "message": "no" }).to_string()),
            }
        }))
        .await;
        let s = session(base);
        let e = |kind: &str, id: &str, v3: bool| Entity {
            kind: kind.into(),
            id: id.into(),
            v3,
        };
        let mut list = vec![
            e("job", "774", false),
            e("query", "q1", false),
            e("query", "q2", false),
            e("dashboard", "d3", true),
            e("dashboard", "d1", false),
            e("notebook", "3271915304643002", false),
            e("job", "774", false),
        ];
        // More jobs than the parallelism: all are named.
        list.extend((0..20).map(|i| e("job", &format!("9{i}"), false)));
        let mut names = s.entity_names(&list).await;
        names.sort();
        let find = |id: &str| names.iter().find(|(i, _)| i == id).map(|(_, n)| n.as_str());
        assert_eq!(find("774"), Some("nightly load"));
        assert_eq!(find("q1"), Some("Top orders"));
        assert_eq!(find("q2"), Some("Legacy query"));
        assert_eq!(find("d3"), Some("Sales board"));
        assert_eq!(find("d1"), Some("Old board"));
        assert_eq!(find("90"), Some("nightly load"));
        assert_eq!(
            find("3271915304643002"),
            None,
            "no call names a notebook by its id"
        );
        // The unrepeated ones were asked once: 21 jobs, 2 queries (+1 legacy), 1 + 1 dashboards.
        let first = seen.lock().unwrap().len();
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.contains("3271915304643002")));
        // Asked again: from memory, no new calls.
        let again = s.entity_names(&list).await;
        assert_eq!(again.len(), names.len());
        assert_eq!(seen.lock().unwrap().len(), first);
    }
}

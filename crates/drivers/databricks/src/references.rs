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
use serde_json::Value as Json;

pub(crate) const LINEAGE_PATH: &str = "/api/2.0/lineage-tracking/table-lineage";
/// Names of jobs and pipelines looked up for one report, at most.
const NAME_LOOKUPS: usize = 10;

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
    let mut l = Lineage::default();
    for entry in resp
        .get("upstreams")
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
            // A table: its foreign keys (on the warehouse). Lineage doesn't
            // need it: a failure only leaves the keys out.
            match references_scan(self, target, scan).await {
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
                self.api
                    .get_query(LINEAGE_PATH, &lineage_query(&catalog, &schema, &t.name))
                    .await
            };
            match lineage {
                Ok(resp) => {
                    let l = parse_lineage(&resp);
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

    /// Jobs' and pipelines' names (`jobs/get`, `pipelines/{id}`), at most
    /// [`NAME_LOOKUPS`]; one that can't be read keeps its id.
    pub(crate) async fn entity_names(&self, entities: &[Entity]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for e in entities
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "job" | "pipeline"))
            .take(NAME_LOOKUPS)
        {
            let name = if e.kind == "job" {
                self.api
                    .get_query("/api/2.1/jobs/get", &[("job_id", e.id.clone())])
                    .await
                    .ok()
                    .and_then(|j| j.pointer("/settings/name")?.as_str().map(str::to_string))
            } else {
                self.api
                    .get(&format!("/api/2.0/pipelines/{}", escape(&e.id)))
                    .await
                    .ok()
                    .and_then(|p| p.get("name")?.as_str().map(str::to_string))
            };
            if let Some(n) = name {
                out.push((e.id.clone(), n));
            }
        }
        out
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
}

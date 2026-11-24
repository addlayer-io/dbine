//! What reads a table (`Session::dependents`) and the lineage of a dataset
//! (`Session::lineage_graph`), from BigQuery's own sources first.
//!
//! The generic scan reads every object's definition, and here each definition is a
//! billed query job (`Capabilities::billed_metadata`), so it is only the last fallback.
//! In this order:
//!
//! 1. **Data Lineage API** (`datalineage.googleapis.com`), Confirmed. It records what
//!    ran (query jobs with DDL and DML, copy and load jobs) after the API was enabled,
//!    for 30 days, and answers one level per call.
//!    - `POST /v1/projects/{p}/locations/{l}:searchLinks` with `source` (what reads an
//!      asset) or `target` (what feeds it): `{ "source": { "fullyQualifiedName":
//!      "bigquery:project.dataset.table" } }`, `pageSize` up to 100, `nextPageToken`.
//!      Needs `datalineage.locations.searchLinks` (`roles/datalineage.viewer`). The
//!      location is the one the transformation ran in, the dataset's.
//!    - `POST …:batchSearchLinkProcesses` with up to 100 link names gives each link's
//!      processes; `GET /v1/{process}` gives the process's `displayName` and
//!      `origin.sourceType` (`BIGQUERY`, `COMPOSER`, `DATAFLOW`, `LOOKER_STUDIO`…).
//!    - Fully qualified names: `bigquery:{project}.{dataset}.{table}`, a part with a dot,
//!      a colon or a space in backticks (a backtick doubled).
//!    - Docs: <https://cloud.google.com/dataplex/docs/about-data-lineage> (what is
//!      tracked, 30 days, API enabled per project),
//!      <https://cloud.google.com/dataplex/docs/fully-qualified-names>,
//!      <https://cloud.google.com/data-catalog/docs/reference/data-lineage/rest/v1/projects.locations/searchLinks>,
//!      <https://cloud.google.com/data-catalog/docs/reference/data-lineage/rest/v1/projects.locations/batchSearchLinkProcesses>,
//!      <https://cloud.google.com/data-catalog/docs/reference/data-lineage/rest/v1/projects.locations.processes>,
//!      <https://cloud.google.com/dataplex/docs/iam-roles> (`roles/datalineage.viewer`).
//! 2. **`INFORMATION_SCHEMA.JOBS_BY_PROJECT`**, Confirmed, when the API is off, forbidden
//!    or has nothing: `referenced_tables` and `destination_table` of the query jobs that
//!    finished OK in the last [`JOBS_DAYS`] days. The view keeps 180 days, is
//!    partitioned by `creation_time` (the filter prunes) and region-qualified
//!    (`` `region-us`.INFORMATION_SCHEMA.JOBS_BY_PROJECT ``, run in that location), and
//!    needs `bigquery.jobs.listAll`. It is billed by the bytes it scans, so the query
//!    never selects the `query` text, and a dry run runs first: past [`JOBS_MAX_BYTES`]
//!    it is not run. Docs: <https://cloud.google.com/bigquery/docs/information-schema-jobs>.
//! 3. **The dataset's definitions in one query** (`INFORMATION_SCHEMA.TABLES.ddl` for
//!    views and materialized views, `ROUTINES.ddl` for routines, one `UNION ALL`, the
//!    needle filtered by the server), Probable, plus one query on `TABLE_CONSTRAINTS` ⋈
//!    `CONSTRAINT_COLUMN_USAGE` for the foreign keys that point at a table. A metadata
//!    view is billed the 10 MB minimum.
//! 4. **The contract's scan** only when those queries can't be read.
//!
//! What a table feeds in other datasets of the project comes from 1 and 2 only: a
//! definition is searched in the dataset of the object asked for.
//!
//! Nothing here fails the panel: each source that can't be read says so in a note and
//! the next one answers.

use crate::jobs::{text, Dts, Failed};
use crate::references::{add, body_names, mentions_at, sort, Name};
use crate::{Api, BigQuerySession};
use dbine_driver::dependencies::{
    self, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent, ExternalRef,
    LineageEdge, LineageExternal, LineageGraph, LineageJob, LineageScope, Relation, JOBS_READ,
};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, HashMap};

/// The Data Lineage API.
pub(crate) const API: &str = "https://datalineage.googleapis.com";
/// Links per `searchLinks` page (the service's maximum) and pages read.
const LINKS_PAGE: u32 = 100;
const LINKS_PAGES: usize = 5;
/// `batchSearchLinkProcesses` takes this many links at most.
const PROCESS_LINKS: usize = 100;
/// Processes whose name is read, at most, eight at a time.
const PROCESS_NAMES: usize = 20;
const PARALLEL: usize = 8;
/// How far back the jobs are read (days). The view keeps 180; a short window prunes the
/// partitions and a line that hasn't run lately still comes from the definitions.
pub(crate) const JOBS_DAYS: u32 = 14;
/// Rows the jobs query returns at most.
const JOBS_ROWS: u32 = 5000;
/// Bytes the jobs query may scan (its dry run): about 0.006 USD on demand.
pub(crate) const JOBS_MAX_BYTES: u64 = 1 << 30;
/// Tables whose lineage a dataset's graph asks the API for, one call each.
const GRAPH_TABLES: usize = 150;
/// Rows the definitions query returns at most.
const TEXT_ROWS: u32 = 5000;

pub(crate) const API_OFF: &str = "La API de Data Lineage no está habilitada en el proyecto (solo registra los trabajos que corran después de habilitarla).";
pub(crate) const API_DENIED: &str = "Este usuario no puede leer Data Lineage (hace falta el rol roles/datalineage.viewer y la API habilitada en el proyecto).";
pub(crate) const API_FAILED: &str = "No se pudo leer Data Lineage.";
pub(crate) const FROM_JOBS: &str = "Las líneas confirmadas salen del historial de trabajos de los últimos 14 días (INFORMATION_SCHEMA.JOBS_BY_PROJECT).";
pub(crate) const JOBS_BIG: &str = "El historial de trabajos pesa demasiado para leerlo sin costo (más de 1 GiB): no se usó, y las líneas salen de las definiciones, sin confirmar.";
pub(crate) const NO_JOBS: &str = "No se pudo leer el historial de trabajos (hace falta bigquery.jobs.listAll): las líneas salen de las definiciones, sin confirmar.";
pub(crate) const NO_CATALOG: &str = "No se pudieron leer las definiciones del dataset de una sola vez (INFORMATION_SCHEMA): se leyeron una por una.";
pub(crate) const API_PARTIAL: &str =
    "Data Lineage no respondió para algunas tablas: les faltan sus líneas confirmadas.";

// -- names -------------------------------------------------------------------------------------

fn fqn_part(s: &str) -> String {
    if s.chars()
        .any(|c| matches!(c, '.' | ':' | '`') || c.is_whitespace())
    {
        format!("`{}`", s.replace('`', "``"))
    } else {
        s.to_string()
    }
}

/// The fully qualified name Data Lineage gives a BigQuery table or view.
pub(crate) fn fqn(project: &str, dataset: &str, table: &str) -> String {
    format!(
        "bigquery:{}.{}.{}",
        fqn_part(project),
        fqn_part(dataset),
        fqn_part(table)
    )
}

/// What a link's end is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Asset {
    Table {
        project: String,
        dataset: String,
        name: String,
    },
    /// Anything else (a Cloud Storage path, a Looker or Spanner asset): the system's word
    /// (`gcs`, `spanner`…) and the rest of the name.
    Other { system: String, name: String },
}

/// Splits `a.`b.c`.d` at the dots outside backticks.
fn split_parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '`' if !quoted => quoted = true,
            '`' if it.peek() == Some(&'`') => {
                cur.push('`');
                it.next();
            }
            '`' => quoted = false,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

pub(crate) fn parse_fqn(s: &str) -> Asset {
    let (system, rest) = s.split_once(':').unwrap_or(("", s));
    if system == "bigquery" {
        if let [p, d, t] = split_parts(rest).as_slice() {
            return Asset::Table {
                project: p.clone(),
                dataset: d.clone(),
                name: t.clone(),
            };
        }
    }
    Asset::Other {
        system: system.to_string(),
        name: rest.to_string(),
    }
}

fn system_word(system: &str) -> String {
    match system {
        "bigquery" => "BigQuery".into(),
        "gcs" => "Cloud Storage".into(),
        "spanner" => "Spanner".into(),
        "bigtable" => "Bigtable".into(),
        "cloudsql" => "Cloud SQL".into(),
        "pubsub" => "Pub/Sub".into(),
        "" => "Data Lineage".into(),
        s => s.to_string(),
    }
}

// -- the API -----------------------------------------------------------------------------------

/// Which way to ask: what an asset feeds (its `source`'s links) or what feeds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Side {
    Down,
    Up,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Link {
    pub name: String,
    pub source: String,
    pub target: String,
    pub end: Option<String>,
}

pub(crate) fn parse_links(page: &Json) -> Vec<Link> {
    page.get("links")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(Link {
                name: text(l, "/name").unwrap_or_default(),
                source: text(l, "/source/fullyQualifiedName")?,
                target: text(l, "/target/fullyQualifiedName")?,
                end: text(l, "/endTime"),
            })
        })
        .collect()
}

/// Why the API gave nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Why {
    /// Not enabled in the project.
    Off,
    /// 401 or 403 for another reason: no role.
    Denied,
    Other,
}

impl Why {
    pub(crate) fn of(f: &Failed) -> Why {
        if f.disabled {
            Why::Off
        } else if matches!(f.status, 401 | 403) {
            Why::Denied
        } else {
            Why::Other
        }
    }

    pub(crate) fn note(self) -> &'static str {
        match self {
            Why::Off => API_OFF,
            Why::Denied => API_DENIED,
            Why::Other => API_FAILED,
        }
    }
}

/// The links of one asset in one direction, every page up to [`LINKS_PAGES`].
pub(crate) async fn search_links(
    api: &Api,
    base: &str,
    location: &str,
    side: Side,
    asset: &str,
) -> std::result::Result<Vec<Link>, Failed> {
    let dts = Dts { api, base };
    let last = format!("{location}:searchLinks");
    let url = dts.url(&["projects", &api.project, "locations", &last])?;
    let key = if side == Side::Down {
        "source"
    } else {
        "target"
    };
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..LINKS_PAGES {
        let mut body = json!({ key: { "fullyQualifiedName": asset }, "pageSize": LINKS_PAGE });
        if let Some(t) = token.take() {
            body["pageToken"] = json!(t);
        }
        let page = dts.send(api.http.post(url.clone()).json(&body)).await?;
        out.extend(parse_links(&page));
        match page.get("nextPageToken").and_then(Json::as_str) {
            Some(t) if !t.is_empty() => token = Some(t.to_string()),
            _ => break,
        }
    }
    Ok(out)
}

/// What a process says about itself, in a few words (`bigquery job-1`, `composer daily`).
pub(crate) fn process_label(p: &Json) -> Option<String> {
    let kind = text(p, "/origin/sourceType")
        .filter(|k| k != "SOURCE_TYPE_UNSPECIFIED" && k != "CUSTOM")
        .map(|k| k.to_lowercase());
    let name = text(p, "/displayName").or_else(|| {
        text(p, "/origin/name").map(|n| n.rsplit('/').next().unwrap_or(&n).to_string())
    });
    let label = [kind, name]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!label.is_empty()).then_some(label)
}

/// Each link's processes' labels, best effort: the links' names in one batch, then each
/// distinct process once (at most [`PROCESS_NAMES`], [`PARALLEL`] at a time). A process
/// that can't be read keeps its id.
pub(crate) async fn link_processes(
    api: &Api,
    base: &str,
    location: &str,
    links: &[Link],
) -> HashMap<String, Vec<String>> {
    let names: Vec<&str> = links
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| !n.is_empty())
        .take(PROCESS_LINKS)
        .collect();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if names.is_empty() {
        return out;
    }
    let dts = Dts { api, base };
    let last = format!("{location}:batchSearchLinkProcesses");
    let Ok(url) = dts.url(&["projects", &api.project, "locations", &last]) else {
        return out;
    };
    // process → its links
    let mut by_process: Vec<(String, Vec<String>)> = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..3 {
        let mut body = json!({ "links": names, "pageSize": PROCESS_LINKS });
        if let Some(t) = token.take() {
            body["pageToken"] = json!(t);
        }
        let Ok(page) = dts.send(api.http.post(url.clone()).json(&body)).await else {
            break;
        };
        for pl in page
            .get("processLinks")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let Some(process) = text(pl, "/process") else {
                continue;
            };
            let links: Vec<String> = pl
                .get("links")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|l| text(l, "/link"))
                .collect();
            by_process.push((process, links));
        }
        match page.get("nextPageToken").and_then(Json::as_str) {
            Some(t) if !t.is_empty() => token = Some(t.to_string()),
            _ => break,
        }
    }
    let mut distinct: Vec<String> = Vec::new();
    for (p, _) in &by_process {
        if !distinct.contains(p) {
            distinct.push(p.clone());
        }
    }
    distinct.truncate(PROCESS_NAMES);
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for p in distinct {
        let (api, gate, base) = (api.clone(), gate.clone(), base.to_string());
        set.spawn(async move {
            let _slot = gate.acquire().await.ok();
            let dts = Dts {
                api: &api,
                base: &base,
            };
            let segments: Vec<&str> = p.split('/').collect();
            let label = match dts.url(&segments) {
                Ok(url) => dts
                    .send(api.http.get(url))
                    .await
                    .ok()
                    .as_ref()
                    .and_then(process_label),
                Err(_) => None,
            };
            (p, label)
        });
    }
    let mut labels: HashMap<String, String> = HashMap::new();
    while let Some(done) = set.join_next().await {
        if let Ok((p, Some(l))) = done {
            labels.insert(p, l);
        }
    }
    for (process, links) in by_process {
        let label = labels
            .get(&process)
            .cloned()
            .unwrap_or_else(|| process.rsplit('/').next().unwrap_or(&process).to_string());
        for l in links {
            let list = out.entry(l).or_default();
            if !list.contains(&label) {
                list.push(label.clone());
            }
        }
    }
    out
}

// -- what a link says --------------------------------------------------------------------------

/// Where the answer is placed: the connection's project and the dataset the session is on.
pub(crate) struct Ctx<'a> {
    pub project: &'a str,
    pub session_dataset: Option<&'a str>,
}

/// An object of this project, or a source outside it.
pub(crate) enum Placed {
    Item(Dependent),
    Outside(ExternalRef),
}

fn day(t: &str) -> &str {
    t.get(..10).unwrap_or(t)
}

/// Data Lineage's words for one line: when it last ran and which processes carried it.
fn link_detail(end: Option<&str>, labels: &[String]) -> String {
    let mut parts = vec!["Data Lineage".to_string()];
    if let Some(e) = end {
        parts.push(day(e).to_string());
    }
    if !labels.is_empty() {
        parts.push(labels.join(", "));
    }
    parts.join(" · ")
}

/// An asset as an item of this project (kind from `kinds`, what `look_up` found) or a
/// source outside. An object that no longer exists (lineage outlives a dropped table for
/// 30 days) and the anonymous datasets of query results are `None`.
pub(crate) fn place(
    ctx: &Ctx,
    a: &Asset,
    kinds_found: &[(String, String, bool, String)],
    detail: String,
) -> Option<Placed> {
    match a {
        Asset::Table {
            project,
            dataset,
            name,
        } => {
            if dataset.starts_with('_') {
                return None;
            }
            if project != ctx.project {
                return Some(Placed::Outside(ExternalRef {
                    system: Some("BigQuery".into()),
                    server: Some(project.clone()),
                    database: Some(dataset.clone()),
                    name: Some(name.clone()),
                    detail: Some(detail),
                    ..Default::default()
                }));
            }
            let (_, _, _, kind) = kinds_found
                .iter()
                .find(|(d, n, call, _)| d == dataset && n == name && !call)?;
            Some(Placed::Item(Dependent {
                kind: kind.clone(),
                schema: (Some(dataset.as_str()) != ctx.session_dataset).then(|| dataset.clone()),
                name: name.clone(),
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Confirmed,
                detail: Some(detail),
                mentions: Vec::new(),
            }))
        }
        Asset::Other { system, name } => Some(Placed::Outside(ExternalRef {
            system: Some(system_word(system)),
            name: Some(name.clone()),
            detail: Some(detail),
            ..Default::default()
        })),
    }
}

/// The names of this project's tables a set of assets names, to look up.
pub(crate) fn names_of(assets: &[Asset], project: &str) -> Vec<Name> {
    let mut out: Vec<Name> = Vec::new();
    for a in assets {
        if let Asset::Table {
            project: p,
            dataset,
            name,
        } = a
        {
            if p == project
                && !dataset.starts_with('_')
                && !out.iter().any(|n| n.dataset == *dataset && n.name == *name)
            {
                out.push(Name {
                    project: p.clone(),
                    dataset: dataset.clone(),
                    name: name.clone(),
                    call: false,
                    at: Vec::new(),
                });
            }
        }
    }
    out
}

/// The asset at the far end of each link, with when it last ran and which processes carried
/// it: one entry per asset.
pub(crate) fn neighbors(
    me: &str,
    side: Side,
    links: &[Link],
    labels: &HashMap<String, Vec<String>>,
) -> Vec<(Asset, String)> {
    let mut by: BTreeMap<String, (Option<String>, Vec<String>)> = BTreeMap::new();
    for l in links {
        let other = if side == Side::Down {
            &l.target
        } else {
            &l.source
        };
        if other == me {
            continue;
        }
        let e = by.entry(other.clone()).or_default();
        if l.end > e.0 {
            e.0 = l.end.clone();
        }
        for p in labels.get(&l.name).into_iter().flatten() {
            if !e.1.contains(p) {
                e.1.push(p.clone());
            }
        }
    }
    by.into_iter()
        .map(|(f, (end, ls))| (parse_fqn(&f), link_detail(end.as_deref(), &ls)))
        .collect()
}

// -- the jobs ----------------------------------------------------------------------------------

/// What the jobs query is asked.
pub(crate) enum JobsShape<'a> {
    /// Which tables the jobs that read this one wrote.
    Readers { dataset: &'a str, table: &'a str },
    /// Every pair whose destination is in the dataset.
    Dataset { dataset: &'a str },
}

fn lit(s: &str) -> String {
    format!(
        "'{}'",
        s.replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
    )
}

/// The jobs of the last [`JOBS_DAYS`] days that finished OK, as (read, written) pairs. One
/// row per pair, with how many times and the last day. No `query` column: only the narrow
/// ones are scanned.
pub(crate) fn jobs_sql(project: &str, location: &str, shape: &JobsShape) -> String {
    let view = format!(
        "{}.{}.INFORMATION_SCHEMA.JOBS_BY_PROJECT",
        quote_ident(Quote::Backtick, project),
        quote_ident(Quote::Backtick, &format!("region-{location}"))
    );
    let mine = |p: &str, d: &str| {
        format!(
            "{p}.project_id = {} AND {p}.dataset_id = {}",
            lit(project),
            lit(d)
        )
    };
    let filter = match shape {
        JobsShape::Readers { dataset, table } => {
            format!("{} AND r.table_id = {}", mine("r", dataset), lit(table))
        }
        JobsShape::Dataset { dataset } => mine("destination_table", dataset),
    };
    format!(
        "SELECT r.project_id AS src_project, r.dataset_id AS src_dataset, r.table_id AS src_table,
       destination_table.project_id AS dst_project, destination_table.dataset_id AS dst_dataset,
       destination_table.table_id AS dst_table, COUNT(*) AS runs,
       FORMAT_TIMESTAMP('%Y-%m-%d', MAX(creation_time)) AS last_run
  FROM {view}, UNNEST(referenced_tables) AS r
 WHERE creation_time >= TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL {JOBS_DAYS} DAY)
   AND job_type = 'QUERY' AND state = 'DONE' AND error_result IS NULL
   AND destination_table.table_id IS NOT NULL
   AND NOT STARTS_WITH(destination_table.dataset_id, '_')
   AND NOT STARTS_WITH(destination_table.table_id, 'anon')
   AND {filter}
 GROUP BY 1, 2, 3, 4, 5, 6
 ORDER BY runs DESC
 LIMIT {JOBS_ROWS}"
    )
}

/// One (read, written) pair of the jobs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobEdge {
    pub src: Asset,
    pub dst: Asset,
    pub runs: u64,
    pub last: String,
}

pub(crate) fn job_edges(rows: &[crate::ddl::Row]) -> Vec<JobEdge> {
    let asset = |r: &crate::ddl::Row, p: &str| {
        let g = |k: &str| r.get(&format!("{p}_{k}")).cloned();
        Some(Asset::Table {
            project: g("project")?,
            dataset: g("dataset")?,
            name: g("table")?,
        })
    };
    rows.iter()
        .filter_map(|r| {
            let (src, dst) = (asset(r, "src")?, asset(r, "dst")?);
            let own = |a: &Asset| matches!(a, Asset::Table { name, .. } if name.contains("INFORMATION_SCHEMA"));
            if src == dst || own(&src) {
                return None;
            }
            Some(JobEdge {
                src,
                dst,
                runs: r.get("runs").and_then(|v| v.parse().ok()).unwrap_or(1),
                last: r.get("last_run").cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn jobs_detail(e: &JobEdge) -> String {
    format!("JOBS_BY_PROJECT · {}× · {}", e.runs, e.last)
}

/// Why the jobs weren't read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum JobsWhy {
    TooBig,
    Failed,
}

// -- the definitions ---------------------------------------------------------------------------

/// The views, materialized views and routines of the dataset with their DDL in one query;
/// with `needle` the server keeps those whose DDL contains it.
pub(crate) fn text_sql(project: &str, dataset: &str, needle: Option<&str>) -> String {
    let is = format!(
        "{}.{}.INFORMATION_SCHEMA",
        quote_ident(Quote::Backtick, project),
        quote_ident(Quote::Backtick, dataset)
    );
    let filter = needle
        .map(|n| format!(" AND STRPOS(LOWER(ddl), LOWER({})) > 0", lit(n)))
        .unwrap_or_default();
    format!(
        "SELECT table_name AS name, table_type AS kind_raw, ddl AS body FROM {is}.TABLES
 WHERE table_type IN ('VIEW', 'MATERIALIZED VIEW'){filter}
 UNION ALL
SELECT routine_name, routine_type, ddl FROM {is}.ROUTINES WHERE TRUE{filter}
 LIMIT {TEXT_ROWS}"
    )
}

/// The foreign keys of the dataset (one row per constraint and table it points at); with
/// `parent` only those that point at that table.
pub(crate) fn foreign_keys_sql(project: &str, dataset: &str, parent: Option<&str>) -> String {
    let is = format!(
        "{}.{}.INFORMATION_SCHEMA",
        quote_ident(Quote::Backtick, project),
        quote_ident(Quote::Backtick, dataset)
    );
    let filter = parent
        .map(|p| format!(" AND u.table_name = {}", lit(p)))
        .unwrap_or_default();
    format!(
        "SELECT c.table_name AS child, c.constraint_name AS constraint_name,
       u.table_schema AS ref_schema, u.table_name AS ref_table
  FROM {is}.TABLE_CONSTRAINTS c
  JOIN {is}.CONSTRAINT_COLUMN_USAGE u ON u.constraint_name = c.constraint_name
 WHERE c.constraint_type = 'FOREIGN KEY'{filter}
 GROUP BY 1, 2, 3, 4"
    )
}

/// A definition read from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Def {
    pub name: String,
    pub kind: &'static str,
    pub body: String,
}

pub(crate) fn defs(rows: &[crate::ddl::Row]) -> Vec<Def> {
    rows.iter()
        .filter_map(|r| {
            let kind = match r.get("kind_raw")?.to_ascii_uppercase().as_str() {
                "VIEW" => kinds::VIEW,
                "MATERIALIZED VIEW" => kinds::MATERIALIZED_VIEW,
                "PROCEDURE" => kinds::PROCEDURE,
                _ => kinds::FUNCTION,
            };
            Some(Def {
                name: r.get("name")?.clone(),
                kind,
                body: r.get("body")?.clone(),
            })
        })
        .collect()
}

/// The definitions that name the target (Probable), with the lines. A name is matched as
/// BigQuery writes it (see `references::body_names`): the target's own dataset and name. A
/// column counts only in a definition that names its table.
pub(crate) fn code_dependents(
    defs: &[Def],
    target: &DependencyTarget,
    scan: &DependencyScan,
    ctx: &Ctx,
    dataset: &str,
) -> Vec<Dependent> {
    let t = &target.object;
    let call = matches!(t.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE);
    let mut out = Vec::new();
    for d in defs {
        if d.name == t.name && d.kind == t.kind {
            continue;
        }
        let found = body_names(&d.body, &scan.dialect, ctx.project, dataset);
        let hits: Vec<usize> = found
            .names
            .iter()
            .filter(|n| {
                n.project == ctx.project
                    && n.dataset == dataset
                    && n.name == t.name
                    && n.call == call
            })
            .flat_map(|n| n.at.iter().copied())
            .collect();
        if hits.is_empty() {
            continue;
        }
        let (confidence, mentions) = if target.column.is_some() {
            let probe = DependencyTarget {
                object: ObjectRef {
                    schema: None,
                    ..t.clone()
                },
                column: target.column.clone(),
            };
            match dependencies::find_mentions(&d.body, &scan.dialect, &probe) {
                Some(m) => m,
                None => continue,
            }
        } else {
            (Confidence::Probable, mentions_at(&d.body, &hits))
        };
        add(
            &mut out,
            Dependent {
                kind: d.kind.into(),
                schema: (Some(dataset) != ctx.session_dataset).then(|| dataset.to_string()),
                name: d.name.clone(),
                parent: None,
                relation: Relation::Code,
                confidence,
                detail: None,
                mentions,
            },
        );
    }
    out
}

/// The tables whose foreign keys point at the target's table (Confirmed).
pub(crate) fn foreign_key_children(
    rows: &[crate::ddl::Row],
    ctx: &Ctx,
    dataset: &str,
    parent: &str,
) -> Vec<Dependent> {
    let mut out = Vec::new();
    for r in rows {
        let (Some(child), Some(name), Some(rt)) =
            (r.get("child"), r.get("constraint_name"), r.get("ref_table"))
        else {
            continue;
        };
        if rt != parent || child == parent {
            continue;
        }
        add(
            &mut out,
            Dependent {
                kind: kinds::TABLE.into(),
                schema: (Some(dataset) != ctx.session_dataset).then(|| dataset.to_string()),
                name: child.clone(),
                parent: None,
                relation: Relation::ForeignKey,
                confidence: Confidence::Confirmed,
                detail: Some(name.clone()),
                mentions: Vec::new(),
            },
        );
    }
    out
}

// -- the session -------------------------------------------------------------------------------

impl BigQuerySession {
    /// The dataset's location in lower case (where its lineage and its jobs are).
    pub(crate) async fn dataset_location(&self, dataset: &str) -> Option<String> {
        let d = self.api.get(&["datasets", dataset], &[]).await.ok()?;
        text(&d, "/location").map(|l| l.to_lowercase())
    }

    /// The bytes a dry run of `sql` would scan.
    async fn dry_bytes(&self, sql: &str) -> Result<u64> {
        let j = self.dry_run(sql).await?;
        Ok([
            "/statistics/totalBytesProcessed",
            "/statistics/query/totalBytesProcessed",
        ]
        .iter()
        .find_map(|p| text(&j, p).and_then(|v| v.parse().ok()))
        .unwrap_or(0))
    }

    /// The jobs query, after a dry run: not run past [`JOBS_MAX_BYTES`].
    pub(crate) async fn jobs_edges(
        &self,
        location: &str,
        shape: &JobsShape<'_>,
    ) -> std::result::Result<Vec<JobEdge>, JobsWhy> {
        let at = self.located(Some(location));
        let sql = jobs_sql(&self.api.project, location, shape);
        match at.dry_bytes(&sql).await {
            Ok(b) if b > JOBS_MAX_BYTES => return Err(JobsWhy::TooBig),
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("bigquery jobs dry run: {e}");
                return Err(JobsWhy::Failed);
            }
        }
        match at.named_rows(&sql).await {
            Ok(rows) => Ok(job_edges(&rows)),
            Err(e) => {
                tracing::debug!("bigquery jobs: {e}");
                Err(JobsWhy::Failed)
            }
        }
    }

    /// What reads a table, view, materialized view or routine (`Session::dependents`).
    pub(crate) async fn dependents_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        self.dependents_with(API, target, scan).await
    }

    pub(crate) async fn dependents_with(
        &mut self,
        base: &str,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = target.object.clone();
        if !matches!(
            t.kind.as_str(),
            kinds::TABLE
                | kinds::VIEW
                | kinds::MATERIALIZED_VIEW
                | kinds::FUNCTION
                | kinds::PROCEDURE
        ) {
            return dependencies::scan(self, target, scan).await;
        }
        let ds = self.dataset(&t)?;
        let project = self.api.project.clone();
        let session_dataset = self.dataset.clone();
        let ctx = Ctx {
            project: &project,
            session_dataset: session_dataset.as_deref(),
        };
        let object =
            t.kind != kinds::FUNCTION && t.kind != kinds::PROCEDURE && target.column.is_none();
        let loc = if self.api.emulator {
            None
        } else {
            self.dataset_location(&ds).await
        };
        let at = self.located(loc.as_deref());
        let mut report = DependencyReport::default();
        let mut notes: Vec<&str> = Vec::new();
        let mut generic = false;

        // The definitions that name it: one query on the dataset.
        match at.named_rows(&text_sql(&project, &ds, Some(&t.name))).await {
            Ok(rows) => {
                report.scanned = rows.len() as u32;
                for d in code_dependents(&defs(&rows), target, scan, &ctx, &ds) {
                    add(&mut report.items, d);
                }
            }
            Err(e) => {
                tracing::debug!("bigquery definitions of {ds}: {e}");
                generic = true;
                notes.push(NO_CATALOG);
                if let Ok(r) = dependencies::scan(self, target, scan).await {
                    report.scanned = r.scanned;
                    report.unreadable = r.unreadable;
                    for d in r.items {
                        add(&mut report.items, d);
                    }
                }
            }
        }

        // The foreign keys that point at it.
        if !generic && scan.foreign_keys && t.kind == kinds::TABLE && target.column.is_none() {
            match at
                .named_rows(&foreign_keys_sql(&project, &ds, Some(&t.name)))
                .await
            {
                Ok(rows) => {
                    for d in foreign_key_children(&rows, &ctx, &ds, &t.name) {
                        add(&mut report.items, d);
                    }
                }
                Err(e) => tracing::debug!("bigquery foreign keys to {ds}.{}: {e}", t.name),
            }
        }

        // What ran: the Data Lineage API, then the jobs.
        if let (true, Some(loc)) = (object, loc.as_deref()) {
            let me = fqn(&project, &ds, &t.name);
            let mut api_failed = false;
            let mut try_jobs = false;
            match search_links(&self.api, base, loc, Side::Down, &me).await {
                Ok(links) if links.is_empty() => try_jobs = true,
                Ok(links) => {
                    let labels = link_processes(&self.api, base, loc, &links).await;
                    let near = neighbors(&me, Side::Down, &links, &labels);
                    let assets: Vec<Asset> = near.iter().map(|(a, _)| a.clone()).collect();
                    let looked = self.look_up(&names_of(&assets, &project), &project).await;
                    for (a, detail) in near {
                        match place(&ctx, &a, &looked, detail) {
                            Some(Placed::Item(d)) => add(&mut report.items, d),
                            Some(Placed::Outside(x)) => report.external_users.push(x),
                            None => {}
                        }
                    }
                }
                Err(f) => {
                    tracing::debug!("bigquery data lineage of {me}: {f:?}");
                    notes.push(Why::of(&f).note());
                    api_failed = true;
                    try_jobs = true;
                }
            }
            if try_jobs {
                let shape = JobsShape::Readers {
                    dataset: &ds,
                    table: &t.name,
                };
                match self.jobs_edges(loc, &shape).await {
                    Ok(edges) => {
                        if api_failed {
                            notes.push(FROM_JOBS);
                        }
                        let assets: Vec<Asset> = edges.iter().map(|e| e.dst.clone()).collect();
                        let looked = self.look_up(&names_of(&assets, &project), &project).await;
                        for e in edges {
                            match place(&ctx, &e.dst, &looked, jobs_detail(&e)) {
                                Some(Placed::Item(d)) => add(&mut report.items, d),
                                Some(Placed::Outside(x)) => report.external_users.push(x),
                                None => {}
                            }
                        }
                    }
                    Err(JobsWhy::TooBig) => notes.push(JOBS_BIG),
                    Err(JobsWhy::Failed) if api_failed => notes.push(NO_JOBS),
                    Err(JobsWhy::Failed) => {}
                }
            }
        }
        sort(&mut report.items);
        if !notes.is_empty() {
            report.note = Some(notes.join(" "));
        }
        Ok(report)
    }

    /// What feeds a table: Data Lineage's `target` links (Confirmed), no billed query. The
    /// panel of what an object uses adds them to what its definition says. Nothing when the
    /// API can't answer, and no note: `dependents` says why.
    pub(crate) async fn lineage_sources(
        &self,
        base: &str,
        target: &ObjectRef,
        ds: &str,
    ) -> (Vec<Dependent>, Vec<ExternalRef>) {
        let Some(loc) = self.dataset_location(ds).await else {
            return (Vec::new(), Vec::new());
        };
        let project = self.api.project.clone();
        let session_dataset = self.dataset.clone();
        let ctx = Ctx {
            project: &project,
            session_dataset: session_dataset.as_deref(),
        };
        let me = fqn(&project, ds, &target.name);
        let Ok(links) = search_links(&self.api, base, &loc, Side::Up, &me).await else {
            return (Vec::new(), Vec::new());
        };
        let labels = link_processes(&self.api, base, &loc, &links).await;
        let near = neighbors(&me, Side::Up, &links, &labels);
        let assets: Vec<Asset> = near.iter().map(|(a, _)| a.clone()).collect();
        let looked = self.look_up(&names_of(&assets, &project), &project).await;
        let (mut items, mut outside) = (Vec::new(), Vec::new());
        for (a, detail) in near {
            match place(&ctx, &a, &looked, detail) {
                Some(Placed::Item(d)) => add(&mut items, d),
                Some(Placed::Outside(x)) => outside.push(x),
                None => {}
            }
        }
        (items, outside)
    }

    /// The lineage of a dataset (`Session::lineage_graph`).
    pub(crate) async fn lineage_of(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        self.lineage_with(API, database, scope, scan).await
    }

    pub(crate) async fn lineage_with(
        &mut self,
        base: &str,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let ds = if database.is_empty() {
            self.dataset.clone().ok_or_else(|| {
                dbine_driver::Error::Query("no hay un dataset seleccionado".into())
            })?
        } else {
            database.to_string()
        };
        let project = self.api.project.clone();
        let loc = if self.api.emulator {
            None
        } else {
            self.dataset_location(&ds).await
        };
        let at = self.located(loc.as_deref());

        // The definitions, all in one query. Without them the contract's scan.
        let rows = match at.named_rows(&text_sql(&project, &ds, None)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("bigquery definitions of {ds}: {e}");
                let mut g = dependencies::graph_scan(self, &ds, scope, scan).await?;
                g.notes.push(NO_CATALOG.into());
                return Ok(g);
            }
        };
        let mut g = LineageGraph::default();
        let nodes = self.graph_nodes(&ds, scope, &mut g).await?;
        let defs = defs(&rows);
        g.scanned = defs.len() as u32;
        let node_kind = |name: &str, call: bool| {
            nodes
                .iter()
                .find(|n| {
                    n.name == name
                        && matches!(n.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE) == call
                })
                .cloned()
        };
        for d in &defs {
            let Some(to) = node_kind(
                &d.name,
                matches!(d.kind, kinds::FUNCTION | kinds::PROCEDURE),
            ) else {
                continue;
            };
            let found = body_names(&d.body, &scan.dialect, &project, &ds);
            for n in &found.names {
                if n.project != project {
                    g.external.push(LineageExternal {
                        source: ExternalRef {
                            system: Some("BigQuery".into()),
                            server: Some(n.project.clone()),
                            database: Some(n.dataset.clone()),
                            name: Some(n.name.clone()),
                            mentions: mentions_at(&d.body, &n.at),
                            ..Default::default()
                        },
                        to: to.clone(),
                    });
                    continue;
                }
                if n.dataset != ds {
                    // Another dataset of the project: it exists if the name is a table or a
                    // routine, and the diagram of this dataset draws it as a source outside.
                    if !n.name.is_empty()
                        && !n.dataset.starts_with('_')
                        && n.dataset != "INFORMATION_SCHEMA"
                    {
                        g.external.push(LineageExternal {
                            source: ExternalRef {
                                system: Some("BigQuery".into()),
                                server: Some(n.project.clone()),
                                database: Some(n.dataset.clone()),
                                name: Some(n.name.clone()),
                                mentions: mentions_at(&d.body, &n.at),
                                ..Default::default()
                            },
                            to: to.clone(),
                        });
                    }
                    continue;
                }
                let Some(from) = node_kind(&n.name, n.call) else {
                    continue;
                };
                if same_node(&from, &to) {
                    continue;
                }
                dependencies::merge_edge(
                    &mut g.edges,
                    LineageEdge {
                        from,
                        to: to.clone(),
                        relation: Relation::Code,
                        confidence: Confidence::Probable,
                        detail: None,
                        mentions: mentions_at(&d.body, &n.at),
                    },
                    same_node,
                );
            }
            g.external
                .extend(found.external.into_iter().map(|source| LineageExternal {
                    source,
                    to: to.clone(),
                }));
        }

        // The foreign keys.
        if scan.foreign_keys {
            match at.named_rows(&foreign_keys_sql(&project, &ds, None)).await {
                Ok(fk) => {
                    for r in &fk {
                        let (Some(child), Some(cn), Some(rt)) =
                            (r.get("child"), r.get("constraint_name"), r.get("ref_table"))
                        else {
                            continue;
                        };
                        if child == rt {
                            continue;
                        }
                        let (Some(from), Some(to)) =
                            (node_kind(rt, false), node_kind(child, false))
                        else {
                            continue;
                        };
                        dependencies::merge_edge(
                            &mut g.edges,
                            LineageEdge {
                                from,
                                to,
                                relation: Relation::ForeignKey,
                                confidence: Confidence::Confirmed,
                                detail: Some(cn.clone()),
                                mentions: Vec::new(),
                            },
                            same_node,
                        );
                    }
                }
                Err(e) => tracing::debug!("bigquery foreign keys of {ds}: {e}"),
            }
        }

        // What ran: the API for each table, else the jobs in one query.
        if let Some(loc) = loc.as_deref() {
            let tables: Vec<ObjectRef> = nodes
                .iter()
                .filter(|n| n.kind == kinds::TABLE)
                .cloned()
                .collect();
            let mut pairs: Vec<(Asset, ObjectRef, String)> = Vec::new();
            let mut why: Option<Why> = None;
            let mut links_seen = 0usize;
            let mut partial = false;
            if tables.len() > GRAPH_TABLES {
                g.notes.push(format!(
                    "se le preguntó a Data Lineage por las primeras {GRAPH_TABLES} tablas, de {}",
                    tables.len()
                ));
            }
            let mut todo = tables.into_iter().take(GRAPH_TABLES);
            // The first one alone: if the API is off or forbidden nothing else is asked.
            if let Some(first) = todo.next() {
                match search_links(
                    &self.api,
                    base,
                    loc,
                    Side::Up,
                    &fqn(&project, &ds, &first.name),
                )
                .await
                {
                    Ok(links) => {
                        links_seen += links.len();
                        pairs.extend(link_pairs(&project, &ds, &first, &links));
                    }
                    Err(f) => why = Some(Why::of(&f)),
                }
                if why.is_none() {
                    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(PARALLEL));
                    let mut set = tokio::task::JoinSet::new();
                    for t in todo {
                        let (api, gate, base, loc) = (
                            self.api.clone(),
                            gate.clone(),
                            base.to_string(),
                            loc.to_string(),
                        );
                        let (project, ds) = (project.clone(), ds.clone());
                        set.spawn(async move {
                            let _slot = gate.acquire().await.ok();
                            let r = search_links(
                                &api,
                                &base,
                                &loc,
                                Side::Up,
                                &fqn(&project, &ds, &t.name),
                            )
                            .await;
                            (t, r)
                        });
                    }
                    while let Some(done) = set.join_next().await {
                        match done {
                            Ok((t, Ok(links))) => {
                                links_seen += links.len();
                                pairs.extend(link_pairs(&project, &ds, &t, &links));
                            }
                            _ => partial = true,
                        }
                    }
                }
            }
            if partial {
                g.notes.push(API_PARTIAL.into());
            }
            let mut from_jobs = false;
            if why.is_some() || links_seen == 0 {
                match self
                    .jobs_edges(loc, &JobsShape::Dataset { dataset: &ds })
                    .await
                {
                    Ok(edges) => {
                        from_jobs = true;
                        for e in &edges {
                            if let Asset::Table {
                                name,
                                dataset,
                                project: p,
                            } = &e.dst
                            {
                                if *dataset == ds && *p == project {
                                    if let Some(to) = nodes
                                        .iter()
                                        .find(|n| n.name == *name && n.kind == kinds::TABLE)
                                    {
                                        pairs.push((e.src.clone(), to.clone(), jobs_detail(e)));
                                    }
                                }
                            }
                        }
                    }
                    Err(JobsWhy::TooBig) => g.notes.push(JOBS_BIG.into()),
                    Err(JobsWhy::Failed) if why.is_some() => g.notes.push(NO_JOBS.into()),
                    Err(JobsWhy::Failed) => {}
                }
            }
            if let Some(w) = why {
                g.notes.push(w.note().into());
                if from_jobs {
                    g.notes.push(FROM_JOBS.into());
                }
            }
            for (src, to, detail) in pairs {
                place_source(&mut g, &project, &ds, &nodes, src, to, detail);
            }
        }

        if scope.jobs {
            self.graph_server_jobs(&ds, scan, &nodes, &mut g).await;
        }
        g.objects = nodes;
        Ok(g)
    }

    /// The dataset's objects as nodes (tables, views, materialized views, routines), kept to
    /// the scope. REST calls, not billed.
    async fn graph_nodes(
        &self,
        ds: &str,
        scope: &LineageScope,
        g: &mut LineageGraph,
    ) -> Result<Vec<ObjectRef>> {
        let limit = if scope.limit == 0 {
            dependencies::GRAPH_OBJECTS
        } else {
            scope.limit
        } as usize;
        let mut all: Vec<ObjectRef> = Vec::new();
        for t in self
            .api
            .list_all(&["datasets", ds, "tables"], "tables")
            .await?
        {
            if let Some(name) = text(&t, "/tableReference/tableId") {
                all.push(ObjectRef {
                    kind: crate::references::table_kind(&t).into(),
                    schema: None,
                    name,
                });
            }
        }
        // Routines are optional: an emulator may not have them.
        if let Ok(routines) = self
            .api
            .list_all(&["datasets", ds, "routines"], "routines")
            .await
        {
            for r in routines {
                if let Some(name) = text(&r, "/routineReference/routineId") {
                    all.push(ObjectRef {
                        kind: crate::references::routine_kind(&r).into(),
                        schema: None,
                        name,
                    });
                }
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        let mut nodes = Vec::new();
        for o in all {
            if !scope.kinds.is_empty() && !scope.kinds.contains(&o.kind) {
                continue;
            }
            if nodes.len() >= limit {
                g.truncated = true;
                break;
            }
            nodes.push(o);
        }
        Ok(nodes)
    }

    /// The scheduled queries, each with what its query uses (as the contract's graph does).
    async fn graph_server_jobs(
        &mut self,
        ds: &str,
        scan: &DependencyScan,
        nodes: &[ObjectRef],
        g: &mut LineageGraph,
    ) {
        use dbine_driver::Session;
        let list = match self.jobs().await {
            Ok(l) => l,
            Err(dbine_driver::Error::Unsupported(_)) => return,
            Err(e) => {
                g.job_notes
                    .push(format!("no se pudieron leer los jobs del servidor: {e}"));
                return;
            }
        };
        g.job_notes.extend(list.notes);
        let total = list.jobs.len();
        for job in list.jobs.into_iter().take(JOBS_READ) {
            let Ok(r) = self.job_references(ds, &job.id, scan).await else {
                g.unreadable.push(job.name);
                continue;
            };
            g.scanned += 1;
            let uses: Vec<Dependent> = r
                .items
                .into_iter()
                .filter(|d| {
                    nodes.iter().any(|n| {
                        n.kind == d.kind
                            && n.name == d.name
                            && d.schema.as_deref().is_none_or(|s| s == ds)
                    })
                })
                .collect();
            if uses.is_empty() && r.external.is_empty() {
                continue;
            }
            g.jobs.push(LineageJob {
                id: job.id,
                name: job.name,
                kind: job.kind,
                group: job.group,
                uses,
                external: r.external,
            });
        }
        if total > JOBS_READ {
            g.job_notes.push(format!(
                "se leyeron los primeros {JOBS_READ} jobs del servidor, de {total}"
            ));
        }
    }
}

fn same_node(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && a.name == b.name
}

/// The (source, table, detail) of the links that feed `to`.
fn link_pairs(
    project: &str,
    ds: &str,
    to: &ObjectRef,
    links: &[Link],
) -> Vec<(Asset, ObjectRef, String)> {
    let me = fqn(project, ds, &to.name);
    neighbors(&me, Side::Up, links, &HashMap::new())
        .into_iter()
        .map(|(a, d)| (a, to.clone(), d))
        .collect()
}

/// A source feeding `to` as an edge when it is a node of the dataset, else a source outside.
fn place_source(
    g: &mut LineageGraph,
    project: &str,
    ds: &str,
    nodes: &[ObjectRef],
    src: Asset,
    to: ObjectRef,
    detail: String,
) {
    match &src {
        Asset::Table {
            project: p,
            dataset,
            name,
        } if p == project && dataset == ds => {
            let Some(from) = nodes.iter().find(|n| {
                n.name == *name && !matches!(n.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE)
            }) else {
                return;
            };
            if same_node(from, &to) {
                return;
            }
            dependencies::merge_edge(
                &mut g.edges,
                LineageEdge {
                    from: from.clone(),
                    to,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: Some(detail),
                    mentions: Vec::new(),
                },
                same_node,
            );
        }
        Asset::Table { dataset, .. } if dataset.starts_with('_') => {}
        // Another dataset or project, or not a table: named, not looked up.
        Asset::Table {
            project: p,
            dataset,
            name,
        } => g.external.push(LineageExternal {
            source: ExternalRef {
                system: Some("BigQuery".into()),
                server: Some(p.clone()),
                database: Some(dataset.clone()),
                name: Some(name.clone()),
                detail: Some(detail),
                ..Default::default()
            },
            to,
        }),
        Asset::Other { system, name } => g.external.push(LineageExternal {
            source: ExternalRef {
                system: Some(system_word(system)),
                name: Some(name.clone()),
                detail: Some(detail),
                ..Default::default()
            },
            to,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ConnectionConfig;
    use std::sync::{Arc, Mutex};

    // -- the pieces -----------------------------------------------------------------------

    #[test]
    fn names_are_written_as_data_lineage_does() {
        assert_eq!(fqn("p", "ds", "t"), "bigquery:p.ds.t");
        // A part with a dot, a colon or a space is in backticks, a backtick doubled.
        assert_eq!(
            fqn("google.com:proj", "ds", "a b"),
            "bigquery:`google.com:proj`.ds.`a b`"
        );
        assert_eq!(fqn("p", "ds", "a`b.c"), "bigquery:p.ds.`a``b.c`");
        for (p, d, t) in [
            ("p", "ds", "t"),
            ("google.com:proj", "ds", "a b"),
            ("p", "d", "a`b.c"),
        ] {
            assert_eq!(
                parse_fqn(&fqn(p, d, t)),
                Asset::Table {
                    project: p.into(),
                    dataset: d.into(),
                    name: t.into()
                }
            );
        }
        assert_eq!(
            parse_fqn("gcs:bucket/exports/day=1"),
            Asset::Other {
                system: "gcs".into(),
                name: "bucket/exports/day=1".into()
            }
        );
        assert!(matches!(parse_fqn("bigquery:p.ds"), Asset::Other { .. }));
        assert_eq!(system_word("gcs"), "Cloud Storage");
    }

    #[test]
    fn links_and_processes_are_read_from_the_answer() {
        let page = json!({ "links": [
            { "name": "projects/p/locations/us/links/l1",
              "source": { "fullyQualifiedName": "bigquery:p.ds.a" },
              "target": { "fullyQualifiedName": "bigquery:p.ds.b" },
              "endTime": "2026-11-20T10:00:00Z" },
            { "source": { "fullyQualifiedName": "bigquery:p.ds.a" } }
        ], "nextPageToken": "n" });
        let links = parse_links(&page);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "bigquery:p.ds.b");
        assert_eq!(links[0].end.as_deref(), Some("2026-11-20T10:00:00Z"));
        assert!(parse_links(&json!({})).is_empty());
        assert_eq!(
            process_label(
                &json!({ "displayName": "daily", "origin": { "sourceType": "COMPOSER" } })
            )
            .as_deref(),
            Some("composer daily")
        );
        assert_eq!(
            process_label(
                &json!({ "origin": { "sourceType": "BIGQUERY", "name": "projects/p/bigquery_job/j9" } })
            )
            .as_deref(),
            Some("bigquery j9")
        );
        assert_eq!(
            process_label(&json!({ "origin": { "sourceType": "CUSTOM" } })),
            None
        );
    }

    #[test]
    fn a_failed_call_says_why() {
        let f = |status, disabled| Failed {
            status,
            disabled,
            error: dbine_driver::Error::Query("x".into()),
        };
        assert_eq!(Why::of(&f(403, true)), Why::Off);
        assert_eq!(Why::of(&f(403, false)), Why::Denied);
        assert_eq!(Why::of(&f(401, false)), Why::Denied);
        assert_eq!(Why::of(&f(500, false)), Why::Other);
        assert_ne!(Why::Off.note(), Why::Denied.note());
    }

    #[test]
    fn the_jobs_query_is_narrow_and_cheap() {
        let sql = jobs_sql(
            "my-proj",
            "us",
            &JobsShape::Readers {
                dataset: "ds",
                table: "o'rders",
            },
        );
        assert!(
            sql.contains("`my-proj`.`region-us`.INFORMATION_SCHEMA.JOBS_BY_PROJECT"),
            "{sql}"
        );
        // The window prunes the partitions; the query text is never selected.
        assert!(
            sql.contains("creation_time >= TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 14 DAY)")
        );
        assert!(!sql.contains("query AS") && !sql.contains(" query,"));
        assert!(sql.contains("error_result IS NULL") && sql.contains("job_type = 'QUERY'"));
        // A name with a quote is escaped, not spliced.
        assert!(sql.contains(r"r.table_id = 'o\'rders'"), "{sql}");
        assert!(sql.contains("LIMIT 5000"));
        let sql = jobs_sql("p", "europe-west1", &JobsShape::Dataset { dataset: "ds" });
        assert!(sql.contains("`region-europe-west1`"));
        assert!(sql.contains("destination_table.dataset_id = 'ds'"));
        assert!(!sql.contains("r.table_id ="));
    }

    #[test]
    fn the_definitions_are_one_query_filtered_by_the_server() {
        let all = text_sql("p", "ds", None);
        assert!(
            all.contains("`p`.`ds`.INFORMATION_SCHEMA.TABLES")
                && all.contains("INFORMATION_SCHEMA.ROUTINES")
        );
        assert!(all.contains("UNION ALL") && !all.contains("STRPOS"));
        let one = text_sql("p", "ds", Some("orders"));
        assert_eq!(
            one.matches("STRPOS(LOWER(ddl), LOWER('orders')) > 0")
                .count(),
            2
        );
        let fk = foreign_keys_sql("p", "ds", Some("orders"));
        assert!(
            fk.contains("constraint_type = 'FOREIGN KEY'")
                && fk.contains("u.table_name = 'orders'")
        );
        assert!(!foreign_keys_sql("p", "ds", None).contains("u.table_name ="));
    }

    fn row(pairs: &[(&str, &str)]) -> crate::ddl::Row {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_jobs_rows_are_pairs_without_noise() {
        let pair = |s: &str, d: &str, runs: &str| {
            row(&[
                ("src_project", "p"),
                ("src_dataset", "ds"),
                ("src_table", s),
                ("dst_project", "p"),
                ("dst_dataset", "ds"),
                ("dst_table", d),
                ("runs", runs),
                ("last_run", "2026-11-20"),
            ])
        };
        // Itself, and a system view: left out.
        let rows = vec![
            pair("orders", "daily", "12"),
            pair("a", "a", "1"),
            pair("INFORMATION_SCHEMA.TABLES", "z", "1"),
        ];
        let e = job_edges(&rows);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].runs, 12);
        assert_eq!(jobs_detail(&e[0]), "JOBS_BY_PROJECT · 12× · 2026-11-20");
    }

    fn ctx() -> Ctx<'static> {
        Ctx {
            project: "p",
            session_dataset: Some("ds"),
        }
    }

    #[test]
    fn links_become_items_and_sources_outside() {
        let link = |n: &str, t: &str, end: &str| Link {
            name: n.into(),
            source: "bigquery:p.ds.orders".into(),
            target: t.into(),
            end: Some(end.into()),
        };
        let links = vec![
            link("l1", "bigquery:p.ds.daily", "2026-11-19T01:00:00Z"),
            link("l2", "bigquery:p.ds.daily", "2026-11-20T01:00:00Z"),
            link("l3", "bigquery:p.other.mart", "2026-11-20T01:00:00Z"),
            link("l4", "bigquery:q.x.copy", "2026-11-20T01:00:00Z"),
            link("l5", "gcs:bucket/out", "2026-11-20T01:00:00Z"),
            link("l6", "bigquery:p._anon.tmp", "2026-11-20T01:00:00Z"),
            link("l7", "bigquery:p.ds.gone", "2026-11-20T01:00:00Z"),
            link("l8", "bigquery:p.ds.orders", "2026-11-20T01:00:00Z"),
        ];
        let labels: HashMap<String, Vec<String>> = [
            ("l1".to_string(), vec!["bigquery j1".to_string()]),
            ("l2".into(), vec!["bigquery j2".into()]),
        ]
        .into();
        let near = neighbors("bigquery:p.ds.orders", Side::Down, &links, &labels);
        let assets: Vec<Asset> = near.iter().map(|(a, _)| a.clone()).collect();
        let names = names_of(&assets, "p");
        // The tables of this project, asked once each; the anonymous dataset not at all.
        let asked: Vec<_> = names
            .iter()
            .map(|n| (n.dataset.as_str(), n.name.as_str()))
            .collect();
        assert!(asked.contains(&("ds", "daily")) && asked.contains(&("other", "mart")));
        assert!(!asked.iter().any(|(d, _)| d.starts_with('_')));
        let looked = vec![
            (
                "ds".to_string(),
                "daily".to_string(),
                false,
                "table".to_string(),
            ),
            (
                "other".to_string(),
                "mart".to_string(),
                false,
                "view".to_string(),
            ),
        ];
        let (mut items, mut outside) = (Vec::new(), Vec::new());
        for (a, d) in near {
            match place(&ctx(), &a, &looked, d) {
                Some(Placed::Item(i)) => items.push(i),
                Some(Placed::Outside(x)) => outside.push(x),
                None => {}
            }
        }
        // `daily`: one item for the two links, the last day and both processes; `mart` in
        // another dataset has it as its schema; the dropped table and the anonymous one are not there.
        assert_eq!(items.len(), 2, "{items:?}");
        let daily = items.iter().find(|i| i.name == "daily").unwrap();
        assert_eq!(daily.schema, None);
        assert_eq!(daily.confidence, Confidence::Confirmed);
        assert_eq!(
            daily.detail.as_deref(),
            Some("Data Lineage · 2026-11-20 · bigquery j1, bigquery j2")
        );
        let mart = items.iter().find(|i| i.name == "mart").unwrap();
        assert_eq!(
            (mart.kind.as_str(), mart.schema.as_deref()),
            ("view", Some("other"))
        );
        let mut systems: Vec<_> = outside.iter().map(|x| x.system.clone().unwrap()).collect();
        systems.sort();
        assert_eq!(systems, ["BigQuery", "Cloud Storage"]);
    }

    fn scan() -> DependencyScan {
        let code: Vec<String> = ["view", "materialized_view", "function", "procedure"]
            .iter()
            .map(|k| k.to_string())
            .collect();
        DependencyScan {
            source_kinds: code.clone(),
            dialect: crate::script::dialect(),
            foreign_keys: true,
            written_kinds: code,
            has_schemas: false,
        }
    }

    fn target(kind: &str, name: &str) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            column: None,
        }
    }

    #[test]
    fn definitions_that_name_the_table_are_probable_with_their_lines() {
        let rows = vec![
            row(&[
                ("name", "v_orders"),
                ("kind_raw", "VIEW"),
                (
                    "body",
                    "CREATE VIEW `p.ds.v_orders` AS\nSELECT *\nFROM `p.ds.orders` o JOIN ds.items i ON TRUE",
                ),
            ]),
            // Another dataset's `orders`, and a column of that name: not this table.
            row(&[
                ("name", "v_other"),
                ("kind_raw", "VIEW"),
                ("body", "SELECT * FROM other.orders"),
            ]),
            row(&[
                ("name", "v_col"),
                ("kind_raw", "VIEW"),
                ("body", "SELECT orders FROM ds.items"),
            ]),
            row(&[
                ("name", "mv"),
                ("kind_raw", "MATERIALIZED VIEW"),
                ("body", "SELECT 1 FROM orders"),
            ]),
            row(&[
                ("name", "fn_x"),
                ("kind_raw", "FUNCTION"),
                ("body", "SELECT (SELECT COUNT(*) FROM ds.orders)"),
            ]),
        ];
        let got = code_dependents(
            &defs(&rows),
            &target("table", "orders"),
            &scan(),
            &ctx(),
            "ds",
        );
        let names: Vec<_> = got
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("view", "v_orders"),
                ("materialized_view", "mv"),
                ("function", "fn_x")
            ]
        );
        assert!(got
            .iter()
            .all(|d| d.confidence == Confidence::Probable && d.relation == Relation::Code));
        assert_eq!(got[0].mentions[0].line, 3);
        // A routine as the target: only calls count.
        let rows = vec![
            row(&[
                ("name", "v1"),
                ("kind_raw", "VIEW"),
                ("body", "SELECT ds.f(1) FROM ds.t"),
            ]),
            row(&[
                ("name", "v2"),
                ("kind_raw", "VIEW"),
                ("body", "SELECT 1 FROM ds.f"),
            ]),
        ];
        let got = code_dependents(
            &defs(&rows),
            &target("function", "f"),
            &scan(),
            &ctx(),
            "ds",
        );
        assert_eq!(
            got.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["v1"]
        );
    }

    #[test]
    fn foreign_keys_that_point_at_the_table_are_confirmed() {
        let fk = |c: &str, n: &str, r: &str| {
            row(&[
                ("child", c),
                ("constraint_name", n),
                ("ref_schema", "ds"),
                ("ref_table", r),
            ])
        };
        // Its own constraint, and another parent: not counted.
        let rows = vec![
            fk("items", "fk_o", "orders"),
            fk("orders", "pk", "orders"),
            fk("x", "fk_y", "y"),
        ];
        let got = foreign_key_children(&rows, &ctx(), "ds", "orders");
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].name.as_str(), got[0].relation, got[0].confidence),
            ("items", Relation::ForeignKey, Confidence::Confirmed)
        );
        assert_eq!(got[0].detail.as_deref(), Some("fk_o"));
    }

    // -- against a fake service -------------------------------------------------------------

    /// A tiny HTTP server: one request per connection, answered by `h`
    /// (method, path with query, body) → (status, body). As the jobs tests'.
    type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync>;

    async fn serve(h: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = vec![0u8; 1 << 16];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let (method, path) = (
                        first.next().unwrap_or("").to_string(),
                        first.next().unwrap_or("").to_string(),
                    );
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let (status, text) = h(&method, &path, &body);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn session(base: &str, emulator: bool) -> BigQuerySession {
        let http = reqwest::Client::new();
        let cfg = ConnectionConfig {
            options: [("endpoint_url".to_string(), base.to_string())].into(),
            ..Default::default()
        };
        BigQuerySession {
            api: Api {
                tokens: crate::gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
                http,
                base: base.into(),
                project: "p".into(),
                location: None,
                emulator,
            },
            dataset: Some("ds".into()),
            session_id: None,
            job: Arc::new(Mutex::new(None)),
            storage: Default::default(),
            profiler: None,
            no_catalog_pages: false,
        }
    }

    fn rows_json(cols: &[&str], data: &[&[&str]]) -> String {
        let fields: Vec<Json> = cols
            .iter()
            .map(|c| json!({ "name": c, "type": "STRING" }))
            .collect();
        let rows: Vec<Json> = data
            .iter()
            .map(|r| json!({ "f": r.iter().map(|v| json!({ "v": v })).collect::<Vec<_>>() }))
            .collect();
        json!({ "jobComplete": true, "jobReference": { "jobId": "j" },
                "schema": { "fields": fields }, "rows": rows })
        .to_string()
    }

    fn err(status: u16, message: &str, disabled: bool) -> (u16, String) {
        let details = if disabled {
            json!([{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED" }])
        } else {
            json!([])
        };
        (
            status,
            json!({ "error": { "code": status, "message": message, "details": details } })
                .to_string(),
        )
    }

    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    fn asked(seen: &Seen, needle: &str) -> Vec<(String, String, String)> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(_, p, b)| p.contains(needle) || b.contains(needle))
            .cloned()
            .collect()
    }

    const DEFS_BODY: &str = "CREATE VIEW `p.ds.v_orders` AS\nSELECT *\nFROM `p.ds.orders`";

    /// The service: dataset `ds` in the US; the lineage API with `api` (`None`: it says it is
    /// off); the jobs view with a dry run of `jobs_bytes` (`None`: no permission); the
    /// definitions and foreign keys when `defs_ok`.
    async fn service(
        seen: Seen,
        api: Option<Json>,
        jobs_bytes: Option<u64>,
        defs_ok: bool,
    ) -> String {
        serve(Arc::new(move |m, p, b| {
            seen.lock()
                .unwrap()
                .push((m.to_string(), p.to_string(), b.to_string()));
            let path = p.split_once('?').map_or(p, |x| x.0);
            let bq = "/bigquery/v2/projects/p";
            match (m, path) {
                ("GET", x) if x == format!("{bq}/datasets/ds") => {
                    (200, json!({ "location": "US" }).to_string())
                }
                ("POST", "/v1/projects/p/locations/us:searchLinks") => match &api {
                    None => err(
                        403,
                        "Data Lineage API has not been used in project 1 before or it is disabled.",
                        true,
                    ),
                    Some(a) => {
                        // `down` for the source side; `up` only for `daily`, the others have no source.
                        let answer = if b.contains("\"source\"") {
                            a["down"].clone()
                        } else if b.contains("p.ds.daily") {
                            a["up"].clone()
                        } else {
                            json!({})
                        };
                        (200, answer.to_string())
                    }
                },
                ("POST", "/v1/projects/p/locations/us:batchSearchLinkProcesses") => (
                    200,
                    json!({ "processLinks": [{
                        "process": "projects/p/locations/us/processes/pr1",
                        "links": [{ "link": "projects/p/locations/us/links/l1" }] }] })
                    .to_string(),
                ),
                ("GET", "/v1/projects/p/locations/us/processes/pr1") => (
                    200,
                    json!({ "name": "projects/p/locations/us/processes/pr1",
                            "displayName": "daily load", "origin": { "sourceType": "BIGQUERY" } })
                    .to_string(),
                ),
                ("GET", x) if x == format!("{bq}/datasets/ds/tables") => (
                    200,
                    json!({ "tables": [
                        { "tableReference": { "tableId": "orders" }, "type": "TABLE" },
                        { "tableReference": { "tableId": "daily" }, "type": "TABLE" },
                        { "tableReference": { "tableId": "v_orders" }, "type": "VIEW" }] })
                    .to_string(),
                ),
                ("GET", x) if x == format!("{bq}/datasets/ds/routines") => (200, "{}".into()),
                ("GET", x) if x == format!("{bq}/datasets/ds/tables/v_orders") => (
                    200,
                    json!({ "type": "VIEW", "view": { "query": "SELECT * FROM ds.orders" } })
                        .to_string(),
                ),
                ("GET", x) if x.starts_with(&format!("{bq}/datasets/ds/tables/")) => {
                    (200, json!({ "type": "TABLE" }).to_string())
                }
                ("POST", x) if x == format!("{bq}/jobs") => match jobs_bytes {
                    Some(n) => (
                        200,
                        json!({ "statistics": { "totalBytesProcessed": n.to_string() } })
                            .to_string(),
                    ),
                    None => err(403, "Access Denied: bigquery.jobs.listAll", false),
                },
                ("POST", x) if x == format!("{bq}/queries") => {
                    let sql = serde_json::from_str::<Json>(b).unwrap()["query"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    if sql.contains("JOBS_BY_PROJECT") {
                        return (
                            200,
                            rows_json(
                                &[
                                    "src_project",
                                    "src_dataset",
                                    "src_table",
                                    "dst_project",
                                    "dst_dataset",
                                    "dst_table",
                                    "runs",
                                    "last_run",
                                ],
                                &[&["p", "ds", "orders", "p", "ds", "daily", "7", "2026-11-18"]],
                            ),
                        );
                    }
                    if !defs_ok {
                        return err(
                            400,
                            "Not found: Dataset p:ds was not found in location US",
                            false,
                        );
                    }
                    if sql.contains("INFORMATION_SCHEMA.TABLES") {
                        (
                            200,
                            rows_json(
                                &["name", "kind_raw", "body"],
                                &[&["v_orders", "VIEW", DEFS_BODY]],
                            ),
                        )
                    } else if sql.contains("TABLE_CONSTRAINTS") {
                        (
                            200,
                            rows_json(
                                &["child", "constraint_name", "ref_schema", "ref_table"],
                                &[&["items", "fk_o", "ds", "orders"]],
                            ),
                        )
                    } else {
                        err(400, "unexpected query", false)
                    }
                }
                _ => err(404, "Not found", false),
            }
        }))
        .await
    }

    fn lineage_answer() -> Json {
        let l = |n: &str, s: &str, t: &str| {
            json!({ "name": format!("projects/p/locations/us/links/{n}"),
                    "source": { "fullyQualifiedName": s }, "target": { "fullyQualifiedName": t },
                    "endTime": "2026-11-20T10:00:00Z" })
        };
        json!({
            "down": { "links": [l("l1", "bigquery:p.ds.orders", "bigquery:p.ds.daily"),
                                 l("l2", "bigquery:p.ds.orders", "bigquery:q.x.copy")] },
            "up": { "links": [l("l1", "bigquery:p.ds.orders", "bigquery:p.ds.daily"),
                               l("l3", "gcs:bucket/in", "bigquery:p.ds.daily")] },
        })
    }

    #[tokio::test]
    async fn what_reads_a_table_comes_from_data_lineage_and_one_query_of_definitions() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), true).await;
        let mut s = session(&base, false);
        let r = s
            .dependents_with(&base, &target("table", "orders"), &scan())
            .await
            .unwrap();
        let by = |n: &str| {
            r.items
                .iter()
                .find(|i| i.name == n)
                .unwrap_or_else(|| panic!("{n}: {:?}", r.items))
        };
        // Confirmed by the API, with the process; the view's definition (Probable); the foreign key.
        let daily = by("daily");
        assert_eq!(daily.confidence, Confidence::Confirmed);
        assert_eq!(
            daily.detail.as_deref(),
            Some("Data Lineage · 2026-11-20 · bigquery daily load")
        );
        assert_eq!(
            (by("v_orders").confidence, by("v_orders").kind.as_str()),
            (Confidence::Probable, "view")
        );
        assert_eq!(by("v_orders").mentions[0].line, 3);
        assert_eq!(
            (by("items").relation, by("items").confidence),
            (Relation::ForeignKey, Confidence::Confirmed)
        );
        // A table of another project reading it is a reader outside.
        assert_eq!(r.external_users.len(), 1);
        assert_eq!(r.external_users[0].server.as_deref(), Some("q"));
        assert!(r.note.is_none(), "{:?}", r.note);
        // The question was this one table: the API's `source` side once, the definitions in
        // one query, no job history, and no object's definition read one by one.
        let links = asked(&seen, ":searchLinks");
        assert_eq!(links.len(), 1);
        assert!(
            links[0]
                .2
                .contains(r#""source":{"fullyQualifiedName":"bigquery:p.ds.orders"}"#),
            "{}",
            links[0].2
        );
        assert!(asked(&seen, "JOBS_BY_PROJECT").is_empty());
        assert_eq!(asked(&seen, "INFORMATION_SCHEMA.TABLES").len(), 1);
        assert_eq!(asked(&seen, "/queries").len(), 2);
    }

    #[tokio::test]
    async fn without_the_api_the_jobs_confirm_with_a_note() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), None, Some(5 << 20), true).await;
        let mut s = session(&base, false);
        let r = s
            .dependents_with(&base, &target("table", "orders"), &scan())
            .await
            .unwrap();
        let daily = r
            .items
            .iter()
            .find(|i| i.name == "daily")
            .expect("daily from the jobs");
        assert_eq!(daily.confidence, Confidence::Confirmed);
        assert_eq!(
            daily.detail.as_deref(),
            Some("JOBS_BY_PROJECT · 7× · 2026-11-18")
        );
        assert!(r.items.iter().any(|i| i.name == "v_orders"));
        let note = r.note.unwrap();
        assert!(note.contains(API_OFF) && note.contains(FROM_JOBS), "{note}");
        // A dry run first, in the dataset's location; then the query in the region's view.
        let dry = asked(&seen, "dryRun");
        assert_eq!(dry.len(), 1);
        assert!(dry[0].2.contains(r#""location":"us""#), "{}", dry[0].2);
        let q = asked(&seen, "JOBS_BY_PROJECT");
        assert!(q
            .iter()
            .any(|(_, p, b)| p.ends_with("/queries") && b.contains("region-us")));
    }

    #[tokio::test]
    async fn a_jobs_history_too_big_to_read_is_not_read() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), None, Some(JOBS_MAX_BYTES + 1), true).await;
        let mut s = session(&base, false);
        let r = s
            .dependents_with(&base, &target("table", "orders"), &scan())
            .await
            .unwrap();
        assert!(!r.items.iter().any(|i| i.name == "daily"));
        assert!(
            r.items.iter().any(|i| i.name == "v_orders"),
            "the definitions still answer"
        );
        assert!(r.note.unwrap().contains(JOBS_BIG));
        // The dry run was asked; the query itself was not.
        assert_eq!(asked(&seen, "dryRun").len(), 1);
        assert!(asked(&seen, "JOBS_BY_PROJECT")
            .iter()
            .all(|(_, _, b)| b.contains("dryRun")));
    }

    #[tokio::test]
    async fn no_permission_for_the_api_or_the_jobs_still_answers() {
        let base = service(Default::default(), None, None, true).await;
        let mut s = session(&base, false);
        let r = s
            .dependents_with(&base, &target("table", "orders"), &scan())
            .await
            .unwrap();
        assert!(r.items.iter().any(|i| i.name == "v_orders"));
        let note = r.note.unwrap();
        assert!(note.contains(API_OFF) && note.contains(NO_JOBS), "{note}");
    }

    #[tokio::test]
    async fn an_empty_api_answer_is_completed_with_the_jobs_without_a_note() {
        let empty = json!({ "down": {}, "up": {} });
        let base = service(Default::default(), Some(empty), Some(1), true).await;
        let mut s = session(&base, false);
        let r = s
            .dependents_with(&base, &target("table", "orders"), &scan())
            .await
            .unwrap();
        assert!(r.items.iter().any(|i| i.name == "daily"));
        assert!(
            r.note.is_none(),
            "lineage only covers jobs run after it was enabled: not an error"
        );
    }

    #[tokio::test]
    async fn when_the_catalog_cannot_be_read_the_contracts_scan_runs_with_a_note() {
        let base = service(Default::default(), Some(lineage_answer()), Some(0), false).await;
        let mut s = session(&base, false);
        let mut sc = scan();
        sc.foreign_keys = false;
        let r = s
            .dependents_with(&base, &target("table", "orders"), &sc)
            .await
            .unwrap();
        assert!(r.note.unwrap().contains(NO_CATALOG));
        // The scan read the view through the REST API, and Data Lineage still added `daily`.
        assert!(r
            .items
            .iter()
            .any(|i| i.name == "v_orders" && i.confidence == Confidence::Probable));
        assert!(r
            .items
            .iter()
            .any(|i| i.name == "daily" && i.confidence == Confidence::Confirmed));
    }

    #[tokio::test]
    async fn the_emulator_has_no_lineage_service_and_asks_nothing_of_it() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), false).await;
        let mut s = session(&base, true);
        let mut sc = scan();
        sc.foreign_keys = false;
        let r = s
            .dependents_with(&base, &target("table", "orders"), &sc)
            .await
            .unwrap();
        assert!(r.items.iter().any(|i| i.name == "v_orders"));
        assert!(asked(&seen, "/v1/").is_empty() && asked(&seen, "JOBS_BY_PROJECT").is_empty());
    }

    #[tokio::test]
    async fn what_feeds_a_table_comes_from_data_lineage_for_free() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), true).await;
        let s = session(&base, false);
        let t = ObjectRef {
            kind: "table".into(),
            schema: None,
            name: "daily".into(),
        };
        let (items, outside) = s.lineage_sources(&base, &t, "ds").await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0].name.as_str(), items[0].confidence),
            ("orders", Confidence::Confirmed)
        );
        assert_eq!(outside.len(), 1);
        assert_eq!(outside[0].system.as_deref(), Some("Cloud Storage"));
        let links = asked(&seen, ":searchLinks");
        assert!(
            links[0]
                .2
                .contains(r#""target":{"fullyQualifiedName":"bigquery:p.ds.daily"}"#),
            "{}",
            links[0].2
        );
        // No query job at all.
        assert!(asked(&seen, "/queries").is_empty() && asked(&seen, "dryRun").is_empty());
        // Off: nothing, and no error.
        let base = service(Default::default(), None, Some(0), true).await;
        let (items, outside) = session(&base, false).lineage_sources(&base, &t, "ds").await;
        assert!(items.is_empty() && outside.is_empty());
    }

    #[tokio::test]
    async fn the_graph_of_a_dataset_is_one_definitions_query_and_one_call_per_table() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), true).await;
        let mut s = session(&base, false);
        let g = s
            .lineage_with(&base, "ds", &LineageScope::default(), &scan())
            .await
            .unwrap();
        let names: Vec<_> = g.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["daily", "orders", "v_orders"]);
        let edge = |from: &str, to: &str| {
            g.edges
                .iter()
                .find(|e| e.from.name == from && e.to.name == to)
        };
        // The view's code (Probable) and the API's line (Confirmed).
        let v = edge("orders", "v_orders").expect("view edge");
        assert_eq!(
            (v.relation, v.confidence),
            (Relation::Code, Confidence::Probable)
        );
        assert_eq!(v.mentions[0].line, 3);
        let d = edge("orders", "daily").expect("api edge");
        assert_eq!(
            (d.relation, d.confidence),
            (Relation::Code, Confidence::Confirmed)
        );
        assert!(d.detail.as_deref().unwrap().starts_with("Data Lineage"));
        // `items` has a foreign key but is not a node of the dataset: no edge.
        assert!(g.edges.iter().all(|e| e.relation != Relation::ForeignKey));
        // The source outside the dataset.
        assert_eq!(g.external.len(), 1);
        assert_eq!(
            g.external[0].source.system.as_deref(),
            Some("Cloud Storage")
        );
        assert_eq!(g.external[0].to.name, "daily");
        assert!(g.notes.is_empty(), "{:?}", g.notes);
        // One query for every definition, one call per table (2 tables), no job history,
        // nothing read object by object.
        assert_eq!(asked(&seen, "INFORMATION_SCHEMA.TABLES").len(), 1);
        assert_eq!(asked(&seen, ":searchLinks").len(), 2);
        assert!(asked(&seen, "JOBS_BY_PROJECT").is_empty());
        assert!(asked(&seen, "@name").is_empty());
    }

    #[tokio::test]
    async fn the_graph_without_the_api_takes_the_jobs_in_one_query_and_says_so() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), None, Some(1 << 20), true).await;
        let mut s = session(&base, false);
        let g = s
            .lineage_with(&base, "ds", &LineageScope::default(), &scan())
            .await
            .unwrap();
        let d = g
            .edges
            .iter()
            .find(|e| e.from.name == "orders" && e.to.name == "daily")
            .expect("from the jobs");
        assert_eq!(d.confidence, Confidence::Confirmed);
        assert_eq!(
            d.detail.as_deref(),
            Some("JOBS_BY_PROJECT · 7× · 2026-11-18")
        );
        assert!(
            g.notes.iter().any(|n| n == API_OFF) && g.notes.iter().any(|n| n == FROM_JOBS),
            "{:?}",
            g.notes
        );
        // The API was asked once (the first table), not for every table.
        assert_eq!(asked(&seen, ":searchLinks").len(), 1);
        let jobs = asked(&seen, "JOBS_BY_PROJECT");
        assert!(jobs.iter().any(|(_, p, b)| p.ends_with("/queries")
            && b.contains("destination_table.dataset_id = 'ds'")));
    }

    #[tokio::test]
    async fn the_graph_falls_back_to_the_contracts_scan_when_the_catalog_cannot_be_read() {
        let base = service(Default::default(), Some(lineage_answer()), Some(0), false).await;
        let mut s = session(&base, false);
        let mut sc = scan();
        sc.foreign_keys = false;
        let g = s
            .lineage_with(&base, "ds", &LineageScope::default(), &sc)
            .await
            .unwrap();
        assert!(g.notes.iter().any(|n| n == NO_CATALOG));
        assert!(g
            .edges
            .iter()
            .any(|e| e.from.name == "orders" && e.to.name == "v_orders"));
    }

    #[tokio::test]
    async fn the_references_of_a_table_add_what_data_lineage_saw_feed_it() {
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), true).await;
        let mut s = session(&base, false);
        let r = s
            .references_with(&base, &target("table", "daily"), &scan())
            .await
            .unwrap();
        let orders = r.items.iter().find(|i| i.name == "orders").expect("orders");
        assert_eq!(orders.confidence, Confidence::Confirmed);
        assert!(orders
            .detail
            .as_deref()
            .unwrap()
            .starts_with("Data Lineage"));
        assert!(r
            .external
            .iter()
            .any(|x| x.system.as_deref() == Some("Cloud Storage")));
        // Free: the REST API and the lineage service, no query job.
        assert!(asked(&seen, "/queries").is_empty() && asked(&seen, "dryRun").is_empty());
        // The emulator has none of it.
        let seen: Seen = Default::default();
        let base = service(seen.clone(), Some(lineage_answer()), Some(0), true).await;
        let mut s = session(&base, true);
        s.references_with(&base, &target("table", "daily"), &scan())
            .await
            .unwrap();
        assert!(asked(&seen, "/v1/").is_empty());
    }
}

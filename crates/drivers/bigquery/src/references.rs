//! What an object uses (`Session::references`), from the REST API only.
//!
//! Every `INFORMATION_SCHEMA` read is a billed query job
//! (`Capabilities::billed_metadata`): the default (the definition from
//! `INFORMATION_SCHEMA`, then one `INFORMATION_SCHEMA` lookup per name)
//! would bill several jobs per object. The REST API isn't billed:
//!
//! - A view's and a materialized view's query: `tables.get`
//!   (`view.query`, `materializedView.query`). A SQL function's or
//!   procedure's body: `routines.get` (`definitionBody`).
//! - The names the body uses (the contract's `used_names`):
//!   `project.dataset.table` (backticked whole or in parts),
//!   `dataset.table` (the connection's project) or a bare name (the
//!   object's own dataset). Each one in this project is looked up with
//!   `tables.get` (a call, `dataset.f(`, with `routines.get`), or with one
//!   `tables.list` when a dataset has many of them; what doesn't exist (a
//!   CTE, an alias) drops out. Probable, with the lines that name it. A
//!   name in another project is `external`: it may not be readable from
//!   this connection. `INFORMATION_SCHEMA` views are left out.
//! - `EXTERNAL_QUERY('connection', 'SELECT …')`: a federated query,
//!   `external` with its connection and the tables its query names.
//! - A table: the tables its foreign keys point at
//!   (`tableConstraints.foreignKeys`, Confirmed), the table a snapshot or
//!   clone was made from (Confirmed), and an external table's files
//!   (`externalDataConfiguration.sourceUris`) as `external`.
//! - A table's, view's or materialized view's sources that Data Lineage saw
//!   (`searchLinks`, `target` side, Confirmed; see `lineage`): the API isn't a query job,
//!   so it's free, and it says nothing when the API is off or not allowed.
//! - A JavaScript function's libraries (`importedLibraries`) and a remote
//!   function's endpoint: `external`.
//!
//! BigQuery keeps no dependency catalog: what a view reads is Probable. An
//! object in another dataset of the project is an item with the dataset as
//! its schema, which the driver reads; one in the connection's dataset has
//! none, as the explorer lists it.

use crate::BigQuerySession;
use dbine_driver::dependencies::{
    foreign_key_parents, used_names, Confidence, DependencyReport, DependencyScan,
    DependencyTarget, Dependent, ExternalRef, Mention, Relation, LINE_CHARS, MAX_MENTIONS,
};
use dbine_driver::sql::{code_tokens, quote_ident, Quote, ScriptDialect, TokenKind};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value as Json;

/// More names than this in one dataset: one `tables.list` instead of a
/// `tables.get` each.
const GETS_PER_DATASET: usize = 8;

/// A name the body uses, in this project or another.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Name {
    pub project: String,
    pub dataset: String,
    pub name: String,
    /// `dataset.f(`: a routine.
    pub call: bool,
    /// Byte offsets in the body.
    pub at: Vec<usize>,
}

/// What a body names: objects to look up, and federated queries.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Found {
    pub names: Vec<Name>,
    pub external: Vec<ExternalRef>,
}

/// The names `body` uses, qualified with `project` and `dataset` where it
/// leaves them out.
pub(crate) fn body_names(
    body: &str,
    dialect: &ScriptDialect,
    project: &str,
    dataset: &str,
) -> Found {
    let mut f = Found::default();
    for u in used_names(body, dialect) {
        // `a.b.c`, `` `a.b.c` `` and `` `a.b`.c `` are the same name.
        let parts: Vec<&str> = u
            .quals
            .iter()
            .rev()
            .chain(std::iter::once(&u.name))
            .flat_map(|p| p.split('.'))
            .collect();
        if parts.iter().any(|p| {
            p.eq_ignore_ascii_case("INFORMATION_SCHEMA")
                || p.to_ascii_lowercase().starts_with("region-")
        }) {
            continue;
        }
        let (p, d, n) = match parts.as_slice() {
            // A bare call is a built-in or a temporary function.
            [_] if u.call => continue,
            [n] => (project, dataset, *n),
            [d, n] => (project, *d, *n),
            [p, d, n] => (*p, *d, *n),
            _ => continue,
        };
        let found = f
            .names
            .iter_mut()
            .find(|x| x.project == p && x.dataset == d && x.name == n && x.call == u.call);
        match found {
            Some(x) => x.at.push(u.at),
            None => f.names.push(Name {
                project: p.to_string(),
                dataset: d.to_string(),
                name: n.to_string(),
                call: u.call,
                at: vec![u.at],
            }),
        }
    }
    f.external.extend(federated(body, dialect));
    f
}

/// `EXTERNAL_QUERY('connection', 'SELECT …')`: the connection, and each
/// table its query names (or the connection alone).
fn federated(body: &str, dialect: &ScriptDialect) -> Vec<ExternalRef> {
    let toks = code_tokens(body, dialect);
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let call = t.kind == TokenKind::Name
            && t.text.eq_ignore_ascii_case("EXTERNAL_QUERY")
            && toks
                .get(i + 1)
                .is_some_and(|p| p.kind == TokenKind::Punct && p.text == "(");
        if !call {
            continue;
        }
        let strings: Vec<String> = toks[i + 2..]
            .iter()
            .take_while(|x| !(x.kind == TokenKind::Punct && x.text == ")"))
            .filter(|x| x.kind == TokenKind::String)
            .map(|x| unquote(x.text))
            .collect();
        let Some(connection) = strings.first().cloned() else {
            continue;
        };
        let mention = mentions_at(body, &[t.start]);
        let one =
            |database: Option<String>, schema: Option<String>, name: Option<String>| ExternalRef {
                system: Some("EXTERNAL_QUERY".into()),
                server: Some(connection.clone()),
                database,
                schema,
                name,
                detail: None,
                mentions: mention.clone(),
            };
        let generic = ScriptDialect::generic();
        let inner = strings.get(1).map(String::as_str).unwrap_or("");
        let mut any = false;
        for u in used_names(inner, &generic).into_iter().filter(|u| !u.call) {
            let mut quals: Vec<String> = u.quals.iter().rev().cloned().collect();
            let x = match quals.len() {
                0 => one(None, None, Some(u.name)),
                1 => one(quals.pop(), None, Some(u.name)),
                _ => {
                    let schema = quals.pop();
                    one(quals.pop(), schema, Some(u.name))
                }
            };
            if !out.contains(&x) {
                out.push(x);
            }
            any = true;
        }
        if !any {
            out.push(one(None, None, None));
        }
    }
    out
}

/// A string literal's text: its quotes off (`'…'`, `"…"`, triple quotes,
/// an `r` prefix) and backslash escapes read.
pub(crate) fn unquote(s: &str) -> String {
    let (raw, s) = match s.strip_prefix(['r', 'R']) {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let inner = ["\"\"\"", "'''", "\"", "'"]
        .iter()
        .find_map(|q| s.strip_prefix(q).and_then(|x| x.strip_suffix(q)))
        .unwrap_or(s);
    if raw {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(n) => out.push(n),
            None => {}
        }
    }
    out
}

/// The parts of a `tables.get` answer this reads.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TableMeta {
    /// `view.query` or `materializedView.query`, with the statement it's
    /// shown in (`CREATE VIEW … AS`).
    pub body: Option<String>,
    /// The table a snapshot or clone was made from: project, dataset,
    /// table, and `SNAPSHOT` or `CLONE`.
    pub base: Option<(String, String, String, &'static str)>,
    /// An external table's files: the system and each URI.
    pub files: Vec<(String, String)>,
    pub connection: Option<String>,
}

/// The kind of a `tables.get` / `tables.list` item (`type`).
pub(crate) fn table_kind(t: &Json) -> &'static str {
    match t.get("type").and_then(Json::as_str) {
        Some("VIEW") => kinds::VIEW,
        Some("MATERIALIZED_VIEW") => kinds::MATERIALIZED_VIEW,
        _ => kinds::TABLE,
    }
}

/// What a `tables.get` answer says the table reads.
pub(crate) fn table_meta(t: &Json, dataset: &str, name: &str) -> TableMeta {
    let s = |v: Option<&Json>| v.and_then(Json::as_str).map(str::to_string);
    let shown = format!(
        "{}.{}",
        quote_ident(Quote::Backtick, dataset),
        quote_ident(Quote::Backtick, name)
    );
    let body = if let Some(q) = s(t.pointer("/view/query")) {
        Some(format!("CREATE VIEW {shown} AS\n{q}"))
    } else {
        s(t.pointer("/materializedView/query"))
            .map(|q| format!("CREATE MATERIALIZED VIEW {shown} AS\n{q}"))
    };
    let base = [
        ("/snapshotDefinition/baseTableReference", "SNAPSHOT"),
        ("/cloneDefinition/baseTableReference", "CLONE"),
    ]
    .into_iter()
    .find_map(|(path, how)| {
        let r = t.pointer(path)?;
        Some((
            s(r.get("projectId")).unwrap_or_default(),
            s(r.get("datasetId"))?,
            s(r.get("tableId"))?,
            how,
        ))
    });
    let format = s(t.pointer("/externalDataConfiguration/sourceFormat")).unwrap_or_default();
    let files = t
        .pointer("/externalDataConfiguration/sourceUris")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(|u| (storage_system(u, &format).to_string(), clean_location(u)))
        .collect();
    TableMeta {
        body,
        base,
        files,
        connection: s(t.pointer("/externalDataConfiguration/connectionId")),
    }
}

/// Which service a URI is in.
fn storage_system(uri: &str, format: &str) -> &'static str {
    match format {
        "GOOGLE_SHEETS" => return "Google Sheets",
        "BIGTABLE" => return "Bigtable",
        _ => {}
    }
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("gs://") {
        "Cloud Storage"
    } else if lower.starts_with("s3://") {
        "Amazon S3"
    } else if lower.starts_with("azure://") {
        "Azure Blob Storage"
    } else if lower.contains("docs.google.com") {
        "Google Sheets"
    } else if lower.contains("bigtable") {
        "Bigtable"
    } else {
        "URL"
    }
}

/// A URI as shown: without user info, query string or fragment.
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

/// A routine's body (SQL only) and the sources outside it reads: a
/// JavaScript function's libraries, a remote function's endpoint.
pub(crate) fn routine_meta(r: &Json) -> (Option<String>, Vec<ExternalRef>) {
    let sql = r
        .get("language")
        .and_then(Json::as_str)
        .is_none_or(|l| l.eq_ignore_ascii_case("SQL"));
    let body = r
        .get("definitionBody")
        .and_then(Json::as_str)
        .filter(|_| sql)
        .map(str::to_string);
    let mut external: Vec<ExternalRef> = r
        .get("importedLibraries")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(|u| ExternalRef {
            system: Some(storage_system(u, "").to_string()),
            name: Some(clean_location(u)),
            ..Default::default()
        })
        .collect();
    if let Some(endpoint) = r
        .pointer("/remoteFunctionOptions/endpoint")
        .and_then(Json::as_str)
    {
        external.push(ExternalRef {
            system: Some("HTTP".into()),
            name: Some(clean_location(endpoint)),
            detail: r
                .pointer("/remoteFunctionOptions/connection")
                .and_then(Json::as_str)
                .map(str::to_string),
            ..Default::default()
        });
    }
    (body, external)
}

/// The kind of a `routines.get` / `routines.list` item.
pub(crate) fn routine_kind(r: &Json) -> &'static str {
    if r.get("routineType").and_then(Json::as_str) == Some("PROCEDURE") {
        kinds::PROCEDURE
    } else {
        kinds::FUNCTION
    }
}

impl BigQuerySession {
    pub(crate) async fn references_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        self.references_with(crate::lineage::API, target, scan)
            .await
    }

    /// As [`Self::references_of`], with the Data Lineage service at `lineage`.
    pub(crate) async fn references_with(
        &mut self,
        lineage: &str,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let mut report = DependencyReport::default();
        let ds = self.dataset(t)?;
        let project = self.api.project.clone();
        let routine = matches!(t.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE);
        let what = if routine { "routines" } else { "tables" };
        let meta = match self.api.get(&["datasets", &ds, what, &t.name], &[]).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("bigquery references: {ds}.{} not read: {e}", t.name);
                report.unreadable.push(format!("{ds}.{}", t.name));
                return Ok(report);
            }
        };
        let body = if routine {
            let (body, external) = routine_meta(&meta);
            report.external.extend(external);
            body
        } else {
            let m = table_meta(&meta, &ds, &t.name);
            let fks = crate::index_usage::foreign_keys(&meta, &project);
            for d in foreign_key_parents(&fks, t) {
                self.place(d, &project, &mut report);
            }
            if let Some((p, d, n, how)) = m.base {
                let base = Dependent {
                    kind: kinds::TABLE.into(),
                    schema: Some(d),
                    name: n,
                    parent: None,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: Some(how.into()),
                    mentions: Vec::new(),
                };
                if p.is_empty() || p == project {
                    self.place(base, &project, &mut report);
                } else {
                    report.external.push(ExternalRef {
                        system: Some("BigQuery".into()),
                        server: Some(p),
                        database: base.schema,
                        name: Some(base.name),
                        detail: base.detail,
                        ..Default::default()
                    });
                }
            }
            for (system, uri) in m.files {
                report.external.push(ExternalRef {
                    system: Some(system),
                    name: Some(uri),
                    detail: m.connection.clone(),
                    ..Default::default()
                });
            }
            // What Data Lineage saw feed it (free: no query job); nothing when it can't answer.
            if !self.api.emulator && target.column.is_none() {
                let (items, outside) = self.lineage_sources(lineage, t, &ds).await;
                for d in items {
                    self.place(d, &project, &mut report);
                }
                report.external.extend(outside);
            }
            m.body
        };
        if let Some(body) = body {
            report.scanned = 1;
            let found = body_names(&body, &scan.dialect, &project, &ds);
            let looked = self.look_up(&found.names, &project).await;
            for d in resolved(
                &found.names,
                &looked,
                &body,
                &project,
                self.dataset.as_deref(),
                t,
            ) {
                add(&mut report.items, d);
            }
            report
                .external
                .extend(other_projects(&found.names, &project, &body));
            report.external.extend(found.external);
        }
        sort(&mut report.items);
        Ok(report)
    }

    /// An item in this project: no schema in the connection's dataset.
    pub(crate) fn place(&self, mut d: Dependent, project: &str, report: &mut DependencyReport) {
        // `foreign_keys` writes another project's dataset as `project.dataset`.
        if let Some((p, ds)) = d.schema.as_deref().and_then(|s| s.split_once('.')) {
            if p != project {
                report.external.push(ExternalRef {
                    system: Some("BigQuery".into()),
                    server: Some(p.to_string()),
                    database: Some(ds.to_string()),
                    name: Some(d.name),
                    detail: d.detail,
                    ..Default::default()
                });
                return;
            }
        }
        if d.schema.is_some() && d.schema.as_deref() == self.dataset.as_deref() {
            d.schema = None;
        }
        add(&mut report.items, d);
    }

    /// The kinds of the names in this project that exist: (dataset, name,
    /// call) → kind. A dataset with many names is listed once.
    pub(crate) async fn look_up(
        &self,
        names: &[Name],
        project: &str,
    ) -> Vec<(String, String, bool, String)> {
        let mine: Vec<&Name> = names.iter().filter(|n| n.project == project).collect();
        let mut out = Vec::new();
        let mut datasets: Vec<&str> = mine.iter().map(|n| n.dataset.as_str()).collect();
        datasets.sort_unstable();
        datasets.dedup();
        for ds in datasets {
            for call in [false, true] {
                let here: Vec<&&Name> = mine
                    .iter()
                    .filter(|n| n.dataset == ds && n.call == call)
                    .collect();
                if here.is_empty() {
                    continue;
                }
                let what = if call { "routines" } else { "tables" };
                if here.len() > GETS_PER_DATASET {
                    let Ok(items) = self.api.list_all(&["datasets", ds, what], what).await else {
                        continue;
                    };
                    for item in &items {
                        let (key, kind) = if call {
                            ("/routineReference/routineId", routine_kind(item))
                        } else {
                            ("/tableReference/tableId", table_kind(item))
                        };
                        if let Some(name) = item.pointer(key).and_then(Json::as_str) {
                            if here.iter().any(|n| n.name == name) {
                                out.push((
                                    ds.to_string(),
                                    name.to_string(),
                                    call,
                                    kind.to_string(),
                                ));
                            }
                        }
                    }
                    continue;
                }
                for n in here {
                    if let Ok(item) = self.api.get(&["datasets", ds, what, &n.name], &[]).await {
                        let kind = if call {
                            routine_kind(&item)
                        } else {
                            table_kind(&item)
                        };
                        out.push((ds.to_string(), n.name.clone(), call, kind.to_string()));
                    }
                }
            }
        }
        out
    }
}

/// The names in this project that `looked` found, as items. The
/// connection's dataset has no schema; the target itself is left out.
pub(crate) fn resolved(
    names: &[Name],
    looked: &[(String, String, bool, String)],
    body: &str,
    project: &str,
    session_dataset: Option<&str>,
    target: &ObjectRef,
) -> Vec<Dependent> {
    let target_ds = target.schema().or(session_dataset).unwrap_or_default();
    let mut out = Vec::new();
    for n in names.iter().filter(|n| n.project == project) {
        let Some((_, _, _, kind)) = looked
            .iter()
            .find(|(d, name, call, _)| *d == n.dataset && *name == n.name && *call == n.call)
        else {
            continue;
        };
        if n.dataset == target_ds && n.name == target.name && *kind == target.kind {
            continue;
        }
        add(
            &mut out,
            Dependent {
                kind: kind.clone(),
                schema: (Some(n.dataset.as_str()) != session_dataset).then(|| n.dataset.clone()),
                name: n.name.clone(),
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: mentions_at(body, &n.at),
            },
        );
    }
    out
}

/// The names in other projects, as sources outside.
pub(crate) fn other_projects(names: &[Name], project: &str, body: &str) -> Vec<ExternalRef> {
    names
        .iter()
        .filter(|n| n.project != project)
        .map(|n| ExternalRef {
            system: Some("BigQuery".into()),
            server: Some(n.project.clone()),
            database: Some(n.dataset.clone()),
            name: Some(n.name.clone()),
            schema: None,
            detail: None,
            mentions: mentions_at(body, &n.at),
        })
        .collect()
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

/// Adds `d`, or merges it into the same object already there.
pub(crate) fn add(items: &mut Vec<Dependent>, d: Dependent) {
    match items
        .iter_mut()
        .find(|x| x.relation == d.relation && x.schema == d.schema && x.name == d.name)
    {
        Some(x) => {
            x.confidence = x.confidence.min(d.confidence);
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
    use serde_json::json;

    fn names(body: &str) -> Vec<(String, String, String, bool)> {
        body_names(body, &crate::script::dialect(), "test", "ds1")
            .names
            .into_iter()
            .map(|n| (n.project, n.dataset, n.name, n.call))
            .collect()
    }

    fn n(p: &str, d: &str, t: &str, call: bool) -> (String, String, String, bool) {
        (p.into(), d.into(), t.into(), call)
    }

    #[test]
    fn names_written_every_way() {
        let body = "CREATE VIEW `ds1`.`v` AS\n\
                    SELECT * FROM `test.ds1.orders` o\n\
                    JOIN `test`.`ds2`.customers c USING (id)\n\
                    JOIN ds2.regions r ON r.id = c.region\n\
                    JOIN `other-proj.sales.leads` l ON l.id = o.id\n\
                    JOIN `test.ds1`.orders o2 ON o2.id = o.id\n\
                    CROSS JOIN ds1.split_tags(o.tags) t\n\
                    WHERE ds2.normalize(o.code) = UPPER(o.code)\n\
                    AND o.id IN (SELECT id FROM `test`.ds1.INFORMATION_SCHEMA.TABLES)\n\
                    AND o.id IN (SELECT id FROM `region-us`.INFORMATION_SCHEMA.JOBS)";
        assert_eq!(
            names(body),
            [
                n("test", "ds1", "orders", false),
                n("test", "ds2", "customers", false),
                n("test", "ds2", "regions", false),
                n("other-proj", "sales", "leads", false),
                n("test", "ds1", "split_tags", true),
                n("test", "ds2", "normalize", true),
            ]
        );
        // A bare name is in the object's dataset (a CTE drops out later);
        // a bare call is a built-in.
        assert_eq!(
            names("WITH x AS (SELECT 1) SELECT SAFE_DIVIDE(a, b) FROM x JOIN events USING (id)"),
            [
                n("test", "ds1", "x", false),
                n("test", "ds1", "events", false)
            ]
        );
    }

    #[test]
    fn a_federated_query_is_a_source_outside() {
        let body = "SELECT * FROM EXTERNAL_QUERY(\"test.us.crm\", \"SELECT id, name FROM public.customers c JOIN orders o ON o.cid = c.id\")\n\
                    UNION ALL SELECT * FROM EXTERNAL_QUERY('test.us.erp', 'SELECT 1')";
        let f = body_names(body, &crate::script::dialect(), "test", "ds1");
        assert!(f.names.is_empty(), "{:?}", f.names);
        let got: Vec<_> = f
            .external
            .iter()
            .map(|x| {
                (
                    x.system.as_deref(),
                    x.server.as_deref(),
                    x.database.as_deref(),
                    x.name.as_deref(),
                    x.mentions.first().map(|m| m.line),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    Some("EXTERNAL_QUERY"),
                    Some("test.us.crm"),
                    Some("public"),
                    Some("customers"),
                    Some(1)
                ),
                (
                    Some("EXTERNAL_QUERY"),
                    Some("test.us.crm"),
                    None,
                    Some("orders"),
                    Some(1)
                ),
                (
                    Some("EXTERNAL_QUERY"),
                    Some("test.us.erp"),
                    None,
                    None,
                    Some(2)
                ),
            ]
        );
    }

    #[test]
    fn what_a_tables_get_answer_says() {
        let v = json!({"type": "VIEW", "view": {"query": "SELECT 1\nFROM ds1.t", "useLegacySql": false}});
        let m = table_meta(&v, "ds1", "v");
        assert_eq!(
            m.body.as_deref(),
            Some("CREATE VIEW `ds1`.`v` AS\nSELECT 1\nFROM ds1.t")
        );
        assert_eq!(table_kind(&v), kinds::VIEW);
        let mv = json!({"type": "MATERIALIZED_VIEW", "materializedView": {"query": "SELECT 2"}});
        assert_eq!(
            table_meta(&mv, "ds1", "mv").body.as_deref(),
            Some("CREATE MATERIALIZED VIEW `ds1`.`mv` AS\nSELECT 2")
        );
        let snap = json!({"type": "SNAPSHOT", "snapshotDefinition": {"baseTableReference": {"projectId": "test", "datasetId": "ds2", "tableId": "orders"}}});
        assert_eq!(
            table_meta(&snap, "ds1", "s").base,
            Some(("test".into(), "ds2".into(), "orders".into(), "SNAPSHOT"))
        );
        let clone = json!({"cloneDefinition": {"baseTableReference": {"projectId": "p2", "datasetId": "d", "tableId": "t"}}});
        assert_eq!(
            table_meta(&clone, "ds1", "c").base.map(|b| b.3),
            Some("CLONE")
        );
        let ext = json!({"type": "EXTERNAL", "externalDataConfiguration": {
            "sourceFormat": "CSV",
            "sourceUris": ["gs://bucket/in/*.csv", "https://user:pw@files.example/a.csv?sig=1"],
            "connectionId": "test.us.lake"}});
        let m = table_meta(&ext, "ds1", "e");
        assert_eq!(
            m.files,
            [
                (
                    "Cloud Storage".to_string(),
                    "gs://bucket/in/*.csv".to_string()
                ),
                ("URL".into(), "https://files.example/a.csv".into())
            ]
        );
        assert_eq!(m.connection.as_deref(), Some("test.us.lake"));
        let sheet = json!({"externalDataConfiguration": {"sourceFormat": "GOOGLE_SHEETS", "sourceUris": ["https://docs.google.com/spreadsheets/d/abc"]}});
        assert_eq!(table_meta(&sheet, "ds1", "g").files[0].0, "Google Sheets");
        assert_eq!(
            table_meta(&json!({"type": "TABLE"}), "ds1", "t"),
            TableMeta::default()
        );
    }

    #[test]
    fn what_a_routines_get_answer_says() {
        let sql = json!({"routineType": "SCALAR_FUNCTION", "language": "SQL", "definitionBody": "(SELECT COUNT(*) FROM ds1.t) + x"});
        assert_eq!(
            routine_meta(&sql).0.as_deref(),
            Some("(SELECT COUNT(*) FROM ds1.t) + x")
        );
        assert_eq!(routine_kind(&sql), kinds::FUNCTION);
        let proc = json!({"routineType": "PROCEDURE", "definitionBody": "BEGIN\n  INSERT INTO ds1.log SELECT * FROM ds2.src;\nEND"});
        assert!(routine_meta(&proc).0.is_some());
        assert_eq!(routine_kind(&proc), kinds::PROCEDURE);
        let js = json!({"language": "JAVASCRIPT", "definitionBody": "return lib.f(x);", "importedLibraries": ["gs://libs/lib.js"]});
        let (body, x) = routine_meta(&js);
        assert_eq!(body, None);
        assert_eq!(
            (x[0].system.as_deref(), x[0].name.as_deref()),
            (Some("Cloud Storage"), Some("gs://libs/lib.js"))
        );
        let remote = json!({"remoteFunctionOptions": {"endpoint": "https://fn.example/run?key=secret", "connection": "projects/test/locations/us/connections/fn"}});
        let (_, x) = routine_meta(&remote);
        assert_eq!(
            (
                x[0].system.as_deref(),
                x[0].name.as_deref(),
                x[0].detail.as_deref()
            ),
            (
                Some("HTTP"),
                Some("https://fn.example/run"),
                Some("projects/test/locations/us/connections/fn")
            )
        );
    }

    #[test]
    fn found_names_become_items_and_other_projects_sources_outside() {
        let body = "CREATE VIEW `ds1`.`v` AS\nSELECT * FROM ds1.orders\nJOIN ds2.customers USING (id)\nJOIN `p2.sales.leads` USING (id)\nJOIN ds1.cte_or_alias USING (id)";
        let f = body_names(body, &crate::script::dialect(), "test", "ds1");
        let looked = [
            (
                "ds1".to_string(),
                "orders".to_string(),
                false,
                kinds::TABLE.to_string(),
            ),
            ("ds2".into(), "customers".into(), false, kinds::VIEW.into()),
        ];
        let target = ObjectRef {
            kind: kinds::VIEW.into(),
            schema: None,
            name: "v".into(),
        };
        let items = resolved(&f.names, &looked, body, "test", Some("ds1"), &target);
        let got: Vec<_> = items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.schema.as_deref(),
                    d.name.as_str(),
                    d.confidence,
                    d.mentions[0].line,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (kinds::TABLE, None, "orders", Confidence::Probable, 2),
                (
                    kinds::VIEW,
                    Some("ds2"),
                    "customers",
                    Confidence::Probable,
                    3
                )
            ]
        );
        let x = other_projects(&f.names, "test", body);
        assert_eq!(x.len(), 1);
        assert_eq!(
            (
                x[0].system.as_deref(),
                x[0].server.as_deref(),
                x[0].database.as_deref(),
                x[0].name.as_deref()
            ),
            (Some("BigQuery"), Some("p2"), Some("sales"), Some("leads"))
        );
        assert_eq!(x[0].mentions[0].line, 4);
        // No dataset on the connection: every item has its dataset.
        let items = resolved(
            &f.names,
            &looked,
            body,
            "test",
            None,
            &ObjectRef {
                schema: Some("ds1".into()),
                ..target
            },
        );
        assert_eq!(items[0].schema.as_deref(), Some("ds1"));
    }

    #[test]
    fn strings_and_locations() {
        assert_eq!(unquote(r#""a\"b""#), "a\"b");
        assert_eq!(unquote("'it\\'s'"), "it's");
        assert_eq!(unquote(r#""""SELECT 'x'""""#), "SELECT 'x'");
        assert_eq!(unquote(r"r'\d+'"), r"\d+");
        assert_eq!(clean_location("gs://b/k"), "gs://b/k");
        assert_eq!(clean_location("https://u:p@h/x?a=1"), "https://h/x");
    }
}

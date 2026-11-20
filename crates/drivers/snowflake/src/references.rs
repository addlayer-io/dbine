//! What an object uses (`Session::references`).
//!
//! - `SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES`, the `REFERENCING_*`
//!   columns the target, `REFERENCED_*` what it uses: views, materialized
//!   views, functions, streams, dynamic and external tables… Confirmed. An
//!   object of another database, and what DBine doesn't list (a stage, an
//!   integration), is `external`. The view needs access to ACCOUNT_USAGE
//!   (IMPORTED PRIVILEGES on the SNOWFLAKE database) and lags up to three
//!   hours.
//! - The target's own definition, read as the contract's default does
//!   (`used_names`, then a lookup by name): what the catalog doesn't record
//!   yet, and procedures (not tracked), stay Probable. A name qualified
//!   with another database (`otherdb.schema.t`) is `external` instead of a
//!   same-named object here.
//! - A table: its foreign keys (the default), and an external table's
//!   files (`INFORMATION_SCHEMA.EXTERNAL_TABLES.LOCATION`) as `external`.
//! - Without access to ACCOUNT_USAGE: the definition alone, and a note.

use crate::SnowflakeSession;
use dbine_driver::dependencies::{
    references_scan, used_names, used_objects, Confidence, DependencyReport, DependencyScan,
    DependencyTarget, Dependent, ExternalRef, Mention, Relation, UsedName, LINE_CHARS, LOOKUP_CAP,
    MAX_MENTIONS,
};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, DbObject, Error, ObjectQuery, Result, Session};

/// `?` the database, the schema and the name of the target.
pub(crate) const DEPENDENCIES_SQL: &str =
    "SELECT referenced_database, referenced_schema, referenced_object_name,
       referenced_object_domain, referencing_object_domain
  FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES
 WHERE referencing_database = ? AND referencing_schema = ? AND referencing_object_name = ?
 ORDER BY 1, 2, 3";

/// An external table's files: `?` the schema and the name.
pub(crate) fn external_table_sql(database: &str) -> String {
    format!(
        "SELECT location, file_format_name, file_format_type FROM {}.INFORMATION_SCHEMA.EXTERNAL_TABLES
 WHERE table_schema = ? AND table_name = ?",
        qualified_name(Quote::Double, None, database)
    )
}

/// The catalog couldn't be read.
pub(crate) const NO_ACCOUNT_USAGE: &str = "Lo que usa sale de su definición, sin confirmar: confirmarlo con OBJECT_DEPENDENCIES necesita acceso a SNOWFLAKE.ACCOUNT_USAGE (IMPORTED PRIVILEGES sobre la base SNOWFLAKE).";
/// The catalog has nothing yet for an object whose definition uses something.
pub(crate) const LATENCY: &str = "SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES se actualiza con hasta tres horas de demora: lo que todavía no registra sale de la definición, sin confirmar.";

/// The kind DBine lists an object of this domain as; `None`: one it
/// doesn't list (a stage, an integration…).
pub(crate) fn domain_kind(domain: &str) -> Option<&'static str> {
    let d = domain.to_ascii_uppercase();
    Some(match d.as_str() {
        "VIEW" | "SECURE VIEW" => kinds::VIEW,
        "MATERIALIZED VIEW" | "SECURE MATERIALIZED VIEW" => kinds::MATERIALIZED_VIEW,
        "FUNCTION" | "EXTERNAL FUNCTION" | "SECURE FUNCTION" | "TABLE FUNCTION" => kinds::FUNCTION,
        "PROCEDURE" => kinds::PROCEDURE,
        "SEQUENCE" => kinds::SEQUENCE,
        "STREAM" => kinds::STREAM,
        "TASK" => crate::ddl::TASK,
        d if d == "TABLE" || d.ends_with(" TABLE") => kinds::TABLE,
        _ => return None,
    })
}

/// Whether a referencing domain is the target's kind (a function and a
/// view may share a name).
fn domain_fits(domain: &str, kind: &str) -> bool {
    domain_kind(domain) == Some(kind)
}

/// One `OBJECT_DEPENDENCIES` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub domain: String,
    pub referencing_domain: String,
}

pub(crate) fn rows(raw: &[Vec<Option<String>>]) -> Vec<Row> {
    raw.iter()
        .filter_map(|r| {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let name = r.get(2).cloned().flatten()?;
            Some(Row {
                database: s(0),
                schema: s(1),
                name,
                domain: s(3),
                referencing_domain: s(4),
            })
        })
        .collect()
}

/// Adds the catalog's rows for a target of `kind` in `database` to
/// `report`: confirmed items, or sources outside.
pub(crate) fn merge_catalog(
    report: &mut DependencyReport,
    rows: &[Row],
    kind: &str,
    database: &str,
) {
    // Only the rows of the target's own kind, unless none is (a domain
    // this doesn't know).
    let fitting: Vec<&Row> = rows
        .iter()
        .filter(|r| domain_fits(&r.referencing_domain, kind))
        .collect();
    let rows: Vec<&Row> = if fitting.is_empty() {
        rows.iter().collect()
    } else {
        fitting
    };
    for r in rows {
        match domain_kind(&r.domain).filter(|_| r.database.eq_ignore_ascii_case(database)) {
            Some(k) => add(
                &mut report.items,
                Dependent {
                    kind: k.to_string(),
                    schema: Some(r.schema.clone()),
                    name: r.name.clone(),
                    parent: None,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: None,
                    mentions: Vec::new(),
                },
            ),
            None => add_external(
                &mut report.external,
                ExternalRef {
                    system: Some("Snowflake".into()),
                    server: None,
                    database: Some(r.database.clone()),
                    schema: Some(r.schema.clone()),
                    name: Some(r.name.clone()),
                    detail: Some(r.domain.clone()),
                    mentions: Vec::new(),
                },
            ),
        }
    }
}

/// An external table's files (`LOCATION`) as a source outside.
pub(crate) fn external_table(raw: &[Vec<Option<String>>]) -> Option<ExternalRef> {
    let r = raw.first()?;
    let s = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
    let location = s(0)?;
    let lower = location.to_ascii_lowercase();
    let system = if location.starts_with('@') {
        "Snowflake stage"
    } else if lower.starts_with("s3://") || lower.starts_with("s3gov://") {
        "Amazon S3"
    } else if lower.starts_with("gcs://") {
        "Google Cloud Storage"
    } else if lower.starts_with("azure://") {
        "Azure Blob Storage"
    } else {
        "URL"
    };
    Some(ExternalRef {
        system: Some(system.into()),
        name: Some(clean_location(&location)),
        detail: s(1).or_else(|| s(2)),
        ..Default::default()
    })
}

/// A URL as shown: without user info, query string or fragment.
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

/// The names of `body` split: those in `database` (the database qualifier
/// dropped), and those qualified with another one, as sources outside.
pub(crate) fn split_names(
    body: &str,
    used: Vec<UsedName>,
    database: &str,
    system: &str,
) -> (Vec<UsedName>, Vec<ExternalRef>) {
    let mut here = Vec::new();
    let mut outside: Vec<ExternalRef> = Vec::new();
    for mut u in used {
        if u.quals.len() >= 2 {
            if !u.quals[1].eq_ignore_ascii_case(database) || u.quals.len() > 2 {
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
/// of another database are sources outside: the objects `body` names in
/// `database` (one lookup per name), Probable, with their lines.
pub(crate) async fn code_refs<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    scan: &DependencyScan,
    database: &str,
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
    let (here, outside) = split_names(&body, used_names(&body, &scan.dialect), database, system);
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

impl SnowflakeSession {
    pub(crate) async fn references_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let Ok(db) = self.database() else {
            return references_scan(self, target, scan).await;
        };
        let schema = t.schema().unwrap_or("PUBLIC").to_string();
        let mut report = if scan.written_kinds.contains(&t.kind) {
            let mut r = DependencyReport::default();
            code_refs(self, target, scan, &db, "Snowflake", &mut r).await?;
            r
        } else {
            // A table: its foreign keys.
            references_scan(self, target, scan).await?
        };
        let from_code = report.items.len() + report.external.len();
        match self
            .text_rows(DEPENDENCIES_SQL, &[&db, &schema, &t.name])
            .await
        {
            Ok(raw) => {
                let catalog = rows(&raw);
                if catalog.is_empty() && from_code > 0 {
                    report.note = Some(LATENCY.into());
                }
                merge_catalog(&mut report, &catalog, &t.kind, &db);
            }
            Err(e) => {
                tracing::debug!("snowflake OBJECT_DEPENDENCIES: {e}");
                report.note = Some(NO_ACCOUNT_USAGE.into());
            }
        }
        if t.kind == kinds::TABLE {
            if let Ok(raw) = self
                .text_rows(&external_table_sql(&db), &[&schema, &t.name])
                .await
            {
                if let Some(x) = external_table(&raw) {
                    add_external(&mut report.external, x);
                }
            }
        }
        sort(&mut report.items);
        Ok(report)
    }
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

/// Adds `d`, or merges it into the same object already there (names
/// compared ignoring case: the catalog spells them as stored).
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

/// Adds `x`, or merges it into the same source already there: the
/// catalog's spelling and detail, the mentions of both.
pub(crate) fn add_external(list: &mut Vec<ExternalRef>, x: ExternalRef) {
    match list.iter_mut().find(|e| {
        same(&e.system, &x.system)
            && same(&e.server, &x.server)
            && same(&e.database, &x.database)
            && same(&e.schema, &x.schema)
            && same(&e.name, &x.name)
    }) {
        Some(e) => {
            if x.mentions.is_empty() {
                // The catalog's row: its spelling and domain.
                e.database = x.database;
                e.schema = x.schema;
                e.name = x.name;
                e.detail = x.detail.or(e.detail.take());
            } else {
                e.mentions.extend(x.mentions);
                e.mentions.sort_by_key(|m| m.line);
                e.mentions.dedup_by_key(|m| m.line);
            }
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
    use dbine_driver::{ObjectPage, ObjectRef};

    fn raw(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|v| (!v.is_empty()).then(|| v.to_string()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_sql_sent() {
        assert!(DEPENDENCIES_SQL.contains("FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES"));
        assert!(DEPENDENCIES_SQL.contains(
            "WHERE referencing_database = ? AND referencing_schema = ? AND referencing_object_name = ?"
        ));
        assert!(DEPENDENCIES_SQL.starts_with(
            "SELECT referenced_database, referenced_schema, referenced_object_name,\n       referenced_object_domain, referencing_object_domain"
        ));
        assert_eq!(
            external_table_sql("Sales \"DB\""),
            "SELECT location, file_format_name, file_format_type FROM \"Sales \"\"DB\"\"\".INFORMATION_SCHEMA.EXTERNAL_TABLES\n WHERE table_schema = ? AND table_name = ?"
        );
    }

    #[test]
    fn catalog_rows_are_confirmed_items_or_sources_outside() {
        let catalog = rows(&raw(&[
            &["SALES", "PUBLIC", "ORDERS", "TABLE", "VIEW"],
            &["SALES", "REF", "RATES", "MATERIALIZED VIEW", "VIEW"],
            &["SALES", "PUBLIC", "TAX", "FUNCTION", "VIEW"],
            &["SALES", "PUBLIC", "RAW_STAGE", "STAGE", "VIEW"],
            &["CRM", "PUBLIC", "LEADS", "TABLE", "VIEW"],
            // The function of the same name: not the view's.
            &["SALES", "PUBLIC", "OTHER", "TABLE", "FUNCTION"],
            &["", "", "", "TABLE", "VIEW"],
        ]));
        assert_eq!(catalog.len(), 6);
        let mut report = DependencyReport::default();
        merge_catalog(&mut report, &catalog, kinds::VIEW, "sales");
        let items: Vec<_> = report
            .items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.schema.as_deref(),
                    d.name.as_str(),
                    d.confidence,
                )
            })
            .collect();
        assert_eq!(
            items,
            [
                (
                    kinds::TABLE,
                    Some("PUBLIC"),
                    "ORDERS",
                    Confidence::Confirmed
                ),
                (
                    kinds::MATERIALIZED_VIEW,
                    Some("REF"),
                    "RATES",
                    Confidence::Confirmed
                ),
                (
                    kinds::FUNCTION,
                    Some("PUBLIC"),
                    "TAX",
                    Confidence::Confirmed
                ),
            ]
        );
        let outside: Vec<_> = report
            .external
            .iter()
            .map(|x| {
                (
                    x.system.as_deref(),
                    x.database.as_deref(),
                    x.name.as_deref(),
                    x.detail.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            outside,
            [
                (
                    Some("Snowflake"),
                    Some("SALES"),
                    Some("RAW_STAGE"),
                    Some("STAGE")
                ),
                (Some("Snowflake"), Some("CRM"), Some("LEADS"), Some("TABLE")),
            ]
        );
    }

    #[test]
    fn domains() {
        assert_eq!(domain_kind("SECURE VIEW"), Some(kinds::VIEW));
        assert_eq!(domain_kind("DYNAMIC TABLE"), Some(kinds::TABLE));
        assert_eq!(domain_kind("EXTERNAL TABLE"), Some(kinds::TABLE));
        assert_eq!(domain_kind("external function"), Some(kinds::FUNCTION));
        assert_eq!(domain_kind("TASK"), Some(crate::ddl::TASK));
        assert_eq!(domain_kind("INTEGRATION"), None);
    }

    #[test]
    fn the_catalog_confirms_what_the_definition_names() {
        let mut report = DependencyReport::default();
        add(
            &mut report.items,
            Dependent {
                kind: kinds::TABLE.into(),
                schema: Some("PUBLIC".into()),
                name: "ORDERS".into(),
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: vec![Mention {
                    line: 3,
                    text: "FROM orders".into(),
                    dynamic: false,
                }],
            },
        );
        add_external(
            &mut report.external,
            ExternalRef {
                system: Some("Snowflake".into()),
                database: Some("crm".into()),
                schema: Some("public".into()),
                name: Some("leads".into()),
                mentions: vec![Mention {
                    line: 4,
                    text: "JOIN crm.public.leads".into(),
                    dynamic: false,
                }],
                ..Default::default()
            },
        );
        let catalog = rows(&raw(&[
            &["SALES", "PUBLIC", "ORDERS", "TABLE", "VIEW"],
            &["CRM", "PUBLIC", "LEADS", "TABLE", "VIEW"],
        ]));
        merge_catalog(&mut report, &catalog, kinds::VIEW, "SALES");
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].confidence, Confidence::Confirmed);
        assert_eq!(report.items[0].mentions[0].line, 3);
        assert_eq!(report.external.len(), 1);
        let x = &report.external[0];
        assert_eq!(
            (
                x.database.as_deref(),
                x.name.as_deref(),
                x.detail.as_deref(),
                x.mentions.len()
            ),
            (Some("CRM"), Some("LEADS"), Some("TABLE"), 1)
        );
    }

    #[test]
    fn an_external_tables_files() {
        let x = external_table(&raw(&[&["@SALES.PUBLIC.RAW/orders/", "CSV_FMT", "CSV"]])).unwrap();
        assert_eq!(
            (x.system.as_deref(), x.name.as_deref(), x.detail.as_deref()),
            (
                Some("Snowflake stage"),
                Some("@SALES.PUBLIC.RAW/orders/"),
                Some("CSV_FMT")
            )
        );
        let x = external_table(&raw(&[&["s3://bucket/orders/", "", "PARQUET"]])).unwrap();
        assert_eq!(
            (x.system.as_deref(), x.name.as_deref(), x.detail.as_deref()),
            (
                Some("Amazon S3"),
                Some("s3://bucket/orders/"),
                Some("PARQUET")
            )
        );
        assert!(external_table(&[]).is_none());
        assert!(external_table(&raw(&[&["", "", ""]])).is_none());
    }

    #[test]
    fn names_of_another_database_are_sources_outside() {
        let body = "create or replace view V as\nselect * from sales.public.orders o\njoin CRM.PUBLIC.LEADS l on l.id = o.id\njoin ref.rates r on true";
        let used = used_names(body, &crate::script::dialect());
        let (here, outside) = split_names(body, used, "SALES", "Snowflake");
        let here: Vec<_> = here
            .iter()
            .map(|u| (u.quals.clone(), u.name.as_str()))
            .collect();
        assert_eq!(
            here,
            [
                (vec!["public".to_string()], "orders"),
                (vec!["ref".to_string()], "rates")
            ]
        );
        assert_eq!(outside.len(), 1);
        assert_eq!(
            (
                outside[0].database.as_deref(),
                outside[0].schema.as_deref(),
                outside[0].name.as_deref()
            ),
            (Some("CRM"), Some("PUBLIC"), Some("LEADS"))
        );
        assert_eq!(outside[0].mentions[0].line, 3);
    }

    /// A session with fixed objects and one view's body.
    struct Fake {
        objects: Vec<DbObject>,
        lookups: u32,
    }

    #[dbine_driver::async_trait]
    impl Session for Fake {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            Ok(self.objects.clone())
        }
        async fn list_objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
            self.lookups += 1;
            let name = q.name.clone().unwrap_or_default();
            Ok(ObjectPage {
                objects: self
                    .objects
                    .iter()
                    .filter(|o| o.name.eq_ignore_ascii_case(&name))
                    .cloned()
                    .collect(),
                next: None,
            })
        }
        async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<dbine_driver::ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
            Ok((o.name == "V").then(|| {
                "create or replace view V as\nselect * from public.orders o\njoin CRM.PUBLIC.ORDERS c on c.id = o.id\nwhere public.tax(o.total) > 0".to_string()
            }))
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(
            &mut self,
            _: &str,
            _: usize,
            _: &mut dbine_driver::QueryOutcome,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_definitions_names_resolved_and_another_databases_outside() {
        let obj = |kind: &str, name: &str| DbObject {
            kind: kind.into(),
            schema: Some("PUBLIC".into()),
            name: name.into(),
            parent: None,
        };
        let mut s = Fake {
            objects: vec![
                obj(kinds::TABLE, "ORDERS"),
                obj(kinds::FUNCTION, "TAX"),
                obj(kinds::VIEW, "V"),
            ],
            lookups: 0,
        };
        let target = DependencyTarget {
            object: ObjectRef {
                kind: kinds::VIEW.into(),
                schema: Some("PUBLIC".into()),
                name: "V".into(),
            },
            column: None,
        };
        let scan = DependencyScan {
            source_kinds: vec![kinds::VIEW.into()],
            dialect: crate::script::dialect(),
            foreign_keys: true,
            written_kinds: vec![kinds::VIEW.into()],
        };
        let mut r = DependencyReport::default();
        code_refs(&mut s, &target, &scan, "SALES", "Snowflake", &mut r)
            .await
            .unwrap();
        let items: Vec<_> = r
            .items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.name.as_str(),
                    d.confidence,
                    d.mentions[0].line,
                )
            })
            .collect();
        // `CRM.PUBLIC.ORDERS` is another database's, not this ORDERS.
        assert_eq!(
            items,
            [
                (kinds::TABLE, "ORDERS", Confidence::Probable, 2),
                (kinds::FUNCTION, "TAX", Confidence::Probable, 4)
            ]
        );
        assert_eq!(r.external.len(), 1);
        assert_eq!(r.external[0].database.as_deref(), Some("CRM"));
        assert_eq!((r.scanned, s.lookups), (1, 2));
    }
}

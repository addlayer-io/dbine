//! The lineage of ONE table or view: what reads it and what it reads.
//!
//! Snowflake's own call for it is `SNOWFLAKE.CORE.GET_LINEAGE`, a table
//! function that answers for the one object asked, one level at a time
//! (`max_distance` 1; the app expands a neighbor by asking again about it).
//! It is **Confirmed**, and it covers what the structural catalog doesn't:
//! the movement of data (`CREATE TABLE AS SELECT`, `INSERT … SELECT`,
//! `MERGE`, `UPDATE … FROM`, `COPY INTO`, clones) as well as views.
//!
//! - Function and its arguments (`object_name`, `object_domain`,
//!   `direction`, `max_distance`), output columns (`SOURCE_OBJECT_*`,
//!   `TARGET_OBJECT_*`, `DISTANCE`, `PROCESS`, `*_DETAILS` with
//!   `dataset_type`) and the Enterprise Edition requirement:
//!   <https://docs.snowflake.com/en/sql-reference/functions/get_lineage-snowflake-core>
//! - Supported operations, objects, the `VIEW LINEAGE` privilege (granted to
//!   PUBLIC by default; `RESOLVE ALL` sees everything), retention (one year;
//!   data movement before November 2024 isn't there) and limits (shared
//!   databases, the `SNOWFLAKE` database and `INFORMATION_SCHEMA` have no
//!   lineage; a procedure can't be asked about):
//!   <https://docs.snowflake.com/en/user-guide/ui-snowsight-lineage>
//! - `ACCOUNT_USAGE.OBJECT_DEPENDENCIES` (structural, up to three hours of
//!   latency, every edition, needs IMPORTED PRIVILEGES on `SNOWFLAKE`):
//!   <https://docs.snowflake.com/en/sql-reference/account-usage/object_dependencies>
//!
//! What reads a table is that call (downstream), completed for that one
//! object only with the structural catalog (`WHERE referenced_* = ?`, never
//! the whole account) and the foreign keys that point at it. Without the
//! call (an edition below Enterprise, a missing privilege) the caller falls
//! back to the contract's scan and says so. Nothing here reads a definition.

use crate::references::{add, add_external, domain_fits, domain_kind, sort};
use dbine_driver::dependencies::{Confidence, DependencyReport, Dependent, ExternalRef, Relation};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, ObjectRef, Result};

/// What the lineage code asks of the session (a fake in the tests).
#[dbine_driver::async_trait]
pub(crate) trait Catalog: Sync {
    /// Text rows of a query with `?` bindings.
    async fn rows(&self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Option<String>>>>;
}

/// The call isn't available for the object.
pub(crate) const NO_LINEAGE: &str = "El linaje de datos de Snowflake (SNOWFLAKE.CORE.GET_LINEAGE) no está disponible para este objeto: hace falta Enterprise Edition o superior y el privilegio VIEW LINEAGE. Se muestran las dependencias estructurales y las definiciones, sin el movimiento de datos (CREATE TABLE AS SELECT, INSERT, MERGE…).";

/// Which way the lineage is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// What the object was made from (what it reads).
    Upstream,
    /// What was made from the object (what reads it).
    Downstream,
}

/// Whether the call answers for objects of this kind (a function, a
/// procedure, a sequence have no data lineage).
pub(crate) fn supported(kind: &str) -> bool {
    matches!(kind, kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW)
}

/// A single-quoted SQL string: `\` and `'` escaped.
pub(crate) fn literal(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''"))
}

/// The one call for one object, one level: every column read by position
/// (see [`parse_edges`]).
pub(crate) fn lineage_sql(database: &str, schema: &str, name: &str, dir: Direction) -> String {
    let full = format!(
        "{}.{}.{}",
        quote_ident(Quote::Double, database),
        quote_ident(Quote::Double, schema),
        quote_ident(Quote::Double, name)
    );
    let dir = match dir {
        Direction::Upstream => "UPSTREAM",
        Direction::Downstream => "DOWNSTREAM",
    };
    format!(
        "SELECT source_object_database, source_object_schema, source_object_name, source_object_domain, source_details,
       target_object_database, target_object_schema, target_object_name, target_object_domain, target_details, distance
  FROM TABLE(SNOWFLAKE.CORE.GET_LINEAGE({}, 'TABLE', '{dir}', 1))",
        literal(&full)
    )
}

/// What reads the object, from the structural catalog, for this object only:
/// `?` the database, the schema and the name of the referenced object.
pub(crate) const STRUCTURAL_DEPENDENTS_SQL: &str =
    "SELECT referencing_database, referencing_schema, referencing_object_name,
       referencing_object_domain, referenced_object_domain
  FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES
 WHERE referenced_database = ? AND referenced_schema = ? AND referenced_object_name = ?
 ORDER BY 1, 2, 3";

/// The tables whose foreign keys point at one table: `?` the schema and the
/// name of the referenced table.
pub(crate) fn foreign_key_children_sql(database: &str) -> String {
    let is = format!(
        "{}.INFORMATION_SCHEMA",
        quote_ident(Quote::Double, database)
    );
    format!(
        "SELECT DISTINCT fk.constraint_name, fk.table_schema, fk.table_name
  FROM {is}.REFERENTIAL_CONSTRAINTS rc
  JOIN {is}.TABLE_CONSTRAINTS fk
    ON fk.constraint_schema = rc.constraint_schema AND fk.constraint_name = rc.constraint_name
  JOIN {is}.TABLE_CONSTRAINTS pk
    ON pk.constraint_schema = rc.unique_constraint_schema AND pk.constraint_name = rc.unique_constraint_name
 WHERE pk.table_schema = ? AND pk.table_name = ?"
    )
}

/// One relationship of the lineage: `source` is upstream of `target`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Node {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub domain: String,
    /// `dataset_type` of the details: more precise than the domain (a
    /// `TABLE` is a table or a view).
    pub dataset_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Edge {
    pub source: Node,
    pub target: Node,
    pub distance: Option<u32>,
}

fn dataset_type(details: Option<&String>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(details?).ok()?;
    v.get("dataset_type")?.as_str().map(str::to_string)
}

/// [`lineage_sql`]'s rows.
pub(crate) fn parse_edges(raw: &[Vec<Option<String>>]) -> Vec<Edge> {
    raw.iter()
        .filter_map(|r| {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let node = |at: usize| Node {
                database: s(at),
                schema: s(at + 1),
                name: s(at + 2),
                domain: s(at + 3),
                dataset_type: dataset_type(r.get(at + 4).and_then(Option::as_ref)),
            };
            let (source, target) = (node(0), node(5));
            if source.name.is_empty() || target.name.is_empty() {
                return None;
            }
            Some(Edge {
                source,
                target,
                distance: r.get(10).cloned().flatten().and_then(|d| d.parse().ok()),
            })
        })
        .collect()
}

fn is_anchor(n: &Node, database: &str, t: &ObjectRef) -> bool {
    n.database.eq_ignore_ascii_case(database)
        && n.schema.eq_ignore_ascii_case(t.schema().unwrap_or(""))
        && n.name.eq_ignore_ascii_case(&t.name)
}

/// The kind DBine lists a lineage node as; `None`: one it doesn't (a stage,
/// an integration, a model…).
fn node_kind(n: &Node) -> Option<&'static str> {
    let precise = |d: &str| domain_kind(&d.replace('_', " "));
    if n.domain.eq_ignore_ascii_case("COLUMN") {
        return None;
    }
    n.dataset_type
        .as_deref()
        .and_then(precise)
        .or_else(|| precise(&n.domain))
}

/// The neighbors of `t` in the edges of one level: this database's objects
/// as **Confirmed** items, everything else as sources outside.
pub(crate) fn neighbors(
    edges: &[Edge],
    dir: Direction,
    database: &str,
    t: &ObjectRef,
) -> (Vec<Dependent>, Vec<ExternalRef>) {
    let mut items = Vec::new();
    let mut external = Vec::new();
    for e in edges {
        // One level only, and never a column: the object's own lineage.
        if e.distance.is_some_and(|d| d != 1) {
            continue;
        }
        let (anchor, other) = match dir {
            Direction::Downstream => (&e.source, &e.target),
            Direction::Upstream => (&e.target, &e.source),
        };
        if !is_anchor(anchor, database, t) || other.domain.eq_ignore_ascii_case("COLUMN") {
            continue;
        }
        match node_kind(other).filter(|_| other.database.eq_ignore_ascii_case(database)) {
            Some(k) => {
                if is_anchor(other, database, t) {
                    continue;
                }
                add(&mut items, dependent(k, &other.schema, &other.name));
            }
            None => add_external(
                &mut external,
                ExternalRef {
                    system: Some("Snowflake".into()),
                    server: None,
                    database: Some(other.database.clone()).filter(|d| !d.is_empty()),
                    schema: Some(other.schema.clone()).filter(|d| !d.is_empty()),
                    name: Some(other.name.clone()),
                    detail: Some(other.domain.clone()),
                    mentions: Vec::new(),
                },
            ),
        }
    }
    (items, external)
}

fn dependent(kind: &str, schema: &str, name: &str) -> Dependent {
    Dependent {
        kind: kind.to_string(),
        schema: Some(schema.to_string()),
        name: name.to_string(),
        parent: None,
        relation: Relation::Code,
        confidence: Confidence::Confirmed,
        detail: None,
        mentions: Vec::new(),
    }
}

/// The structural catalog's rows as what reads the target (kind `kind`).
pub(crate) fn structural_dependents(
    raw: &[Vec<Option<String>>],
    kind: &str,
    database: &str,
) -> (Vec<Dependent>, Vec<ExternalRef>) {
    let mut items = Vec::new();
    let mut external = Vec::new();
    for r in raw {
        let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let (db, schema, name, domain, referenced) = (s(0), s(1), s(2), s(3), s(4));
        if name.is_empty() || (!referenced.is_empty() && !domain_fits(&referenced, kind)) {
            continue;
        }
        match domain_kind(&domain).filter(|_| db.eq_ignore_ascii_case(database)) {
            Some(k) => add(&mut items, dependent(k, &schema, &name)),
            None => add_external(
                &mut external,
                ExternalRef {
                    system: Some("Snowflake".into()),
                    database: Some(db).filter(|d| !d.is_empty()),
                    schema: Some(schema).filter(|d| !d.is_empty()),
                    name: Some(name),
                    detail: Some(domain),
                    ..Default::default()
                },
            ),
        }
    }
    (items, external)
}

/// What the native reads found for one object.
pub(crate) struct Native {
    pub report: DependencyReport,
    /// `GET_LINEAGE` answered (even with no rows). Otherwise the caller
    /// falls back to the contract's scan, with [`NO_LINEAGE`].
    pub lineage: bool,
}

/// What reads the table or view `t` of `database`: three reads, all for this
/// object only.
pub(crate) async fn dependents<C: Catalog + ?Sized>(
    c: &C,
    database: &str,
    t: &ObjectRef,
    foreign_keys: bool,
) -> Native {
    let schema = t.schema().unwrap_or("PUBLIC");
    let mut report = DependencyReport::default();
    let sql = lineage_sql(database, schema, &t.name, Direction::Downstream);
    let lineage = match c.rows(&sql, &[]).await {
        Ok(raw) => {
            let (items, external) =
                neighbors(&parse_edges(&raw), Direction::Downstream, database, t);
            for d in items {
                add(&mut report.items, d);
            }
            for x in external {
                add_external(&mut report.external, x);
            }
            true
        }
        Err(e) => {
            tracing::debug!("snowflake GET_LINEAGE of {schema}.{}: {e}", t.name);
            false
        }
    };
    match c
        .rows(STRUCTURAL_DEPENDENTS_SQL, &[database, schema, &t.name])
        .await
    {
        Ok(raw) => {
            let (items, external) = structural_dependents(&raw, &t.kind, database);
            for d in items {
                add(&mut report.items, d);
            }
            for x in external {
                add_external(&mut report.external, x);
            }
        }
        Err(e) => tracing::debug!("snowflake OBJECT_DEPENDENCIES of {schema}.{}: {e}", t.name),
    }
    if foreign_keys && t.kind == kinds::TABLE {
        match c
            .rows(&foreign_key_children_sql(database), &[schema, &t.name])
            .await
        {
            Ok(raw) => {
                for r in raw {
                    let v = |i: usize| r.get(i).cloned().flatten();
                    let (Some(constraint), Some(sch), Some(table)) = (v(0), v(1), v(2)) else {
                        continue;
                    };
                    add(
                        &mut report.items,
                        Dependent {
                            kind: kinds::TABLE.to_string(),
                            schema: Some(sch),
                            name: table,
                            parent: None,
                            relation: Relation::ForeignKey,
                            confidence: Confidence::Confirmed,
                            detail: Some(constraint),
                            mentions: Vec::new(),
                        },
                    );
                }
            }
            Err(e) => {
                tracing::debug!("snowflake foreign keys to {schema}.{}: {e}", t.name);
                report.unreadable.push(format!("{schema}.{}", t.name));
            }
        }
    }
    sort(&mut report.items);
    Native { report, lineage }
}

/// What the table or view `t` reads, one level: `None` when the call isn't
/// available.
pub(crate) async fn upstream<C: Catalog + ?Sized>(
    c: &C,
    database: &str,
    t: &ObjectRef,
) -> Option<(Vec<Dependent>, Vec<ExternalRef>)> {
    let sql = lineage_sql(
        database,
        t.schema().unwrap_or("PUBLIC"),
        &t.name,
        Direction::Upstream,
    );
    match c.rows(&sql, &[]).await {
        Ok(raw) => Some(neighbors(
            &parse_edges(&raw),
            Direction::Upstream,
            database,
            t,
        )),
        Err(e) => {
            tracing::debug!("snowflake GET_LINEAGE (upstream) of {}: {e}", t.name);
            None
        }
    }
}

/// `native` merged into the contract's scan `scanned` (the fallback): the
/// structural rows and foreign keys read for the object stay **Confirmed**,
/// and the note says what is missing.
pub(crate) fn merge_fallback(scanned: &mut DependencyReport, native: Native) {
    for d in native.report.items {
        add(&mut scanned.items, d);
    }
    for x in native.report.external {
        add_external(&mut scanned.external, x);
    }
    scanned.note = Some(NO_LINEAGE.into());
    sort(&mut scanned.items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        answers: Vec<(&'static str, Vec<Vec<Option<String>>>)>,
        seen: Mutex<Vec<(String, Vec<String>)>>,
    }

    #[dbine_driver::async_trait]
    impl Catalog for Fake {
        async fn rows(&self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
            self.seen.lock().unwrap().push((
                sql.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
            ));
            self.answers
                .iter()
                .find(|(n, _)| sql.contains(n))
                .map(|(_, r)| r.clone())
                .ok_or_else(|| dbine_driver::Error::Query("Insufficient privileges".into()))
        }
    }

    fn fake(answers: Vec<(&'static str, Vec<Vec<Option<String>>>)>) -> Fake {
        Fake {
            answers,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn raw(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|v| (!v.is_empty()).then(|| v.to_string()))
                    .collect()
            })
            .collect()
    }

    fn orders() -> ObjectRef {
        ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("PUBLIC".into()),
            name: "ORDERS".into(),
        }
    }

    const VIEW: &str = r#"{"dataset_type":"VIEW","origin":"NATIVE"}"#;
    const TABLE: &str = r#"{"dataset_type":"TABLE"}"#;

    /// Downstream of SALES.PUBLIC.ORDERS, one level.
    fn downstream() -> Vec<Vec<Option<String>>> {
        raw(&[
            // A view, and a table made by CTAS or INSERT … SELECT.
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", TABLE, "SALES", "PUBLIC", "V_ORDERS",
                "TABLE", VIEW, "1",
            ],
            &[
                "SALES",
                "ETL",
                "ORDERS_COPY",
                "TABLE",
                TABLE,
                "",
                "",
                "",
                "",
                "",
                "1",
            ],
            &[
                "SALES",
                "PUBLIC",
                "ORDERS",
                "TABLE",
                TABLE,
                "SALES",
                "ETL",
                "ORDERS_COPY",
                "TABLE",
                TABLE,
                "1",
            ],
            // Another database, a stage, a column and a deeper level.
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", TABLE, "DWH", "RAW", "ORDERS", "TABLE",
                TABLE, "1",
            ],
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", TABLE, "SALES", "PUBLIC", "UNLOAD", "STAGE",
                "", "1",
            ],
            &[
                "SALES", "PUBLIC", "ORDERS", "COLUMN", "", "SALES", "PUBLIC", "V_ORDERS", "COLUMN",
                "", "1",
            ],
            &[
                "SALES", "PUBLIC", "V_ORDERS", "TABLE", VIEW, "SALES", "PUBLIC", "REPORT", "TABLE",
                VIEW, "2",
            ],
            &[
                "SALES",
                "PUBLIC",
                "ORDERS",
                "TABLE",
                TABLE,
                "SALES",
                "PUBLIC",
                "ORDERS_EXT",
                "EXTERNAL_TABLE",
                "",
                "1",
            ],
        ])
    }

    #[test]
    fn the_call_is_for_one_object_one_level() {
        let sql = lineage_sql("Sales \"DB\"", "PUBLIC", "O'RDERS", Direction::Downstream);
        assert!(
            sql.contains(
                "FROM TABLE(SNOWFLAKE.CORE.GET_LINEAGE('\"Sales \"\"DB\"\"\".\"PUBLIC\".\"O''RDERS\"', 'TABLE', 'DOWNSTREAM', 1))"
            ),
            "{sql}"
        );
        assert!(lineage_sql("D", "S", "T", Direction::Upstream).contains("'UPSTREAM', 1)"));
        assert_eq!(literal("a\\b'c"), "'a\\\\b''c'");
        assert!(STRUCTURAL_DEPENDENTS_SQL
            .contains("WHERE referenced_database = ? AND referenced_schema = ? AND referenced_object_name = ?"));
    }

    #[test]
    fn what_reads_a_table_comes_from_the_downstream_edges() {
        let edges = parse_edges(&downstream());
        let (items, external) = neighbors(&edges, Direction::Downstream, "sales", &orders());
        let got: Vec<_> = items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.schema.as_deref().unwrap(),
                    d.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (kinds::VIEW, "PUBLIC", "V_ORDERS"),
                (kinds::TABLE, "ETL", "ORDERS_COPY"),
                (kinds::TABLE, "PUBLIC", "ORDERS_EXT"),
            ]
        );
        assert!(items.iter().all(|d| d.confidence == Confidence::Confirmed));
        // Another database and a stage are sources outside; the column, the
        // second level and the row not about the table are not there.
        let outside: Vec<_> = external
            .iter()
            .map(|x| {
                (
                    x.database.as_deref(),
                    x.name.as_deref(),
                    x.detail.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            outside,
            [
                (Some("DWH"), Some("ORDERS"), Some("TABLE")),
                (Some("SALES"), Some("UNLOAD"), Some("STAGE")),
            ]
        );
    }

    #[test]
    fn what_a_view_reads_comes_from_the_upstream_edges() {
        let rows = raw(&[
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", TABLE, "SALES", "PUBLIC", "V_ORDERS",
                "TABLE", VIEW, "1",
            ],
            &[
                "SALES", "PUBLIC", "RATES", "TABLE", "", "SALES", "PUBLIC", "V_ORDERS", "TABLE",
                VIEW, "1",
            ],
            &[
                "S3", "X", "FILE", "STAGE", "", "SALES", "PUBLIC", "V_ORDERS", "TABLE", VIEW, "1",
            ],
        ]);
        let view = ObjectRef {
            kind: kinds::VIEW.into(),
            schema: Some("PUBLIC".into()),
            name: "V_ORDERS".into(),
        };
        let (items, external) = neighbors(&parse_edges(&rows), Direction::Upstream, "SALES", &view);
        let names: Vec<_> = items.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["ORDERS", "RATES"]);
        assert_eq!(external.len(), 1);
    }

    #[tokio::test]
    async fn dependents_ask_for_the_one_object_and_merge_what_is_structural() {
        let f = fake(vec![
            ("GET_LINEAGE", downstream()),
            (
                "OBJECT_DEPENDENCIES",
                raw(&[
                    // Also in the lineage: one item, not two.
                    &["SALES", "PUBLIC", "V_ORDERS", "VIEW", "TABLE"],
                    // Only the catalog has it: a function.
                    &["SALES", "PUBLIC", "TAX", "FUNCTION", "TABLE"],
                    // Of another object of the same name: not a table's.
                    &["SALES", "PUBLIC", "OTHER", "VIEW", "VIEW"],
                ]),
            ),
            (
                "REFERENTIAL_CONSTRAINTS",
                raw(&[&["FK_LINES_ORDERS", "PUBLIC", "LINES"]]),
            ),
        ]);
        let n = dependents(&f, "SALES", &orders(), true).await;
        assert!(n.lineage);
        let got: Vec<_> = n
            .report
            .items
            .iter()
            .map(|d| (d.name.as_str(), d.relation, d.confidence))
            .collect();
        assert_eq!(
            got,
            [
                ("LINES", Relation::ForeignKey, Confidence::Confirmed),
                ("ORDERS_COPY", Relation::Code, Confidence::Confirmed),
                ("ORDERS_EXT", Relation::Code, Confidence::Confirmed),
                ("TAX", Relation::Code, Confidence::Confirmed),
                ("V_ORDERS", Relation::Code, Confidence::Confirmed),
            ]
        );
        // Three reads, each for this object: no definition, no whole account.
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].0.contains("'\"SALES\".\"PUBLIC\".\"ORDERS\"'"));
        assert_eq!(seen[1].1, ["SALES", "PUBLIC", "ORDERS"]);
        assert_eq!(seen[2].1, ["PUBLIC", "ORDERS"]);
    }

    #[tokio::test]
    async fn without_the_call_the_structural_rows_still_come_and_the_caller_falls_back() {
        // GET_LINEAGE fails (edition below Enterprise); the catalog answers.
        let f = fake(vec![(
            "OBJECT_DEPENDENCIES",
            raw(&[&["SALES", "PUBLIC", "V_ORDERS", "VIEW", "TABLE"]]),
        )]);
        let n = dependents(&f, "SALES", &orders(), false).await;
        assert!(!n.lineage);
        assert_eq!(n.report.items.len(), 1);
        // The contract's scan found a Probable view and a procedure.
        let mut scanned = DependencyReport {
            items: vec![
                Dependent {
                    confidence: Confidence::Probable,
                    ..dependent(kinds::VIEW, "PUBLIC", "V_ORDERS")
                },
                Dependent {
                    confidence: Confidence::Probable,
                    ..dependent(kinds::PROCEDURE, "PUBLIC", "LOAD")
                },
            ],
            ..Default::default()
        };
        merge_fallback(&mut scanned, n);
        let got: Vec<_> = scanned
            .items
            .iter()
            .map(|d| (d.name.as_str(), d.confidence))
            .collect();
        assert_eq!(
            got,
            [
                ("V_ORDERS", Confidence::Confirmed),
                ("LOAD", Confidence::Probable)
            ]
        );
        assert_eq!(scanned.note.as_deref(), Some(NO_LINEAGE));
    }

    #[tokio::test]
    async fn with_nothing_readable_the_report_is_empty_and_not_confirmed() {
        let f = fake(vec![]);
        let n = dependents(&f, "SALES", &orders(), true).await;
        assert!(!n.lineage);
        assert!(n.report.items.is_empty());
        assert_eq!(n.report.unreadable, ["PUBLIC.ORDERS"]);
    }

    #[tokio::test]
    async fn upstream_is_none_without_the_call() {
        let f = fake(vec![]);
        assert!(upstream(&f, "SALES", &orders()).await.is_none());
        let f = fake(vec![("UPSTREAM", vec![])]);
        let (items, external) = upstream(&f, "SALES", &orders()).await.unwrap();
        assert!(items.is_empty() && external.is_empty());
    }

    #[test]
    fn only_table_like_objects_ask() {
        assert!(supported(kinds::TABLE) && supported(kinds::VIEW));
        assert!(!supported(kinds::PROCEDURE) && !supported(kinds::FUNCTION));
    }
}

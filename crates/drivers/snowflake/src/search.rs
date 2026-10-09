//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! functions and procedures (INFORMATION_SCHEMA `FUNCTIONS` / `PROCEDURES`,
//! the CREATE `definition` builds from them, built by the same expression)
//! and sequences (`SEQUENCES`, through the same builder), matched line by
//! line with the contract's rule, so the hits equal those of the app's
//! per-object scan.
//!
//! Tables, views, streams and tasks come from `GET_DDL`, one call per
//! object: a search that includes them (or every kind) answers `None` and
//! the app scans. So does an overloaded routine that may hold the text:
//! `definition` joins its overloads in an order the catalog doesn't fix.

use crate::{ddl, SnowflakeSession};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, Result};
use std::collections::BTreeMap;

/// Where a routine's CREATE comes from, as `definition` reads it.
pub(crate) struct RoutineSource {
    pub view: &'static str,
    pub schema_col: &'static str,
    pub name_col: &'static str,
    /// Its CREATE (NULL when a part is).
    pub expr: &'static str,
}

/// The body after `AS` is wrapped in `$$`, unless it holds `$$` itself (one
/// written with `AS '…'`): then it goes as a '…' literal with `\` and `'`
/// escaped, so the body can't close the quoting early and leave text that
/// reads as more statements (the rename puts definitions back as scripts).
pub(crate) fn routine_source(kind: &str) -> RoutineSource {
    if kind == kinds::PROCEDURE {
        RoutineSource {
            view: "PROCEDURES",
            schema_col: "procedure_schema",
            name_col: "procedure_name",
            expr: r"'CREATE OR REPLACE PROCEDURE ' || procedure_name || argument_signature || ' RETURNS ' || data_type
                   || ' LANGUAGE ' || procedure_language || ' AS '
                   || CASE WHEN CONTAINS(procedure_definition, '$$')
                      THEN '''' || REPLACE(REPLACE(procedure_definition, '\\', '\\\\'), '''', '''''') || ''''
                      ELSE '$$' || procedure_definition || '$$' END || ';'",
        }
    } else {
        RoutineSource {
            view: "FUNCTIONS",
            schema_col: "function_schema",
            name_col: "function_name",
            expr: r"'CREATE OR REPLACE FUNCTION ' || function_name || argument_signature || ' RETURNS ' || data_type
                   || ' LANGUAGE ' || function_language || ' AS '
                   || CASE WHEN CONTAINS(function_definition, '$$')
                      THEN '''' || REPLACE(REPLACE(function_definition, '\\', '\\\\'), '''', '''''') || ''''
                      ELSE '$$' || function_definition || '$$' END || ';'",
        }
    }
}

/// The `SEQUENCES` columns [`sequence_ddl`] takes, in order.
pub(crate) const SEQUENCE_COLS: &str = "start_value, increment, ordered, comment";

/// A sequence's CREATE from its [`SEQUENCE_COLS`].
pub(crate) fn sequence_ddl(schema: &str, name: &str, r: &[Option<String>]) -> String {
    let keys = ["start_value", "increment", "ordered", "comment"];
    let row: ddl::Row = keys.iter().zip(r).filter_map(|(k, v)| Some((k.to_string(), v.clone()?))).collect();
    ddl::sequence_sql(Some(schema), name, &row)
}

const COVERED: [&str; 3] = [kinds::FUNCTION, kinds::PROCEDURE, kinds::SEQUENCE];

/// The server-side test for a CREATE that may hold the text, and its
/// bound value (`CONTAINS`: no wildcards to escape). A case-insensitive
/// search narrows on the server only for ASCII text, where `LOWER` folds
/// as Rust does; otherwise every source comes back.
fn narrow(expr: &str, q: &CodeSearch) -> (String, Option<String>) {
    if q.case_sensitive {
        (format!("CONTAINS({expr}, ?)"), Some(q.text.clone()))
    } else if q.text.is_ascii() {
        (format!("CONTAINS(LOWER({expr}), ?)"), Some(q.text.to_ascii_lowercase()))
    } else {
        ("TRUE".into(), None)
    }
}

/// Every routine of a kind in `list_objects`' schemas: schema, name,
/// whether its CREATE isn't NULL (1/0) and the CREATE where it may hold
/// the text.
fn routines_sql(is: &str, kind: &str, q: &CodeSearch) -> (String, Option<String>) {
    let r = routine_source(kind);
    let (cond, arg) = narrow(r.expr, q);
    let sql = format!(
        "SELECT {s}, {n}, IFF(({e}) IS NULL, 0, 1), IFF({cond}, {e}, NULL) FROM {is}.{v} WHERE {s} <> 'INFORMATION_SCHEMA'",
        s = r.schema_col,
        n = r.name_col,
        e = r.expr,
        v = r.view
    );
    (sql, arg)
}

fn sequences_sql(is: &str) -> String {
    format!("SELECT sequence_schema, sequence_name, {SEQUENCE_COLS} FROM {is}.SEQUENCES WHERE sequence_schema <> 'INFORMATION_SCHEMA'")
}

/// The kinds to read; `None` when one has no catalog source here (an
/// empty filter means every kind: tables among them).
fn plan(q: &CodeSearch) -> Option<Vec<&'static str>> {
    if q.text.is_empty() || q.kinds.is_empty() {
        return None;
    }
    let covered: Option<Vec<_>> = q.kinds.iter().map(|k| COVERED.iter().find(|c| *c == k).copied()).collect();
    covered.map(|c| COVERED.into_iter().filter(|k| c.contains(k)).collect())
}

/// (schema, name) → its source; `None` (the scan decides) when a name has
/// two non-NULL sources.
type Sources = BTreeMap<(String, String), Option<String>>;

impl SnowflakeSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        let Some(plan) = plan(q) else { return Ok(None) };
        let Ok(db) = self.database() else { return Ok(None) };
        let is = format!("{}.INFORMATION_SCHEMA", qualified_name(Quote::Double, None, &db));
        let mut report = CodeSearchReport::default();
        for kind in plan {
            let mut sources = Sources::new();
            if kind == kinds::SEQUENCE {
                // `definition` would try GET_DDL if this failed.
                let Ok(rows) = self.text_rows(&sequences_sql(&is), &[]).await else { return Ok(None) };
                for r in rows {
                    let (Some(schema), Some(name)) = (r[0].clone(), r[1].clone()) else { continue };
                    let ddl = sequence_ddl(&schema, &name, &r[2..]);
                    sources.insert((schema, name), Some(ddl));
                }
            } else {
                let (sql, arg) = routines_sql(&is, kind, q);
                let args: Vec<&str> = arg.as_deref().into_iter().collect();
                // `list_objects` leaves them out if this view can't be read.
                let Ok(rows) = self.text_rows(&sql, &args).await else { return Ok(None) };
                let mut non_null: BTreeMap<(String, String), usize> = BTreeMap::new();
                for r in rows {
                    let (Some(schema), Some(name)) = (r[0].clone(), r[1].clone()) else { continue };
                    let key = (schema, name);
                    let source = sources.entry(key.clone()).or_default();
                    if r[2].as_deref() == Some("1") {
                        *non_null.entry(key).or_default() += 1;
                        if let Some(text) = r[3].clone() {
                            *source = Some(text);
                        }
                    }
                }
                if non_null.values().any(|n| *n > 1) {
                    return Ok(None);
                }
            }
            for ((schema, name), source) in &sources {
                report.scanned += 1;
                let Some(source) = source else { continue };
                report.hits.extend(hits_in(kind, Some(schema), name, None, source, q));
                if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                    report.hits.truncate(q.max_hits);
                    report.truncated = true;
                    return Ok(Some(report));
                }
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_holding_dollar_quotes_are_quoted_as_literals() {
        for k in [kinds::FUNCTION, kinds::PROCEDURE] {
            let e = routine_source(k).expr;
            assert!(e.contains("CASE WHEN CONTAINS("), "{e}");
            assert!(e.contains(r"'\\', '\\\\'"), "{e}");
            assert!(e.contains("'''', ''''''"), "{e}");
        }
    }

    fn q(text: &str, case: bool, kinds: &[&str]) -> CodeSearch {
        CodeSearch { text: text.into(), case_sensitive: case, kinds: kinds.iter().map(|k| k.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn only_kinds_with_a_catalog_source() {
        assert_eq!(plan(&q("x", false, &["procedure", "function"])), Some(vec!["function", "procedure"]));
        assert_eq!(plan(&q("x", false, &["sequence"])), Some(vec!["sequence"]));
        assert_eq!(plan(&q("x", false, &[])), None, "every kind: tables need GET_DDL");
        assert_eq!(plan(&q("x", false, &["function", "view"])), None);
        assert_eq!(plan(&q("", false, &["function"])), None);
    }

    #[test]
    fn text_goes_as_a_bind() {
        let (sql, arg) = routines_sql("\"DB\".INFORMATION_SCHEMA", kinds::FUNCTION, &q("100%_x'", true, &["function"]));
        assert!(sql.contains("IFF(CONTAINS('CREATE OR REPLACE FUNCTION ' || function_name"), "{sql}");
        assert!(sql.ends_with("FROM \"DB\".INFORMATION_SCHEMA.FUNCTIONS WHERE function_schema <> 'INFORMATION_SCHEMA'"), "{sql}");
        assert!(!sql.contains("100%"), "never in the SQL");
        assert_eq!(arg.as_deref(), Some("100%_x'"), "as is: CONTAINS has no wildcards");
        let (sql, arg) = routines_sql("I", kinds::PROCEDURE, &q("Ventas", false, &["procedure"]));
        assert!(sql.contains("IFF(CONTAINS(LOWER('CREATE OR REPLACE PROCEDURE "), "{sql}");
        assert_eq!(arg.as_deref(), Some("ventas"));
        let (sql, arg) = routines_sql("I", kinds::PROCEDURE, &q("Año", false, &["procedure"]));
        assert!(sql.contains("IFF(TRUE, ") && arg.is_none(), "{sql}");
    }

    #[test]
    fn sequences_as_definition_builds_them() {
        let r = [Some("10".to_string()), Some("5".to_string()), Some("NO".to_string()), None];
        let src = sequence_ddl("APP", "SEQ_FOLIO", &r);
        assert!(src.starts_with("CREATE OR REPLACE SEQUENCE ") && src.contains("START WITH 10 INCREMENT BY 5"), "{src}");
        assert!(sequences_sql("I").starts_with(&format!("SELECT sequence_schema, sequence_name, {SEQUENCE_COLS} FROM I.SEQUENCES")));
    }
}

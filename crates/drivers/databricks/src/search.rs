//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! the functions' `routine_definition` (the catalog's
//! `information_schema.routines`, the text `definition` returns), matched
//! line by line with the contract's rule, so the hits equal those of the
//! app's per-object scan.
//!
//! Tables, views and materialized views come from `SHOW CREATE TABLE`, one
//! statement per object: a search that includes them (or every kind)
//! answers `None` and the app scans.

use crate::DatabricksSession;
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, Result};
use std::collections::BTreeMap;

/// The server-side test for a definition that may hold the text, and its
/// bound value (`instr`: no wildcards to escape). A case-insensitive
/// search narrows on the server only for ASCII text, where `lower` folds
/// as Rust does; otherwise every definition comes back.
fn narrow(q: &CodeSearch) -> (&'static str, Option<String>) {
    if q.case_sensitive {
        ("instr(routine_definition, :p0) > 0", Some(q.text.clone()))
    } else if q.text.is_ascii() {
        ("instr(lower(routine_definition), :p0) > 0", Some(q.text.to_ascii_lowercase()))
    } else {
        ("true", None)
    }
}

/// `list_objects`' functions, with their definition where it may hold the text.
fn functions_sql(catalog: &str, q: &CodeSearch) -> (String, Option<String>) {
    let (cond, arg) = narrow(q);
    let sql = format!(
        "SELECT routine_schema, routine_name, IF({cond}, routine_definition, NULL) FROM {}.information_schema.routines
         WHERE routine_schema <> 'information_schema'",
        quote_ident(Quote::Backtick, catalog)
    );
    (sql, arg)
}

/// Only functions have a catalog source here.
fn covered(q: &CodeSearch) -> bool {
    !q.text.is_empty() && !q.kinds.is_empty() && q.kinds.iter().all(|k| k == kinds::FUNCTION)
}

impl DatabricksSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        if !covered(q) {
            return Ok(None);
        }
        let Ok(cat) = self.catalog().map(str::to_string) else { return Ok(None) };
        let (sql, arg) = functions_sql(&cat, q);
        let args: Vec<&str> = arg.as_deref().into_iter().collect();
        // `list_objects` leaves functions out if this view can't be read.
        let Ok(rows) = self.text_rows(&sql, &args).await else { return Ok(None) };
        let mut sources: BTreeMap<(String, String), Vec<Option<String>>> = BTreeMap::new();
        for mut r in rows {
            let (Some(schema), Some(name)) = (r[0].clone(), r[1].clone()) else { continue };
            sources.entry((schema, name)).or_default().push(r.swap_remove(2));
        }
        // `definition` reads the first row of a name: with two, which one
        // isn't fixed.
        if sources.values().any(|s| s.len() > 1) {
            return Ok(None);
        }
        let mut report = CodeSearchReport { scanned: sources.len(), ..Default::default() };
        for ((schema, name), source) in &sources {
            let Some(Some(source)) = source.first() else { continue };
            report.hits.extend(hits_in(kinds::FUNCTION, Some(schema), name, None, source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                break;
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, case: bool, kinds: &[&str]) -> CodeSearch {
        CodeSearch { text: text.into(), case_sensitive: case, kinds: kinds.iter().map(|k| k.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn only_functions() {
        assert!(covered(&q("x", false, &["function"])));
        assert!(!covered(&q("x", false, &[])), "every kind: tables need SHOW CREATE TABLE");
        assert!(!covered(&q("x", false, &["function", "view"])));
        assert!(!covered(&q("", false, &["function"])));
    }

    #[test]
    fn text_goes_as_a_parameter() {
        let (sql, arg) = functions_sql("main`x", &q("100%_x'", true, &["function"]));
        assert!(sql.starts_with("SELECT routine_schema, routine_name, IF(instr(routine_definition, :p0) > 0, routine_definition, NULL)"), "{sql}");
        assert!(sql.contains("FROM `main``x`.information_schema.routines"), "{sql}");
        assert!(!sql.contains("100%"), "never in the SQL");
        assert_eq!(arg.as_deref(), Some("100%_x'"), "as is: instr has no wildcards");
        let (sql, arg) = functions_sql("c", &q("Ventas", false, &["function"]));
        assert!(sql.contains("IF(instr(lower(routine_definition), :p0) > 0"), "{sql}");
        assert_eq!(arg.as_deref(), Some("ventas"));
        let (sql, arg) = functions_sql("c", &q("Año", false, &["function"]));
        assert!(sql.contains("IF(true, routine_definition, NULL)") && arg.is_none(), "{sql}");
    }
}

//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! the `DEFINITION` of the schema's views, procedures, functions and
//! triggers, one query per kind, built into the text `definition` returns
//! and matched line by line with the contract's rule, so the hits equal
//! those of the app's per-object scan.
//!
//! Tables and sequences (`GET_OBJECT_DEFINITION`, one call per object),
//! synonyms and table types are left to the scan: a search that includes
//! them answers `None`. The sources aren't narrowed on the server (they're
//! NCLOBs); they come back whole and the lines decide.

use crate::{text, view_ddl, HanaSession};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, Result};
use std::collections::HashSet;

/// Per kind, the same objects as `list_objects` (name, trigger's table)
/// and the source `definition` reads, for the session's schema.
fn source_sql(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::VIEW => "SELECT VIEW_NAME, NULL, DEFINITION FROM SYS.VIEWS WHERE SCHEMA_NAME = ? ORDER BY 1",
        kinds::PROCEDURE => "SELECT PROCEDURE_NAME, NULL, DEFINITION FROM SYS.PROCEDURES WHERE SCHEMA_NAME = ? ORDER BY 1",
        kinds::FUNCTION => "SELECT FUNCTION_NAME, NULL, DEFINITION FROM SYS.FUNCTIONS WHERE SCHEMA_NAME = ? ORDER BY 1",
        kinds::TRIGGER => "SELECT TRIGGER_NAME, SUBJECT_TABLE_NAME, DEFINITION FROM SYS.TRIGGERS WHERE SCHEMA_NAME = ? ORDER BY 1",
        _ => return None,
    })
}

/// The kinds to read, in order; `None` when one of them (or every kind,
/// for an empty filter: tables among them) has no catalog query here.
fn plan(q: &CodeSearch) -> Option<Vec<&str>> {
    if q.text.is_empty() || q.kinds.is_empty() {
        return None;
    }
    q.kinds.iter().map(|k| source_sql(k).map(|_| k.as_str())).collect()
}

impl HanaSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        let Some(plan) = plan(q) else { return Ok(None) };
        let schema = self.schema.clone();
        let mut report = CodeSearchReport::default();
        for kind in plan {
            let Some(sql) = source_sql(kind) else { continue };
            let rows = self.rows(sql, &[&schema]).await?;
            // `definition` takes the first row of a name: two rows of one
            // name (not expected) would make the hits differ.
            let mut seen = HashSet::new();
            let mut objects = Vec::new();
            for r in &rows {
                let Some(name) = r.first().and_then(text) else { continue };
                if !seen.insert(name.clone()) {
                    return Ok(None);
                }
                let parent = if kind == kinds::TRIGGER { r.get(1).and_then(text) } else { None };
                let source = r.get(2).and_then(text).map(|d| if kind == kinds::VIEW { view_ddl(&schema, &name, &d) } else { d });
                objects.push((name, parent, source));
            }
            for (name, parent, source) in objects {
                report.scanned += 1;
                let Some(source) = source else { continue };
                report.hits.extend(hits_in(kind, None, &name, parent.as_deref(), &source, q));
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

    fn q(kinds: &[&str]) -> CodeSearch {
        CodeSearch { text: "ventas".into(), kinds: kinds.iter().map(|k| k.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn only_kinds_with_a_catalog_source() {
        assert_eq!(plan(&q(&["view", "trigger"])), Some(vec!["view", "trigger"]));
        assert_eq!(plan(&q(&[])), None, "every kind: tables need GET_OBJECT_DEFINITION");
        assert_eq!(plan(&q(&["view", "table"])), None);
        assert_eq!(plan(&q(&["sequence"])), None);
        assert_eq!(plan(&CodeSearch { text: String::new(), ..q(&["view"]) }), None);
    }

    #[test]
    fn the_schema_is_a_parameter() {
        for k in [kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER] {
            let sql = source_sql(k).unwrap();
            assert!(sql.contains("SCHEMA_NAME = ?") && !sql.contains("LIKE"), "{sql}");
        }
    }

    #[test]
    fn views_as_definition_builds_them() {
        let src = view_ddl("APP", "V_VENTAS", "  SELECT ID\nFROM VENTAS \n");
        assert_eq!(src, "CREATE VIEW \"APP\".\"V_VENTAS\" AS\nSELECT ID\nFROM VENTAS");
        let h = hits_in(kinds::VIEW, None, "V_VENTAS", None, &src, &q(&["view"]));
        assert_eq!(h.iter().map(|h| h.line).collect::<Vec<_>>(), vec![1, 3]);
    }
}

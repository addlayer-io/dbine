//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! `create_table_query` of the database's tables, views, materialized views,
//! dictionaries and streams (`system.tables`) and `create_query` of the SQL
//! functions (`system.functions`), the same texts `definition` returns,
//! narrowed on the server and matched line by line with the contract's rule,
//! so the hits equal those of the app's per-object scan.

use crate::{kind_of, text, ClickHouseSession};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, Result};

/// The server-side filter on `col` and its parameter: `position` (no
/// wildcards to escape). A case-insensitive search narrows on the server
/// only for ASCII text, where the server's folding is Rust's; otherwise
/// every source comes back and the lines decide.
fn narrow(col: &str, q: &CodeSearch) -> (String, Option<String>) {
    if q.case_sensitive {
        (format!("position({col}, {{t:String}}) > 0"), Some(q.text.clone()))
    } else if q.text.is_ascii() {
        (format!("positionCaseInsensitive({col}, {{t:String}}) > 0"), Some(q.text.clone()))
    } else {
        ("1".into(), None)
    }
}

/// `list_objects`'s tables, with their source where it may hold the text.
fn tables_sql(q: &CodeSearch) -> (String, Option<String>) {
    let (cond, param) = narrow("create_table_query", q);
    (
        format!(
            "SELECT name, engine, create_table_query FROM system.tables
             WHERE database = {{db:String}} AND NOT is_temporary AND name NOT LIKE '.inner%' AND {cond}
             ORDER BY name"
        ),
        param,
    )
}

/// `list_objects`'s SQL functions (global, not per database).
fn functions_sql(q: &CodeSearch) -> (String, Option<String>) {
    let (cond, param) = narrow("create_query", q);
    (format!("SELECT name, create_query FROM system.functions WHERE origin = 'SQLUserDefined' AND {cond} ORDER BY name"), param)
}

impl ClickHouseSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        if q.text.is_empty() {
            return Ok(None);
        }
        let wanted = |kind: &str| q.kinds.is_empty() || q.kinds.iter().any(|k| k == kind);
        let mut report = CodeSearchReport::default();
        let push = |report: &mut CodeSearchReport, kind: &str, name: &str, source: &str| {
            report.hits.extend(hits_in(kind, None, name, None, source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
            }
        };

        let (sql, t) = tables_sql(q);
        let db = self.database.clone();
        let mut params = vec![("db", db.as_str())];
        if let Some(t) = t.as_deref() {
            params.push(("t", t));
        }
        let rows = self.rows(&sql, &params).await?;
        for r in &rows {
            let kind = kind_of(&text(&r[1]), self.flavor);
            if !wanted(kind) {
                continue;
            }
            report.scanned += 1;
            push(&mut report, kind, &text(&r[0]), &text(&r[2]));
            if report.truncated {
                return Ok(Some(report));
            }
        }

        if wanted(kinds::FUNCTION) {
            let (sql, t) = functions_sql(q);
            let params: Vec<(&str, &str)> = t.as_deref().map(|t| ("t", t)).into_iter().collect();
            // A server whose `system.functions` can't be read this way: the
            // scan reads each function on its own.
            let Ok(rows) = self.rows(&sql, &params).await else { return Ok(None) };
            for r in &rows {
                report.scanned += 1;
                push(&mut report, kinds::FUNCTION, &text(&r[0]), &text(&r[1]));
                if report.truncated {
                    break;
                }
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, case: bool) -> CodeSearch {
        CodeSearch { text: text.into(), case_sensitive: case, ..Default::default() }
    }

    #[test]
    fn text_goes_as_a_parameter() {
        let (sql, p) = tables_sql(&q("100%_x'", true));
        assert!(sql.contains("position(create_table_query, {t:String}) > 0"), "{sql}");
        assert!(!sql.contains("100%"), "never in the SQL");
        assert_eq!(p.as_deref(), Some("100%_x'"), "as is: position has no wildcards");
        assert!(sql.contains("name NOT LIKE '.inner%'"), "the same objects as list_objects");
    }

    #[test]
    fn case_insensitive_narrows_only_ascii() {
        let (sql, p) = functions_sql(&q("Ventas", false));
        assert!(sql.contains("positionCaseInsensitive(create_query, {t:String}) > 0"), "{sql}");
        assert_eq!(p.as_deref(), Some("Ventas"));
        let (sql, p) = tables_sql(&q("Año", false));
        assert!(sql.contains("AND 1\n"), "{sql}");
        assert!(p.is_none());
        let (_, p) = tables_sql(&q("Año", true));
        assert_eq!(p.as_deref(), Some("Año"), "byte for byte when case matters");
    }
}

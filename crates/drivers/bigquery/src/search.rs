//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! the `ddl` of the dataset's tables, views and materialized views
//! (`INFORMATION_SCHEMA.TABLES`) and routines (`INFORMATION_SCHEMA.ROUTINES`),
//! the text `definition` returns, matched line by line with the contract's
//! rule, so the hits equal those of the app's per-object scan.
//!
//! The objects are `list_objects`' own. `definition` falls back to the
//! REST resource when INFORMATION_SCHEMA has no `ddl` for an object (or
//! can't be read, as on the emulator): then this answers `None` and the
//! scan reads them one by one.

use crate::BigQuerySession;
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, Result};
use serde_json::{json, Value as Json};
use std::collections::HashMap;

/// The server-side test for a `ddl` that may hold the text, and the value
/// of its `@t` parameter (`STRPOS`: no wildcards to escape). A
/// case-insensitive search narrows on the server only for ASCII text,
/// where `LOWER` folds as Rust does; otherwise every `ddl` comes back.
fn narrow(q: &CodeSearch) -> (&'static str, Option<String>) {
    if q.case_sensitive {
        ("STRPOS(ddl, @t) > 0", Some(q.text.clone()))
    } else if q.text.is_ascii() {
        ("STRPOS(LOWER(ddl), @t) > 0", Some(q.text.to_ascii_lowercase()))
    } else {
        ("TRUE", None)
    }
}

/// Every object of an INFORMATION_SCHEMA view (`TABLES` or `ROUTINES`):
/// its name, whether it has no `ddl`, and the `ddl` where it may hold the text.
fn source_sql(project: &str, dataset: &str, view: &str, name_col: &str, q: &CodeSearch) -> (String, Option<Json>) {
    let (cond, t) = narrow(q);
    let sql = format!(
        "SELECT {name_col}, ddl IS NULL, IF({cond}, ddl, NULL) FROM {}.{}.INFORMATION_SCHEMA.{view}",
        quote_ident(Quote::Backtick, project),
        quote_ident(Quote::Backtick, dataset)
    );
    let params = t.map(|t| json!([{ "name": "t", "parameterType": { "type": "STRING" }, "parameterValue": { "value": t } }]));
    (sql, params)
}

/// Name → (no `ddl`, `ddl` if it may match), one entry per row.
type Sources = HashMap<String, Vec<(bool, Option<String>)>>;

fn is_routine(kind: &str) -> bool {
    kind == kinds::FUNCTION || kind == kinds::PROCEDURE
}

impl BigQuerySession {
    async fn sources(&self, dataset: &str, view: &str, name_col: &str, q: &CodeSearch) -> Option<Sources> {
        let (sql, params) = source_sql(&self.api.project, dataset, view, name_col, q);
        let r = self.query(&sql, 1_000_000, params).await.ok()?;
        if r.more {
            return None;
        }
        let mut out = Sources::new();
        for row in &r.rows {
            let cell = |i: usize| row.pointer(&format!("/f/{i}/v"));
            let Some(name) = cell(0).and_then(Json::as_str) else { continue };
            let no_ddl = cell(1).and_then(Json::as_str).is_none_or(|b| b.eq_ignore_ascii_case("true"));
            let ddl = cell(2).and_then(Json::as_str).map(str::to_string);
            out.entry(name.to_string()).or_default().push((no_ddl, ddl));
        }
        Some(out)
    }

    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        let Some(ds) = self.dataset.clone() else { return Ok(None) };
        // The emulator has no INFORMATION_SCHEMA.
        if q.text.is_empty() || self.api.emulator {
            return Ok(None);
        }
        let wanted = |kind: &str| q.kinds.is_empty() || q.kinds.iter().any(|k| k == kind);
        let objects: Vec<_> = dbine_driver::Session::list_objects(self).await?.into_iter().filter(|o| wanted(&o.kind)).collect();
        let tables = if objects.iter().any(|o| !is_routine(&o.kind)) {
            let Some(s) = self.sources(&ds, "TABLES", "table_name", q).await else { return Ok(None) };
            s
        } else {
            Sources::new()
        };
        let routines = if objects.iter().any(|o| is_routine(&o.kind)) {
            let Some(s) = self.sources(&ds, "ROUTINES", "routine_name", q).await else { return Ok(None) };
            s
        } else {
            Sources::new()
        };
        let mut report = CodeSearchReport::default();
        for o in &objects {
            let from = if is_routine(&o.kind) { &routines } else { &tables };
            // `definition` reads the REST resource for an object without a
            // `ddl` here; and the first of two rows of a name is anyone's.
            let source = match from.get(&o.name).map(Vec::as_slice) {
                Some([(false, ddl)]) => ddl.as_deref(),
                _ => return Ok(None),
            };
            report.scanned += 1;
            let Some(source) = source else { continue };
            report.hits.extend(hits_in(&o.kind, None, &o.name, None, source, q));
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

    fn q(text: &str, case: bool) -> CodeSearch {
        CodeSearch { text: text.into(), case_sensitive: case, ..Default::default() }
    }

    #[test]
    fn text_goes_as_a_parameter() {
        let (sql, p) = source_sql("my-proj", "ds`1", "TABLES", "table_name", &q("100%_x'", true));
        assert_eq!(
            sql,
            "SELECT table_name, ddl IS NULL, IF(STRPOS(ddl, @t) > 0, ddl, NULL) FROM `my-proj`.`ds``1`.INFORMATION_SCHEMA.TABLES"
        );
        assert_eq!(p.unwrap().pointer("/0/parameterValue/value"), Some(&json!("100%_x'")));
    }

    #[test]
    fn case_insensitive_narrows_only_ascii() {
        let (sql, p) = source_sql("p", "d", "ROUTINES", "routine_name", &q("Ventas", false));
        assert!(sql.contains("IF(STRPOS(LOWER(ddl), @t) > 0, ddl, NULL)"), "{sql}");
        assert_eq!(p.unwrap().pointer("/0/parameterValue/value"), Some(&json!("ventas")));
        let (sql, p) = source_sql("p", "d", "ROUTINES", "routine_name", &q("Año", false));
        assert!(sql.contains("IF(TRUE, ddl, NULL)") && p.is_none(), "{sql}");
    }
}

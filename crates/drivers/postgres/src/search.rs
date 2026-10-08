//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]).
//!
//! The objects are the explorer's (`list_objects`), and each one's source
//! is the text `definition` gives it, read in bulk: views and materialized
//! views (`pg_get_viewdef` behind the same `CREATE … AS` head), routines
//! (`pg_get_functiondef`, every overload of a name joined in `oid` order)
//! and triggers (`pg_get_triggerdef`, every table's trigger of a name
//! joined by table), one query per kind, narrowed on the server with
//! LIKE / ILIKE and matched line by line with the contract's rule. So the
//! hits equal those of the app's per-object scan.
//!
//! Sequences, types and synonyms are rebuilt from several catalogs
//! (`compare_definition`): they're read one by one here, as the scan would.
//! A variant whose sources only come one at a time (`SHOW CREATE`: the
//! tables and views of CockroachDB, the streaming engines, CrateDB, H2;
//! Redshift; Denodo) leaves the search to the app's scan.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::{compare, Variant};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, ObjectRef, Result, Session};
use std::collections::HashMap;

/// `LIKE` pattern for `text`, its wildcards escaped with `!`.
fn like(text: &str) -> String {
    let mut p = String::from("%");
    for c in text.chars() {
        if matches!(c, '%' | '_' | '!') {
            p.push('!');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// What a source's bulk query is keyed by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    View,
    Routine,
    Trigger,
}

impl PgSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        let v = self.variant;
        if q.text.is_empty()
            || matches!(
                v,
                Variant::Redshift | Variant::Denodo | Variant::RisingWave | Variant::Materialize | Variant::CrateDb | Variant::H2
            )
        {
            return Ok(None);
        }
        let info = v.info();
        let wanted: Vec<&str> = info
            .object_kinds
            .iter()
            .filter(|k| k.has_definition && (q.kinds.is_empty() || q.kinds.iter().any(|w| w == k.id)))
            .map(|k| k.id)
            .collect();
        // CockroachDB's tables and views come from SHOW CREATE, one by one.
        if v == Variant::Cockroach && wanted.iter().any(|k| matches!(*k, kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW)) {
            return Ok(None);
        }
        let objects: Vec<_> = Session::list_objects(self).await?.into_iter().filter(|o| wanted.contains(&o.kind.as_str())).collect();
        let has = |ks: &[&str]| objects.iter().any(|o| ks.contains(&o.kind.as_str()));

        let mut sources: HashMap<(Group, String, String), Vec<String>> = HashMap::new();
        let mut bulk = Vec::new();
        if has(&[kinds::VIEW, kinds::MATERIALIZED_VIEW]) {
            bulk.push((Group::View, self.view_sources(q)));
        }
        if has(&[kinds::PROCEDURE, kinds::FUNCTION]) {
            bulk.push((Group::Routine, self.routine_sources(q)));
        }
        if has(&[kinds::TRIGGER]) {
            bulk.push((Group::Trigger, self.trigger_sources(q)));
        }
        for (group, sql) in bulk {
            let rows = match self.text(&sql).await {
                Ok(r) => r,
                Err(e) => {
                    // The scan reads them one by one (and says which fail).
                    tracing::debug!("{v:?}: bulk sources unavailable: {e}");
                    return Ok(None);
                }
            };
            let views = group == Group::View;
            for r in &rows {
                let (Some(sch), Some(name)) = (cell(r, "sch"), cell(r, "name")) else { continue };
                let Some(def) = cell(r, "def") else { continue };
                let def = if views {
                    // As `pg_view_definition` puts it.
                    let q = qualified_name(Quote::Double, Some(&sch), &name);
                    match cell(r, "rk").as_deref() {
                        Some("m") => format!("CREATE MATERIALIZED VIEW {q} AS\n{def}"),
                        _ => format!("CREATE OR REPLACE VIEW {q} AS\n{def}"),
                    }
                } else {
                    def
                };
                sources.entry((group, sch, name)).or_default().push(def);
            }
        }

        let mut report = CodeSearchReport { scanned: objects.len(), ..Default::default() };
        for o in &objects {
            let schema = o.schema.clone().unwrap_or_default();
            let group = match o.kind.as_str() {
                kinds::VIEW | kinds::MATERIALIZED_VIEW => Some(Group::View),
                kinds::PROCEDURE | kinds::FUNCTION => Some(Group::Routine),
                kinds::TRIGGER => Some(Group::Trigger),
                _ => None,
            };
            let source = match group {
                Some(g) => sources.get(&(g, schema, o.name.clone())).map(|parts| parts.join("\n\n")),
                None if compare::KINDS.contains(&o.kind.as_str()) => {
                    let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
                    match self.compare_definition(&obj).await {
                        Ok(s) => s,
                        Err(_) => {
                            report.unreadable.push(format!("{}.{}", o.schema.as_deref().unwrap_or_default(), o.name));
                            continue;
                        }
                    }
                }
                // Tables: PostgreSQL has no CREATE TABLE of its own.
                None => None,
            };
            let Some(source) = source else { continue };
            report.hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                break;
            }
        }
        Ok(Some(report))
    }

    /// `expr` may hold `q`'s text: LIKE when the case matters, ILIKE when
    /// not (only for ASCII text: the server's case folding may not be
    /// Rust's for the rest, and then nothing is narrowed).
    fn narrow(&self, expr: &str, q: &CodeSearch) -> String {
        let pattern = lit(self.variant, &like(&q.text));
        if q.case_sensitive {
            format!("{expr} LIKE {pattern} ESCAPE '!'")
        } else if q.text.is_ascii() {
            format!("{expr} ILIKE {pattern} ESCAPE '!'")
        } else {
            "true".into()
        }
    }

    /// Views and materialized views: `pg_view_definition` in bulk. The
    /// head it adds is matched too.
    fn view_sources(&self, q: &CodeSearch) -> String {
        let head = "CASE WHEN rk = 'm' THEN 'CREATE MATERIALIZED VIEW ' ELSE 'CREATE OR REPLACE VIEW ' END
                    || '\"' || replace(sch, '\"', '\"\"') || '\".\"' || replace(name, '\"', '\"\"') || '\" AS'";
        format!(
            "SELECT sch, name, rk, def FROM (
               SELECT n.nspname::text AS sch, c.relname::text AS name, c.relkind::text AS rk, pg_get_viewdef(c.oid, true) AS def
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE c.relkind IN ('v', 'm') AND {filter}) x
             WHERE {head_match} OR {def_match}",
            filter = self.filter("n.nspname"),
            head_match = self.narrow(&format!("({head})"), q),
            def_match = self.narrow("def", q),
        )
    }

    /// Functions and procedures: `pg_routine_definition` in bulk, every
    /// overload of a name that has the text (its line numbers count the
    /// overloads before it).
    fn routine_sources(&self, q: &CodeSearch) -> String {
        let def = if self.variant == Variant::OpenGauss { "(pg_get_functiondef(p.oid)).definition" } else { "pg_get_functiondef(p.oid)" };
        format!(
            "WITH r AS (
               SELECT p.oid AS id, n.nspname::text AS sch, p.proname::text AS name, {def} AS def
               FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
               WHERE {routines} AND {filter})
             SELECT sch, name, def FROM r
             WHERE (sch, name) IN (SELECT sch, name FROM r WHERE {matches})
             ORDER BY id",
            routines = self.routine_filter(),
            filter = self.filter("n.nspname"),
            matches = self.narrow("def", q),
        )
    }

    /// Triggers: `pg_trigger_definition` in bulk, every table's trigger of
    /// a name that has the text, by table.
    fn trigger_sources(&self, q: &CodeSearch) -> String {
        format!(
            "WITH t AS (
               SELECT n.nspname::text AS sch, t.tgname::text AS name, c.relname AS rel, pg_get_triggerdef(t.oid, true) AS def
               FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE NOT t.tgisinternal AND {filter})
             SELECT sch, name, def FROM t
             WHERE (sch, name) IN (SELECT sch, name FROM t WHERE {matches})
             ORDER BY sch, name, rel",
            filter = self.filter("n.nspname"),
            matches = self.narrow("def", q),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like("100%_x!"), "%100!%!_x!!%");
    }
}

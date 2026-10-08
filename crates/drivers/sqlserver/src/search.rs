//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! every module's source (`sys.sql_modules`: views, routines, triggers) in
//! one query, narrowed on the server with LIKE and matched line by line
//! with the contract's rule. The other kinds asked for (sequences,
//! synonyms, types, full-text catalogs…, usually few) are read one by one
//! through `definition`, so the hits equal those of the app's scan.

use crate::variant::Variant;
use crate::{text, SqlServerSession};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, ObjectRef, Result, Session};

/// What `sys.sql_modules` holds.
const MODULES: [&str; 4] = [kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER];

/// `LIKE` pattern for `text` (its wildcards escaped with `\`).
fn like(text: &str) -> String {
    let mut p = String::from("%");
    for c in text.chars() {
        if matches!(c, '%' | '_' | '[' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

impl SqlServerSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        // Fabric's warehouse and Babelfish keep their sources elsewhere:
        // the app's generic scan reads them through `definition`.
        if !matches!(self.variant, Variant::SqlServer | Variant::AzureSql) || q.text.is_empty() {
            return Ok(None);
        }
        let rows = self
            .rows(
                "SELECT RTRIM(o.type), s.name, o.name, OBJECT_NAME(o.parent_object_id), m.definition
                 FROM sys.sql_modules m
                 JOIN sys.objects o ON o.object_id = m.object_id
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 WHERE o.is_ms_shipped = 0 AND m.definition LIKE @P1 ESCAPE '\\'
                 ORDER BY s.name, o.name",
                &[&like(&q.text)],
            )
            .await?;
        let wants = |kind: &str| q.kinds.is_empty() || q.kinds.iter().any(|k| k == kind);
        let mut report = CodeSearchReport { scanned: rows.len(), ..Default::default() };
        let full = |report: &mut CodeSearchReport| {
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                true
            } else {
                false
            }
        };
        for r in &rows {
            let kind = match text(r, 0).as_deref() {
                Some("V") => kinds::VIEW,
                Some("P" | "PC") => kinds::PROCEDURE,
                Some("TR") => kinds::TRIGGER,
                _ => kinds::FUNCTION,
            };
            if !q.kinds.is_empty() && !q.kinds.iter().any(|k| k == kind) {
                continue;
            }
            let (Some(name), Some(source)) = (text(r, 2), text(r, 4)) else { continue };
            let parent = if kind == kinds::TRIGGER { text(r, 3) } else { None };
            report.hits.extend(hits_in(kind, text(r, 1).as_deref(), &name, parent.as_deref(), &source, q));
            if full(&mut report) {
                return Ok(Some(report));
            }
        }
        // The rest of the kinds asked for, one object at a time.
        let others: Vec<String> = q.kinds.iter().filter(|k| !MODULES.contains(&k.as_str())).cloned().collect();
        if others.is_empty() && !q.kinds.is_empty() {
            return Ok(Some(report));
        }
        for o in self.list_objects().await? {
            if MODULES.contains(&o.kind.as_str()) || !wants(&o.kind) || (q.kinds.is_empty() && o.kind == kinds::TABLE) {
                continue;
            }
            let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            match self.definition(&obj).await {
                Ok(Some(source)) => {
                    report.scanned += 1;
                    report.hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &source, q));
                }
                Ok(None) => report.scanned += 1,
                Err(_) => report.unreadable.push(o.name.clone()),
            }
            if full(&mut report) {
                break;
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like("a_b%c[d]\\"), "%a\\_b\\%c\\[d]\\\\%");
    }
}

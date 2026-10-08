//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]):
//! every module's source (`sys.sql_modules`) in one query, narrowed on the
//! server with LIKE and matched line by line with the contract's rule, so
//! the hits equal those of the app's per-object scan.

use crate::variant::Variant;
use crate::{text, SqlServerSession};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, Result};

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
        let mut report = CodeSearchReport { scanned: rows.len(), ..Default::default() };
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

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like("a_b%c[d]\\"), "%a\\_b\\%c\\[d]\\\\%");
    }
}

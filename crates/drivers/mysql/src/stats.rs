//! What the catalog already knows about the session database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Rows: `information_schema.TABLES.TABLE_ROWS`, the storage engine's own
//! estimate (InnoDB's statistics, TiDB's and OceanBase's statistics
//! module, the tablet reports of StarRocks and Doris), then Databend's
//! `system.tables.num_rows` (snapshot metadata). Manticore has no
//! `information_schema`: `SHOW TABLE … STATUS` (`indexed_documents`), one
//! table at a time. Never a `COUNT(*)`.
//!
//! Comments: `ROUTINES.ROUTINE_COMMENT` of procedures and functions;
//! `TABLE_COMMENT` of MariaDB's sequences and of the views (and StarRocks'
//! materialized views) where the engine keeps one (MySQL writes `VIEW`
//! there, which isn't a comment). Triggers have no comments.
//!
//! Each query is its own: one that fails leaves fewer results, not an
//! error.

use crate::session::{at, lit, table_kind, MySqlSession};
use crate::Variant;
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use std::collections::HashSet;

/// Manticore tables whose status is read at most.
const MAX_STATUS_READS: usize = 500;

/// A catalog count (`1234`, `1.2e6`); `None` when unknown or negative.
fn parse_rows(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let f = s.parse::<f64>().ok()?;
    (f.is_finite() && f >= 0.0).then(|| f.round() as u64)
}

/// A name Manticore takes unquoted in `SHOW TABLE … STATUS`.
fn plain_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A table comment that is one: MySQL writes `VIEW` for every view.
fn real_comment(c: &str) -> bool {
    let c = c.trim();
    !c.is_empty() && !c.eq_ignore_ascii_case("VIEW")
}

/// The kind a commented non-table relation is listed as; `None`: none
/// the explorer lists.
fn commented_kind(table_type: &str, is_mv: bool, has_sequences: bool) -> Option<&'static str> {
    let t = table_type.to_ascii_uppercase();
    if is_mv {
        Some(kinds::MATERIALIZED_VIEW)
    } else if t == "SEQUENCE" {
        has_sequences.then_some(kinds::SEQUENCE)
    } else if t == "VIEW" {
        Some(kinds::VIEW)
    } else {
        None
    }
}

fn object(kind: &str, name: String) -> ObjectRef {
    ObjectRef { kind: kind.to_string(), schema: None, name }
}

impl MySqlSession {
    /// StarRocks' materialized views, which `TABLES` shows as tables.
    async fn materialized_views(&mut self, dbl: &str) -> HashSet<String> {
        if self.variant != Variant::StarRocks {
            return HashSet::new();
        }
        let sql = format!("SELECT TABLE_NAME FROM information_schema.materialized_views WHERE TABLE_SCHEMA = {dbl}");
        self.optional_rows(&sql).await.iter().filter_map(|r| at(r, 0)).collect()
    }

    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        if self.variant == Variant::Manticore {
            return Ok(self.manticore_rows().await);
        }
        let Some(db) = self.current_database().await else { return Ok(Vec::new()) };
        let dbl = lit(&db);
        let mvs = self.materialized_views(&dbl).await;
        let mut queries = vec![format!(
            "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = {dbl} AND TABLE_ROWS IS NOT NULL"
        )];
        if self.variant == Variant::Databend {
            queries.push(format!(
                "SELECT name, 'BASE TABLE', num_rows FROM system.tables WHERE database = {dbl} AND num_rows IS NOT NULL"
            ));
        }
        for sql in queries {
            match self.rows(&sql).await {
                Ok(rows) => {
                    return Ok(rows
                        .iter()
                        .filter_map(|r| {
                            let name = at(r, 0)?;
                            let kind = if mvs.contains(&name) { kinds::MATERIALIZED_VIEW } else { table_kind(&at(r, 1)?)? };
                            // Plain views keep no rows; a value there isn't one.
                            if kind == kinds::VIEW {
                                return None;
                            }
                            Some(RowEstimate { object: object(kind, name), rows: parse_rows(&at(r, 2)?)? })
                        })
                        .collect())
                }
                Err(e) => tracing::debug!("{:?}: row estimates unavailable: {e}", self.variant),
            }
        }
        Ok(Vec::new())
    }

    /// `indexed_documents` of each table's status (Manticore keeps it per
    /// table; nothing is scanned).
    async fn manticore_rows(&mut self) -> Vec<RowEstimate> {
        let tables: Vec<String> = self.optional_rows("SHOW TABLES").await.iter().filter_map(|r| at(r, 0)).collect();
        let mut out = Vec::new();
        for name in tables.into_iter().filter(|n| plain_name(n)).take(MAX_STATUS_READS) {
            let mut rows = self.optional_rows(&format!("SHOW TABLE {name} STATUS")).await;
            if rows.is_empty() {
                rows = self.optional_rows(&format!("SHOW INDEX {name} STATUS")).await;
            }
            let docs = rows
                .iter()
                .find(|r| at(r, 0).as_deref() == Some("indexed_documents"))
                .and_then(|r| at(r, 1))
                .and_then(|v| parse_rows(&v));
            if let Some(rows) = docs {
                out.push(RowEstimate { object: object(kinds::TABLE, name), rows });
            }
        }
        out
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        if self.variant == Variant::Manticore {
            return Ok(Vec::new());
        }
        let Some(db) = self.current_database().await else { return Ok(Vec::new()) };
        let dbl = lit(&db);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |kind: &str, name: Option<String>, comment: Option<String>| {
            let (Some(name), Some(comment)) = (name, comment) else { return };
            if real_comment(&comment) && seen.insert((kind.to_string(), name.clone())) {
                out.push(ObjectComment { object: object(kind, name), comment });
            }
        };
        if self.variant.has_routines() {
            let sql = format!(
                "SELECT ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_COMMENT FROM information_schema.ROUTINES
                 WHERE ROUTINE_SCHEMA = {dbl} AND ROUTINE_COMMENT <> ''"
            );
            for r in self.optional_rows(&sql).await {
                let kind = match at(&r, 1) {
                    Some(t) if t.eq_ignore_ascii_case("PROCEDURE") => kinds::PROCEDURE,
                    Some(_) => kinds::FUNCTION,
                    None => continue,
                };
                push(kind, at(&r, 0), at(&r, 2));
            }
        }
        let mvs = self.materialized_views(&dbl).await;
        let has_sequences = self.variant.has_sequences();
        let sql = format!(
            "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = {dbl} AND TABLE_COMMENT <> ''"
        );
        for r in self.optional_rows(&sql).await {
            let (Some(name), Some(t)) = (at(&r, 0), at(&r, 1)) else { continue };
            if let Some(kind) = commented_kind(&t, mvs.contains(&name), has_sequences) {
                push(kind, Some(name), at(&r, 2));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_parse_as_the_catalogs_write_them() {
        assert_eq!(parse_rows("1234"), Some(1234));
        assert_eq!(parse_rows("1.2e6"), Some(1_200_000));
        assert_eq!(parse_rows("-1"), None);
        assert_eq!(parse_rows("NULL"), None);
    }

    #[test]
    fn mysql_view_marker_is_not_a_comment() {
        assert!(!real_comment("VIEW"));
        assert!(!real_comment("  "));
        assert!(real_comment("Ventas por mes"));
    }

    #[test]
    fn only_plain_names_reach_show_table_status() {
        assert!(plain_name("productos_2024"));
        assert!(!plain_name("a; DROP TABLE b"));
        assert!(!plain_name(""));
    }

    #[test]
    fn commented_relations_take_the_explorer_kinds() {
        assert_eq!(commented_kind("VIEW", false, false), Some(kinds::VIEW));
        assert_eq!(commented_kind("BASE TABLE", true, false), Some(kinds::MATERIALIZED_VIEW));
        assert_eq!(commented_kind("SEQUENCE", false, true), Some(kinds::SEQUENCE));
        assert_eq!(commented_kind("SEQUENCE", false, false), None);
        assert_eq!(commented_kind("BASE TABLE", false, true), None);
        assert_eq!(commented_kind("SYSTEM VIEW", false, true), None);
    }
}

//! What the dictionary already knows about the schema's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Rows are `ALL_TABLES.NUM_ROWS`, the optimizer statistics DBMS_STATS
//! gathered (NULL: never analyzed, left out): no count, no scan, no lock.
//! A materialized view's container table carries them under the view's
//! name. Comments are `ALL_TAB_COMMENTS` (views) and `ALL_MVIEW_COMMENTS`;
//! Oracle keeps no comments on PL/SQL units, sequences or synonyms.
//! Each read is its own query: one that fails degrades to fewer results.

use crate::{err, OracleSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use oracledb::Connection;

/// Analyzed tables and materialized views of `:1`, with `M` when the
/// table is a materialized view's container.
const ROWS: &str = "SELECT t.table_name, t.num_rows,
        CASE WHEN EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name)
             THEN 'M' ELSE 'T' END
   FROM all_tables t
  WHERE t.owner = :1 AND t.num_rows IS NOT NULL
    AND t.dropped = 'NO' AND t.nested = 'NO' AND t.secondary = 'N'
    AND t.table_name NOT LIKE 'BIN$%'";

const VIEW_COMMENTS: &str = "SELECT table_name, comments FROM all_tab_comments
  WHERE owner = :1 AND table_type = 'VIEW' AND comments IS NOT NULL";

const MVIEW_COMMENTS: &str = "SELECT mview_name, comments FROM all_mview_comments
  WHERE owner = :1 AND comments IS NOT NULL";

/// One `ROWS` row as an estimate: a negative count (never seen, but
/// guarded) is left out.
fn estimate(name: String, rows: i64, flag: &str) -> Option<RowEstimate> {
    let kind = if flag == "M" { kinds::MATERIALIZED_VIEW } else { kinds::TABLE };
    (rows >= 0).then(|| RowEstimate { object: ObjectRef { kind: kind.into(), schema: None, name }, rows: rows as u64 })
}

fn row_estimates(c: &Connection, owner: &str) -> Result<Vec<RowEstimate>> {
    let mut out = Vec::new();
    for row in c.query(ROWS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let name: String = row.get(0).map_err(err)?;
        let rows: Option<i64> = row.get(1).map_err(err)?;
        let flag: String = row.get(2).map_err(err)?;
        out.extend(rows.and_then(|n| estimate(name, n, &flag)));
    }
    Ok(out)
}

fn comments(c: &Connection, sql: &str, owner: &str, kind: &str) -> Result<Vec<ObjectComment>> {
    let mut out = Vec::new();
    for row in c.query(sql, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let name: String = row.get(0).map_err(err)?;
        let comment: Option<String> = row.get(1).map_err(err)?;
        if let Some(comment) = comment.filter(|s| !s.trim().is_empty()) {
            out.push(ObjectComment { object: ObjectRef { kind: kind.into(), schema: None, name }, comment });
        }
    }
    Ok(out)
}

impl OracleSession {
    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        let owner = self.schema.clone();
        self.run(move |c| {
            Ok(row_estimates(c, &owner).unwrap_or_else(|e| {
                tracing::debug!("oracle: row estimates not read: {e}");
                Vec::new()
            }))
        })
        .await
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        let owner = self.schema.clone();
        self.run(move |c| {
            let mut out = Vec::new();
            for (sql, kind) in [(VIEW_COMMENTS, kinds::VIEW), (MVIEW_COMMENTS, kinds::MATERIALIZED_VIEW)] {
                match comments(c, sql, &owner, kind) {
                    Ok(v) => out.extend(v),
                    Err(e) => tracing::debug!("oracle: {kind} comments not read: {e}"),
                }
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_tables_are_materialized_views() {
        let t = estimate("CLIENTES".into(), 42, "T").unwrap();
        assert_eq!((t.object.kind.as_str(), t.object.name.as_str(), t.rows), ("table", "CLIENTES", 42));
        assert!(t.object.schema.is_none());
        let m = estimate("MV_VENTAS".into(), 0, "M").unwrap();
        assert_eq!((m.object.kind.as_str(), m.rows), ("materialized_view", 0));
        assert!(estimate("X".into(), -1, "T").is_none());
    }
}

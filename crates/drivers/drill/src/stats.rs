//! What Drill's metastore already knows about the session workspace's
//! tables ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `NUM_ROWS` of `INFORMATION_SCHEMA.TABLES`, which Drill fills
//!   from the Drill Metastore (`metastore.enabled`) once
//!   `ANALYZE TABLE … REFRESH METADATA` has run. Without the metastore it's
//!   NULL and the table is left out; files are never read for a count. A
//!   Drill older than 1.17 has no such column: empty.
//! - Comments: Drill keeps none (views and tables take no `COMMENT`):
//!   [`dbine_driver::Session::object_comments`] stays empty.

use crate::{lit, DrillSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};

/// `[TABLE_NAME, NUM_ROWS]` rows as text; NULL (empty) or negative: none.
pub(crate) fn estimates(schema: &str, rows: &[Vec<String>]) -> Vec<RowEstimate> {
    rows.iter()
        .filter(|r| r.len() == 2)
        .filter_map(|r| {
            let n: i64 = r[1].trim().parse().ok().filter(|n| *n >= 0)?;
            Some(RowEstimate {
                object: ObjectRef { kind: kinds::TABLE.into(), schema: Some(schema.to_string()), name: r[0].clone() },
                rows: n as u64,
            })
        })
        .collect()
}

pub(crate) async fn row_estimates(s: &DrillSession) -> Result<Vec<RowEstimate>> {
    let Ok(schema) = s.schema() else { return Ok(Vec::new()) };
    let rows = s
        .strings(&format!(
            "SELECT TABLE_NAME, NUM_ROWS FROM INFORMATION_SCHEMA.`TABLES`
             WHERE TABLE_SCHEMA = {} AND TABLE_TYPE <> 'VIEW' AND NUM_ROWS IS NOT NULL ORDER BY TABLE_NAME",
            lit(&schema)
        ))
        .await
        .unwrap_or_default();
    Ok(estimates(&schema, &rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_rows() {
        let rows = vec![vec!["ventas".to_string(), "1200".to_string()], vec!["x".into(), "".into()], vec!["y".into(), "-1".into()]];
        let e = estimates("dfs.tmp", &rows);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].object.schema.as_deref(), e[0].object.name.as_str(), e[0].rows), (Some("dfs.tmp"), "ventas", 1200));
    }
}

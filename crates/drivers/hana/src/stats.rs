//! What the catalog already knows about the schema's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Rows are `M_TABLES.RECORD_COUNT`, the count HANA keeps for each row and
//! column table (summed over hosts and partitions); without access to it,
//! `M_CS_TABLES.RECORD_COUNT` (column tables only). Both are monitoring
//! views read from the engine's own bookkeeping: no count, no scan, no lock.
//! Comments are `SYS.VIEWS.COMMENTS` (`COMMENT ON VIEW`); HANA keeps no
//! comments on procedures, functions, triggers or sequences.

use crate::{int, text, HanaSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use hdbconnect_async::HdbValue;

const ROWS: &str = "SELECT m.TABLE_NAME, SUM(m.RECORD_COUNT) FROM SYS.M_TABLES m
  JOIN SYS.TABLES t ON t.SCHEMA_NAME = m.SCHEMA_NAME AND t.TABLE_NAME = m.TABLE_NAME
 WHERE m.SCHEMA_NAME = ? AND t.IS_SYSTEM_TABLE = 'FALSE' AND t.IS_USER_DEFINED_TYPE = 'FALSE'
 GROUP BY m.TABLE_NAME";

const ROWS_CS: &str = "SELECT m.TABLE_NAME, SUM(m.RECORD_COUNT) FROM SYS.M_CS_TABLES m
  JOIN SYS.TABLES t ON t.SCHEMA_NAME = m.SCHEMA_NAME AND t.TABLE_NAME = m.TABLE_NAME
 WHERE m.SCHEMA_NAME = ? AND t.IS_SYSTEM_TABLE = 'FALSE' AND t.IS_USER_DEFINED_TYPE = 'FALSE'
 GROUP BY m.TABLE_NAME";

const VIEW_COMMENTS: &str = "SELECT VIEW_NAME, COMMENTS FROM SYS.VIEWS WHERE SCHEMA_NAME = ? AND COMMENTS IS NOT NULL";

/// `(name, rows)` rows as estimates; a NULL or negative count is left out.
fn estimates(rows: &[Vec<HdbValue<'static>>]) -> Vec<RowEstimate> {
    rows.iter()
        .filter_map(|r| {
            let name = text(r.first()?)?;
            let n = int(r.get(1)?).filter(|n| *n >= 0)?;
            Some(RowEstimate { object: ObjectRef { kind: kinds::TABLE.into(), schema: None, name }, rows: n as u64 })
        })
        .collect()
}

impl HanaSession {
    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        let s = self.schema.clone();
        for sql in [ROWS, ROWS_CS] {
            match self.rows(sql, &[&s]).await {
                Ok(rows) => return Ok(estimates(&rows)),
                Err(e) => tracing::debug!("hana: row estimates not read: {e}"),
            }
        }
        Ok(Vec::new())
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        let s = self.schema.clone();
        let rows = match self.rows(VIEW_COMMENTS, &[&s]).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("hana: view comments not read: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = text(r.first()?)?;
                let comment = text(r.get(1)?)?.trim().to_string();
                (!comment.is_empty()).then(|| ObjectComment { object: ObjectRef { kind: kinds::VIEW.into(), schema: None, name }, comment })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_become_table_estimates() {
        let rows = vec![
            vec![HdbValue::STRING("CLIENTES".into()), HdbValue::BIGINT(250)],
            vec![HdbValue::STRING("VACIA".into()), HdbValue::NULL],
            vec![HdbValue::STRING("PEDIDOS".into()), HdbValue::STRING("12".into())],
        ];
        let e = estimates(&rows);
        let got: Vec<_> = e.iter().map(|e| (e.object.kind.as_str(), e.object.name.as_str(), e.rows)).collect();
        assert_eq!(got, vec![("table", "CLIENTES", 250), ("table", "PEDIDOS", 12)]);
    }
}

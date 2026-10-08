//! What the catalog already knows about the database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Firebird keeps no row count. What it keeps is each index's selectivity
//! (`RDB$INDICES.RDB$STATISTICS`, 1 / distinct keys), computed when the
//! index is built and by `SET STATISTICS`, which the optimizer uses: for a
//! unique index there are as many keys as rows, so `1 / selectivity` is the
//! table's rows at that moment. The primary key's index is preferred (no
//! NULL keys); otherwise the unique index with the most keys. Tables with
//! no unique index, or whose statistics were never computed (0: the index
//! was built on an empty table), are left out: no count, no scan.
//!
//! Comments are `RDB$DESCRIPTION` (`COMMENT ON`) of views, procedures,
//! functions, packages, triggers, sequences and domains, each kind its own
//! query: one that fails (an older server without packages) is skipped.

use crate::{int, text, FirebirdSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use rsfbclient_core::{Column, SqlType};
use std::collections::BTreeMap;

/// Selectivity of the active unique indexes of persistent user tables,
/// with 1 for the primary key's.
const UNIQUE_STATS: &str = "
SELECT TRIM(i.RDB$RELATION_NAME), i.RDB$STATISTICS,
       CASE WHEN EXISTS (SELECT 1 FROM RDB$RELATION_CONSTRAINTS rc
                          WHERE rc.RDB$INDEX_NAME = i.RDB$INDEX_NAME AND rc.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY')
            THEN 1 ELSE 0 END
  FROM RDB$INDICES i JOIN RDB$RELATIONS r ON r.RDB$RELATION_NAME = i.RDB$RELATION_NAME
 WHERE COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 AND r.RDB$VIEW_BLR IS NULL AND r.RDB$EXTERNAL_FILE IS NULL
   AND COALESCE(r.RDB$RELATION_TYPE, 0) = 0
   AND i.RDB$UNIQUE_FLAG = 1 AND COALESCE(i.RDB$INDEX_INACTIVE, 0) = 0 AND i.RDB$EXPRESSION_BLR IS NULL
   AND i.RDB$STATISTICS > 0";

/// (kind, query of name and description) per commented kind.
const COMMENTS: [(&str, &str); 7] = [
    (
        kinds::VIEW,
        "SELECT TRIM(RDB$RELATION_NAME), RDB$DESCRIPTION FROM RDB$RELATIONS
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NOT NULL AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        kinds::PROCEDURE,
        "SELECT TRIM(RDB$PROCEDURE_NAME), RDB$DESCRIPTION FROM RDB$PROCEDURES
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        kinds::FUNCTION,
        "SELECT TRIM(RDB$FUNCTION_NAME), RDB$DESCRIPTION FROM RDB$FUNCTIONS
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        crate::PACKAGE,
        "SELECT TRIM(RDB$PACKAGE_NAME), RDB$DESCRIPTION FROM RDB$PACKAGES
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        kinds::TRIGGER,
        "SELECT TRIM(RDB$TRIGGER_NAME), RDB$DESCRIPTION FROM RDB$TRIGGERS
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        kinds::SEQUENCE,
        "SELECT TRIM(RDB$GENERATOR_NAME), RDB$DESCRIPTION FROM RDB$GENERATORS
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$DESCRIPTION IS NOT NULL",
    ),
    (
        kinds::TYPE,
        "SELECT TRIM(RDB$FIELD_NAME), RDB$DESCRIPTION FROM RDB$FIELDS
          WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$FIELD_NAME NOT STARTING WITH 'RDB$'
            AND RDB$DESCRIPTION IS NOT NULL",
    ),
];

fn float(c: &Column) -> Option<f64> {
    match &c.value {
        SqlType::Floating(f) => Some(*f),
        SqlType::Integer(i) => Some(*i as f64),
        SqlType::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Rows per table from `(table, selectivity, primary key)`: the primary
/// key's index when there is one, otherwise the most selective unique one.
fn estimates(stats: impl IntoIterator<Item = (String, f64, bool)>) -> Vec<RowEstimate> {
    // table -> (from the primary key, smallest selectivity)
    let mut best: BTreeMap<String, (bool, f64)> = BTreeMap::new();
    for (table, sel, pk) in stats {
        if !(sel > 0.0 && sel <= 1.0) {
            continue;
        }
        let e = best.entry(table).or_insert((pk, sel));
        if (pk && !e.0) || (pk == e.0 && sel < e.1) {
            *e = (pk, sel);
        }
    }
    best.into_iter()
        .map(|(name, (_, sel))| RowEstimate {
            object: ObjectRef { kind: kinds::TABLE.into(), schema: None, name },
            rows: (1.0 / sel).round() as u64,
        })
        .collect()
}

impl FirebirdSession {
    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        let rows = match self.rows(UNIQUE_STATS, vec![]).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("firebird: index statistics not read: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(estimates(rows.iter().filter_map(|r| {
            Some((text(r.first()?)?, float(r.get(1)?)?, r.get(2).and_then(int) == Some(1)))
        })))
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        self.run(|c| {
            let mut out = Vec::new();
            for (kind, sql) in COMMENTS {
                match c.rows(sql, vec![]) {
                    Ok(rows) => out.extend(rows.iter().filter_map(|r| {
                        let name = text(r.first()?)?;
                        let comment = text(r.get(1)?)?.trim().to_string();
                        (!comment.is_empty()).then(|| ObjectComment {
                            object: ObjectRef { kind: kind.into(), schema: None, name },
                            comment,
                        })
                    })),
                    Err(e) => tracing::debug!("firebird: {kind} comments not read: {e}"),
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
    fn rows_from_unique_selectivity() {
        let e = estimates([
            ("CLIENTES".to_string(), 1.0 / 250.0, true),
            // A unique index with more keys doesn't beat the primary key.
            ("CLIENTES".to_string(), 1.0 / 300.0, false),
            ("PEDIDOS".to_string(), 1.0 / 40.0, false),
            ("PEDIDOS".to_string(), 1.0 / 80.0, false),
            ("VACIA".to_string(), 0.0, true),
        ]);
        let got: Vec<_> = e.iter().map(|e| (e.object.name.as_str(), e.rows, e.object.kind.as_str())).collect();
        assert_eq!(got, vec![("CLIENTES", 250, "table"), ("PEDIDOS", 80, "table")]);
    }
}

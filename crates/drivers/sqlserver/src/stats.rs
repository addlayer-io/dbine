//! What the catalog already knows about the session database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Rows: `sys.dm_db_partition_stats` (heap or clustered index, summed over
//! the partitions) of tables and indexed views. It needs `VIEW DATABASE
//! STATE`; without it, `sys.partitions.rows`, which the engine keeps the
//! same way. Never a `COUNT(*)`.
//!
//! Comments: the `MS_Description` extended property of views, procedures,
//! functions, triggers, sequences and synonyms (class 1, `minor_id` 0) and
//! of user types (class 6). Fabric's warehouse has no extended properties.
//!
//! Each query is its own: one that fails leaves fewer results, not an
//! error.

use crate::{text, SqlServerSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use std::collections::HashSet;

const PARTITION_STATS_SQL: &str = "
SELECT RTRIM(o.type), s.name, o.name, CAST(SUM(ps.row_count) AS nvarchar(20))
  FROM sys.dm_db_partition_stats ps
  JOIN sys.objects o ON o.object_id = ps.object_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 AND ps.index_id IN (0, 1)
 GROUP BY o.type, s.name, o.name";

const PARTITIONS_SQL: &str = "
SELECT RTRIM(o.type), s.name, o.name, CAST(SUM(p.rows) AS nvarchar(20))
  FROM sys.partitions p
  JOIN sys.objects o ON o.object_id = p.object_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 AND p.index_id IN (0, 1)
 GROUP BY o.type, s.name, o.name";

const OBJECT_COMMENTS_SQL: &str = "
SELECT RTRIM(o.type), s.name, o.name, CAST(ep.value AS nvarchar(max))
  FROM sys.extended_properties ep
  JOIN sys.objects o ON o.object_id = ep.major_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE ep.class = 1 AND ep.minor_id = 0 AND ep.name = N'MS_Description' AND o.is_ms_shipped = 0
   AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR', 'SO', 'SN')";

const TYPE_COMMENTS_SQL: &str = "
SELECT N'TY', s.name, t.name, CAST(ep.value AS nvarchar(max))
  FROM sys.extended_properties ep
  JOIN sys.types t ON t.user_type_id = ep.major_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE ep.class = 6 AND ep.minor_id = 0 AND ep.name = N'MS_Description' AND t.is_user_defined = 1";

/// The explorer's kind of a `sys.objects.type` (plus `TY`, a user type).
fn kind_of(code: &str) -> Option<&'static str> {
    Some(match code.trim() {
        "U" => kinds::TABLE,
        "V" => kinds::VIEW,
        "P" | "PC" => kinds::PROCEDURE,
        "FN" | "IF" | "TF" | "FS" | "FT" => kinds::FUNCTION,
        "TR" => kinds::TRIGGER,
        "SO" => kinds::SEQUENCE,
        "SN" => kinds::SYNONYM,
        "TY" => kinds::TYPE,
        _ => return None,
    })
}

fn object(r: &tiberius::Row) -> Option<ObjectRef> {
    Some(ObjectRef { kind: kind_of(&text(r, 0)?)?.to_string(), schema: text(r, 1), name: text(r, 2)? })
}

impl SqlServerSession {
    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        for sql in [PARTITION_STATS_SQL, PARTITIONS_SQL] {
            match self.rows(sql, &[]).await {
                Ok(rows) => {
                    return Ok(rows
                        .iter()
                        .filter_map(|r| Some(RowEstimate { object: object(r)?, rows: text(r, 3)?.trim().parse().ok()? }))
                        .collect())
                }
                Err(e) => tracing::debug!("{:?}: row estimates unavailable: {e}", self.variant),
            }
        }
        Ok(Vec::new())
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for sql in [OBJECT_COMMENTS_SQL, TYPE_COMMENTS_SQL] {
            let rows = match self.rows(sql, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("{:?}: object comments unavailable: {e}", self.variant);
                    continue;
                }
            };
            for r in &rows {
                let (Some(object), Some(comment)) = (object(r), text(r, 3)) else { continue };
                if comment.trim().is_empty() {
                    continue;
                }
                if seen.insert((object.kind.clone(), object.schema.clone(), object.name.clone())) {
                    out.push(ObjectComment { object, comment });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::kind_of;
    use dbine_driver::kinds;

    #[test]
    fn object_types_map_to_the_explorer_kinds() {
        assert_eq!(kind_of("U "), Some(kinds::TABLE));
        assert_eq!(kind_of("V"), Some(kinds::VIEW));
        assert_eq!(kind_of("PC"), Some(kinds::PROCEDURE));
        assert_eq!(kind_of("IF"), Some(kinds::FUNCTION));
        assert_eq!(kind_of("TR"), Some(kinds::TRIGGER));
        assert_eq!(kind_of("SO"), Some(kinds::SEQUENCE));
        assert_eq!(kind_of("SN"), Some(kinds::SYNONYM));
        assert_eq!(kind_of("TY"), Some(kinds::TYPE));
        assert_eq!(kind_of("PK"), None);
    }
}

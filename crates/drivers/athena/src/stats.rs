//! What the data catalog already knows about the session database's tables
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Only `ListTableMetadata`, a free catalog call: no query execution, so
//! nothing is billed and no S3 object is read.
//!
//! - Rows: the table parameters the catalog keeps, `numRows` (left by Hive
//!   or Spark `ANALYZE`) or else `recordCount` (left by a Glue crawler). A
//!   table with neither (Athena's own `ANALYZE` keeps only column
//!   statistics) is left out; `-1` means unknown.
//! - Comments: a view's `comment` parameter. Athena writes the fixed
//!   "Presto View" there and has no `COMMENT` for views, so in practice
//!   only views made elsewhere (Hive, Spark) carry one.

use crate::AthenaSession;
use aws_sdk_athena::types::TableMetadata;
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};

const PRESTO_VIEW: &str = "Presto View";

fn is_view(t: &TableMetadata) -> bool {
    t.table_type() == Some("VIRTUAL_VIEW")
}

fn param<'a>(t: &'a TableMetadata, key: &str) -> Option<&'a str> {
    t.parameters()?.get(key).map(String::as_str)
}

/// `numRows`, else `recordCount`; negative or unreadable: none.
pub(crate) fn rows_of(num_rows: Option<&str>, record_count: Option<&str>) -> Option<u64> {
    let n = |v: Option<&str>| v.and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n >= 0).map(|n| n as u64);
    n(num_rows).or_else(|| n(record_count))
}

/// A view comment worth keeping: not empty, not Athena's fixed text.
pub(crate) fn view_comment(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|c| !c.is_empty() && *c != PRESTO_VIEW).map(str::to_string)
}

pub(crate) async fn row_estimates(s: &AthenaSession) -> Result<Vec<RowEstimate>> {
    let tables = s.table_metadata().await.unwrap_or_default();
    Ok(tables
        .iter()
        .filter(|t| !is_view(t))
        .filter_map(|t| {
            Some(RowEstimate {
                object: ObjectRef { kind: kinds::TABLE.into(), schema: None, name: t.name().to_string() },
                rows: rows_of(param(t, "numRows"), param(t, "recordCount"))?,
            })
        })
        .collect())
}

pub(crate) async fn object_comments(s: &AthenaSession) -> Result<Vec<ObjectComment>> {
    let tables = s.table_metadata().await.unwrap_or_default();
    Ok(tables
        .iter()
        .filter(|t| is_view(t))
        .filter_map(|t| {
            Some(ObjectComment {
                object: ObjectRef { kind: kinds::VIEW.into(), schema: None, name: t.name().to_string() },
                comment: view_comment(param(t, "comment"))?,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_parameters() {
        assert_eq!(rows_of(Some("1200"), Some("9")), Some(1200));
        assert_eq!(rows_of(Some("-1"), Some("9")), Some(9));
        assert_eq!(rows_of(None, Some("9.0")), None);
        assert_eq!(rows_of(None, None), None);
    }

    #[test]
    fn view_comments() {
        assert_eq!(view_comment(Some("Presto View")), None);
        assert_eq!(view_comment(Some(" ")), None);
        assert_eq!(view_comment(Some("ventas por día")).as_deref(), Some("ventas por día"));
    }
}

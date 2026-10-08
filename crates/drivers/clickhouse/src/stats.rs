//! What `system.tables` already knows about the session database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! - Rows: `total_rows`, which the engine keeps in its metadata (the sum of
//!   the active parts' row counts for the MergeTree family, the block
//!   counts for Memory…). It is NULL where the engine can't tell without
//!   reading (Log, external tables, views): those are left out. No table is
//!   read.
//! - Comments: the `COMMENT` of views, materialized views and dictionaries
//!   (tables and columns come with the schema). SQL functions take no
//!   comment.

use crate::{kind_of, text, ClickHouseSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value;

/// A non-negative count, quoted (64-bit numbers come as strings) or not.
pub(crate) fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `(name, engine, total_rows)` rows as estimates: tables (and Timeplus
/// streams) whose engine knows its count.
pub(crate) fn estimates(rows: &[Vec<Value>], flavor: crate::Flavor) -> Vec<RowEstimate> {
    rows.iter()
        .filter_map(|r| {
            let kind = kind_of(&text(r.get(1)?), flavor);
            if !matches!(kind, kinds::TABLE | kinds::STREAM) {
                return None;
            }
            Some(RowEstimate {
                object: ObjectRef { kind: kind.into(), schema: None, name: text(r.first()?) },
                rows: count(r.get(2)?)?,
            })
        })
        .collect()
}

/// `(name, engine, comment)` rows as comments, for every kind but tables.
pub(crate) fn comments(rows: &[Vec<Value>], flavor: crate::Flavor) -> Vec<ObjectComment> {
    rows.iter()
        .filter_map(|r| {
            let kind = kind_of(&text(r.get(1)?), flavor);
            let comment = text(r.get(2)?).trim().to_string();
            if matches!(kind, kinds::TABLE | kinds::STREAM) || comment.is_empty() {
                return None;
            }
            Some(ObjectComment { object: ObjectRef { kind: kind.into(), schema: None, name: text(r.first()?) }, comment })
        })
        .collect()
}

pub(crate) async fn row_estimates(s: &ClickHouseSession) -> Result<Vec<RowEstimate>> {
    let db = s.database.clone();
    let rows = s
        .rows(
            "SELECT name, engine, total_rows FROM system.tables
             WHERE database = {db:String} AND NOT is_temporary AND name NOT LIKE '.inner%' AND total_rows IS NOT NULL
             ORDER BY name",
            &[("db", &db)],
        )
        .await
        .unwrap_or_default();
    Ok(estimates(&rows, s.flavor))
}

pub(crate) async fn object_comments(s: &ClickHouseSession) -> Result<Vec<ObjectComment>> {
    let db = s.database.clone();
    let rows = s
        .rows(
            "SELECT name, engine, comment FROM system.tables
             WHERE database = {db:String} AND NOT is_temporary AND name NOT LIKE '.inner%' AND comment != ''
             ORDER BY name",
            &[("db", &db)],
        )
        .await
        .unwrap_or_default();
    Ok(comments(&rows, s.flavor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Flavor;
    use serde_json::json;

    #[test]
    fn counts_quoted_or_not() {
        assert_eq!(count(&json!("18446744073709551615")), Some(u64::MAX));
        assert_eq!(count(&json!(42)), Some(42));
        assert_eq!(count(&Value::Null), None);
        assert_eq!(count(&json!(-1)), None);
    }

    #[test]
    fn estimates_only_tables() {
        let rows = vec![
            vec![json!("ventas"), json!("MergeTree"), json!("1200")],
            vec![json!("resumen"), json!("MaterializedView"), json!("3")],
            vec![json!("log"), json!("Log"), Value::Null],
        ];
        let e = estimates(&rows, Flavor::ClickHouse);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].object.kind.as_str(), e[0].object.name.as_str(), e[0].rows), ("table", "ventas", 1200));
        let e = estimates(&[vec![json!("clicks"), json!("Stream"), json!(7)]], Flavor::Timeplus);
        assert_eq!(e[0].object.kind, kinds::STREAM);
    }

    #[test]
    fn comments_skip_tables() {
        let rows = vec![
            vec![json!("ventas"), json!("MergeTree"), json!("hechos")],
            vec![json!("v"), json!("View"), json!("ventas por día")],
            vec![json!("mv"), json!("MaterializedView"), json!("  ")],
            vec![json!("paises"), json!("Dictionary"), json!("ISO 3166")],
        ];
        let c = comments(&rows, Flavor::ClickHouse);
        let got: Vec<_> = c.iter().map(|c| (c.object.kind.as_str(), c.object.name.as_str(), c.comment.as_str())).collect();
        assert_eq!(got, [("view", "v", "ventas por día"), ("dictionary", "paises", "ISO 3166")]);
    }
}

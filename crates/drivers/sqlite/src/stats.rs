//! Approximate rows per table ([`dbine_driver::Session::row_estimates`])
//! from `sqlite_stat1`, the statistics `ANALYZE` leaves in the file.
//!
//! Only when the table exists: without it SQLite keeps no row statistics
//! and the answer is empty. Never a `COUNT(*)` (it scans the table) and
//! never `ANALYZE` (it writes to the file).
//!
//! SQLite has no comments on objects, so there is no `object_comments`.
//!
//! Shared with the libSQL driver, which runs the reads over HTTP.

use crate::schema::{self, Rows};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef};
use serde_json::Value;
use std::collections::BTreeMap;

/// Whether `sqlite_stat1` exists (`ANALYZE` has run at least once).
pub const STAT1_EXISTS: &str = "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'";

/// The statistics rows of ordinary tables (internal and libSQL's own
/// tables left out), with each table's `CREATE` to tell virtual ones apart.
pub const STAT1: &str = "SELECT s.tbl, s.idx, s.stat, COALESCE(m.sql, '') FROM sqlite_stat1 s
 JOIN sqlite_master m ON m.type = 'table' AND m.name = s.tbl
 WHERE s.tbl NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND s.tbl NOT LIKE '\\_litestream\\_%' ESCAPE '\\'
   AND s.tbl NOT LIKE 'libsql\\_%' ESCAPE '\\'";

/// Row estimates from catalog queries run by `q`. A failed read of
/// `sqlite_stat1` (permissions, a damaged table) degrades to none.
pub fn row_estimates_with<E>(q: &mut dyn FnMut(&str) -> Result<Rows, E>) -> Result<Vec<RowEstimate>, E> {
    if q(STAT1_EXISTS)?.is_empty() {
        return Ok(Vec::new());
    }
    Ok(q(STAT1).map(|rows| from_stat1(&rows)).unwrap_or_default())
}

/// One estimate per table from `sqlite_stat1` rows (`tbl, idx, stat, sql`).
/// `stat` starts with the rows of the table (`idx` NULL: a table without
/// indexes) or of the index; a table with indexes takes the largest, since
/// a partial index holds fewer rows than its table.
pub fn from_stat1(rows: &Rows) -> Vec<RowEstimate> {
    let mut by_table: BTreeMap<String, (Option<u64>, u64)> = BTreeMap::new();
    for row in rows {
        let (Some(Value::String(tbl)), Some(stat)) = (row.first(), row.get(2).and_then(Value::as_str)) else {
            continue;
        };
        if row.get(3).and_then(Value::as_str).is_some_and(schema::is_virtual) {
            continue;
        }
        let Some(n) = stat.split_whitespace().next().and_then(|t| t.parse::<u64>().ok()) else {
            continue;
        };
        let entry = by_table.entry(tbl.clone()).or_default();
        match row.get(1) {
            None | Some(Value::Null) => entry.0 = Some(n),
            Some(_) => entry.1 = entry.1.max(n),
        }
    }
    by_table
        .into_iter()
        .map(|(name, (table, index))| RowEstimate {
            object: ObjectRef { kind: kinds::TABLE.into(), schema: None, name },
            rows: table.unwrap_or(index),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stat1_rows_per_table() {
        let rows = vec![
            vec![json!("orders"), json!("orders_customer"), json!("1200 40"), json!("CREATE TABLE orders (id)")],
            vec![json!("orders"), json!("orders_open"), json!("30 1"), json!("CREATE TABLE orders (id)")],
            vec![json!("log"), Value::Null, json!("57"), json!("CREATE TABLE log (m)")],
            vec![json!("w"), json!("sqlite_autoindex_w_1"), json!("9 1 unordered"), json!("CREATE TABLE w (a)")],
            vec![json!("bad"), Value::Null, json!(""), json!("CREATE TABLE bad (a)")],
            vec![json!("ft"), Value::Null, json!("5"), json!("CREATE VIRTUAL TABLE ft USING fts5(a)")],
        ];
        let got: Vec<(String, u64)> = from_stat1(&rows).into_iter().map(|e| (e.object.name, e.rows)).collect();
        assert_eq!(got, vec![("log".into(), 57), ("orders".into(), 1200), ("w".into(), 9)]);
    }

    #[test]
    fn without_stat1_nothing_else_is_read() {
        let mut asked = Vec::new();
        let got = row_estimates_with::<()>(&mut |sql| {
            asked.push(sql.to_string());
            Ok(Vec::new())
        })
        .unwrap();
        assert!(got.is_empty());
        assert_eq!(asked, vec![STAT1_EXISTS.to_string()]);
    }
}

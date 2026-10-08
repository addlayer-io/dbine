//! What the engine behind the Flight SQL server already knows about the
//! session catalog's objects ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]). Flight SQL's own metadata
//! commands (`GetTables`) carry neither counts nor comments, so this asks
//! the engine where it has them:
//!
//! - DuckDB (GizmoSQL): `duckdb_tables().estimated_size`, the row count
//!   DuckDB keeps in its table metadata (no table is read), and the
//!   `comment` of `duckdb_views()` (`COMMENT ON VIEW`). Functions and
//!   sequences aren't listed by Flight SQL, so their comments aren't given.
//! - Dremio, DataFusion (InfluxDB 3) and any other server: no statistics or
//!   comments reachable through Flight SQL: empty.

use crate::{text, Engine, FlightSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value;

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `[schema, name, value]` rows as `(object, value)`.
fn objects<'a>(kind: &'a str, rows: &'a [Vec<Value>]) -> impl Iterator<Item = (ObjectRef, &'a Value)> + 'a {
    rows.iter().filter(|r| r.len() == 3).map(move |r| {
        (ObjectRef { kind: kind.into(), schema: Some(text(&r[0])).filter(|s| !s.is_empty()), name: text(&r[1]) }, &r[2])
    })
}

pub(crate) fn estimates(rows: &[Vec<Value>]) -> Vec<RowEstimate> {
    objects(kinds::TABLE, rows).filter_map(|(object, v)| Some(RowEstimate { object, rows: count(v)? })).collect()
}

pub(crate) fn comments(rows: &[Vec<Value>]) -> Vec<ObjectComment> {
    objects(kinds::VIEW, rows)
        .filter_map(|(object, v)| Some(ObjectComment { object, comment: Some(text(v).trim().to_string()).filter(|c| !c.is_empty())? }))
        .collect()
}

impl FlightSession {
    /// The `database_name` filter for DuckDB's catalog functions.
    fn duckdb_database(&self) -> String {
        match &self.catalog {
            Some(c) => format!("database_name = {}", lit(c)),
            None => "database_name = current_database()".into(),
        }
    }
}

pub(crate) async fn row_estimates(s: &FlightSession) -> Result<Vec<RowEstimate>> {
    if s.server.engine() != Engine::DuckDb {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT schema_name, table_name, estimated_size FROM duckdb_tables()
         WHERE {} AND NOT internal AND NOT temporary ORDER BY 1, 2",
        s.duckdb_database()
    );
    Ok(s.rows(&sql).await.map(|(_, rows)| estimates(&rows)).unwrap_or_default())
}

pub(crate) async fn object_comments(s: &FlightSession) -> Result<Vec<ObjectComment>> {
    if s.server.engine() != Engine::DuckDb {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT schema_name, view_name, comment FROM duckdb_views()
         WHERE {} AND NOT internal AND NOT temporary AND comment IS NOT NULL ORDER BY 1, 2",
        s.duckdb_database()
    );
    Ok(s.rows(&sql).await.map(|(_, rows)| comments(&rows)).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duckdb_rows() {
        let e = estimates(&[vec![json!("main"), json!("ventas"), json!(1200)], vec![json!("main"), json!("x"), Value::Null]]);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].object.schema.as_deref(), e[0].object.name.as_str(), e[0].rows), (Some("main"), "ventas", 1200));
    }

    #[test]
    fn duckdb_view_comments() {
        let c = comments(&[vec![json!("main"), json!("v"), json!("ventas por día")], vec![json!("main"), json!("w"), json!(" ")]]);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].object.kind.as_str(), c[0].comment.as_str()), ("view", "ventas por día"));
    }
}

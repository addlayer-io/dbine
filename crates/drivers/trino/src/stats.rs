//! What the connectors already know about the session catalog's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! - Rows: `SHOW STATS FOR` each table, its summary row's `row_count`. It
//!   asks the connector for the statistics it keeps (the Hive metastore's,
//!   Iceberg's snapshot summary, Delta's log, a JDBC source's planner
//!   statistics, the memory connector's own count): no table is read. A
//!   connector without statistics gives NULL and the table is left out. At
//!   most [`MAX_TABLES`] tables.
//! - Comments: `system.metadata.table_comments` for views and
//!   `system.metadata.materialized_views` for materialized views. Trino
//!   has no comments on functions.
//!
//! A read that fails (Presto's older `system.metadata`, no access) is
//! skipped.

use crate::{lit, TrinoSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, DbObject, ObjectRef, Result, Session};

/// Tables asked for their statistics at most.
const MAX_TABLES: usize = 500;

fn object(o: &DbObject) -> ObjectRef {
    ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() }
}

/// `row_count` of `SHOW STATS`' summary row (the one without a column
/// name), a double. Rows are `[column_name, …, row_count, …]` as text.
pub(crate) fn summary_rows(rows: &[Vec<String>], row_count_at: usize) -> Option<u64> {
    let r = rows.iter().find(|r| r.first().is_some_and(String::is_empty))?;
    let n: f64 = r.get(row_count_at)?.trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then(|| n.round() as u64)
}

pub(crate) async fn row_estimates(s: &mut TrinoSession) -> Result<Vec<RowEstimate>> {
    let Ok(cat) = s.catalog() else { return Ok(Vec::new()) };
    let objs = s.list_objects().await.unwrap_or_default();
    let mut out = Vec::new();
    for o in objs.iter().filter(|o| o.kind == kinds::TABLE).take(MAX_TABLES) {
        let name = format!(
            "{}.{}.{}",
            quote_ident(Quote::Double, &cat),
            quote_ident(Quote::Double, o.schema.as_deref().unwrap_or_default()),
            quote_ident(Quote::Double, &o.name)
        );
        // column_name, data_size, distinct_values_count, nulls_fraction, row_count, low_value, high_value
        if let Ok(rows) = s.strings(&format!("SHOW STATS FOR {name}")).await {
            if let Some(rows) = summary_rows(&rows, 4) {
                out.push(RowEstimate { object: object(o), rows });
            }
        }
    }
    Ok(out)
}

pub(crate) async fn object_comments(s: &mut TrinoSession) -> Result<Vec<ObjectComment>> {
    let Ok(cat) = s.catalog() else { return Ok(Vec::new()) };
    let objs = s.list_objects().await.unwrap_or_default();
    let find = |kind: &str, schema: &str, name: &str| {
        objs.iter().find(|o| o.kind == kind && o.schema.as_deref() == Some(schema) && o.name == name).map(object)
    };
    let mut out = Vec::new();
    let views = s
        .strings(&format!(
            "SELECT schema_name, table_name, comment FROM system.metadata.table_comments
             WHERE catalog_name = {} AND comment IS NOT NULL AND schema_name <> 'information_schema'",
            lit(&cat)
        ))
        .await
        .unwrap_or_default();
    let mvs = s
        .strings(&format!(
            "SELECT schema_name, name, comment FROM system.metadata.materialized_views
             WHERE catalog_name = {} AND comment IS NOT NULL",
            lit(&cat)
        ))
        .await
        .unwrap_or_default();
    for (kind, rows) in [(kinds::VIEW, views), (kinds::MATERIALIZED_VIEW, mvs)] {
        for r in rows.iter().filter(|r| r.len() == 3 && !r[2].trim().is_empty()) {
            if let Some(object) = find(kind, &r[0], &r[1]) {
                out.push(ObjectComment { object, comment: r[2].trim().to_string() });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn summary_row_count() {
        let rows = vec![row(&["id", "", "3.0", "0.0", "", "1", "3"]), row(&["", "", "", "", "3.0", "", ""])];
        assert_eq!(summary_rows(&rows, 4), Some(3));
        let unknown = vec![row(&["id", "", "", "", "", "", ""]), row(&["", "", "", "", "", "", ""])];
        assert_eq!(summary_rows(&unknown, 4), None);
        assert_eq!(summary_rows(&[], 4), None);
    }
}

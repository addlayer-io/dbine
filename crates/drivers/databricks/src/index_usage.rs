//! A table's indexes (`Session::index_usage`).
//!
//! Databricks has no secondary indexes (Delta skips files with its
//! statistics, liquid clustering and Z-order; the old Bloom filter indexes
//! are deprecated) and keeps no usage counters, so the report has no
//! indexes and says why. Unity Catalog's primary and foreign keys are
//! informational constraints, not indexes: the primary key's columns are
//! marked from `columns`, the foreign keys come from the catalog's
//! `information_schema` (as in `database_schema`), so the explorer can mark
//! them too. Hive metastore catalogs have no constraints.

use crate::DatabricksSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{ForeignKeyDef, IndexUsageReport, ObjectRef, Result};

pub const NOTE: &str = "Databricks no tiene índices secundarios (Delta usa estadísticas por archivo, clustering y Z-order) ni contadores de uso. Las claves primarias y foráneas de Unity Catalog son informativas, no índices.";

/// One table's foreign key columns (`:p0` schema, `:p1` table), in key order.
pub fn foreign_keys_sql(catalog: &str) -> String {
    let is = format!("{}.information_schema", quote_ident(Quote::Backtick, catalog));
    format!(
        "SELECT k.constraint_name, k.column_name, u.table_schema, u.table_name, u.column_name, r.delete_rule, r.update_rule
         FROM {is}.referential_constraints r
         JOIN {is}.key_column_usage k ON k.constraint_schema = r.constraint_schema AND k.constraint_name = r.constraint_name
         JOIN {is}.key_column_usage u ON u.constraint_schema = r.unique_constraint_schema
          AND u.constraint_name = r.unique_constraint_name AND u.ordinal_position = k.position_in_unique_constraint
         WHERE k.table_schema = :p0 AND k.table_name = :p1
         ORDER BY k.constraint_name, k.ordinal_position"
    )
}

/// Its rows grouped by constraint.
pub fn foreign_keys(rows: &[Vec<Option<String>>]) -> Vec<ForeignKeyDef> {
    let cell = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    let rule = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().filter(|v| !matches!(v.as_str(), "" | "NO ACTION" | "RESTRICT"));
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for r in rows {
        let name = Some(cell(r, 0));
        match out.last_mut().filter(|f| f.name == name) {
            Some(fk) => {
                fk.columns.push(cell(r, 1));
                fk.ref_columns.push(cell(r, 4));
            }
            None => out.push(ForeignKeyDef {
                name,
                columns: vec![cell(r, 1)],
                ref_schema: Some(cell(r, 2)),
                ref_table: cell(r, 3),
                ref_columns: vec![cell(r, 4)],
                on_delete: rule(r, 5),
                on_update: rule(r, 6),
            }),
        }
    }
    out
}

pub(crate) async fn report(s: &mut DatabricksSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let schema = table.schema().or(s.schema.as_deref()).unwrap_or("default").to_string();
    let foreign_keys = match s.catalog().map(foreign_keys_sql) {
        Ok(sql) => match s.text_rows(&sql, &[&schema, &table.name]).await {
            Ok(rows) => foreign_keys(&rows),
            // Hive metastore: no information_schema, no constraints.
            Err(e) => {
                tracing::debug!("databricks: foreign keys not read: {e}");
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    };
    Ok(IndexUsageReport { note: Some(NOTE.into()), foreign_keys, seek_scan_split: false, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &[&str]) -> Vec<Option<String>> {
        v.iter().map(|c| (!c.is_empty()).then(|| c.to_string())).collect()
    }

    #[test]
    fn query_and_rows() {
        let q = foreign_keys_sql("main");
        assert!(q.contains("FROM `main`.information_schema.referential_constraints r"), "{q}");
        assert!(q.contains("WHERE k.table_schema = :p0 AND k.table_name = :p1"));
        let fks = foreign_keys(&[
            row(&["fk_c", "a", "s", "c", "x", "NO ACTION", "NO ACTION"]),
            row(&["fk_c", "b", "s", "c", "y", "NO ACTION", "NO ACTION"]),
            row(&["fk_d", "d", "s2", "d", "id", "CASCADE", ""]),
        ]);
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].columns.clone(), fks[0].ref_columns.clone(), fks[0].on_delete.clone()), (vec!["a".into(), "b".into()], vec!["x".into(), "y".into()], None));
        assert_eq!((fks[1].ref_schema.as_deref(), fks[1].ref_table.as_str(), fks[1].on_delete.as_deref()), (Some("s2"), "d", Some("CASCADE")));
    }
}

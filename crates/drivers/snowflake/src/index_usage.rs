//! A table's indexes (`Session::index_usage`).
//!
//! Standard Snowflake tables have no indexes (micro-partitions, clustering
//! and search optimization instead); their primary, unique and foreign keys
//! are informational constraints, not indexes. Hybrid tables do have them:
//! `SHOW INDEXES IN TABLE` lists the primary key's (`SYS_INDEX_…_PRIMARY`),
//! the ones Snowflake makes for unique and foreign keys and the secondary
//! ones (`INCLUDE` columns as the included ones). Snowflake keeps no usage
//! counters per index (`ACCESS_HISTORY` is per column, not per index) nor
//! their size: `stats_available` is false and the note says so.
//!
//! The foreign keys (for the explorer's icons) come from `SHOW IMPORTED
//! KEYS IN TABLE`, as `database_schema` reads them for the whole database.

use crate::ddl::{self, Row};
use crate::SnowflakeSession;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};

pub const NO_INDEXES: &str = "Las tablas estándar de Snowflake no tienen índices (sus claves primarias, únicas y foráneas son informativas); solo las tablas híbridas los tienen.";
pub const NO_COUNTERS: &str = "Snowflake no lleva contadores de uso ni tamaño por índice: se listan los índices de la tabla híbrida sin lecturas ni escrituras.";

/// `[A, "b c"]` / `A,B` → the column names.
fn columns(v: Option<&String>) -> Vec<String> {
    let v = v.map(String::as_str).unwrap_or_default().trim().trim_start_matches('[').trim_end_matches(']');
    v.split(',').map(|c| c.trim().trim_matches('"').to_string()).filter(|c| !c.is_empty()).collect()
}

/// `SHOW INDEXES IN TABLE` rows (lower-case column names) as indexes.
pub fn indexes(rows: &[Row]) -> Vec<IndexUsage> {
    let mut out: Vec<IndexUsage> = rows
        .iter()
        .map(|r| {
            let name = r.get("name").cloned().unwrap_or_default();
            let up = name.to_ascii_uppercase();
            let pk = up.starts_with("SYS_INDEX_") && up.ends_with("_PRIMARY");
            let unique = pk || r.get("is_unique").is_some_and(|u| matches!(u.to_ascii_uppercase().as_str(), "Y" | "YES" | "TRUE"));
            IndexUsage {
                kind: if pk { "PRIMARY KEY".into() } else { "INDEX".into() },
                unique,
                primary_key: pk,
                key_columns: columns(r.get("columns")),
                included_columns: columns(r.get("included_columns")),
                name,
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| b.primary_key.cmp(&a.primary_key).then_with(|| a.name.cmp(&b.name)));
    out
}

pub(crate) async fn report(s: &mut SnowflakeSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let db = s.database()?;
    let schema = table.schema().unwrap_or("PUBLIC");
    let fq = format!("{}.{}", qualified_name(Quote::Double, None, &db), qualified_name(Quote::Double, Some(schema), &table.name));
    // Only hybrid tables answer with rows; a standard one, with none or an error.
    let ixs = match s.named_rows(&format!("SHOW INDEXES IN TABLE {fq}")).await {
        Ok(rows) => indexes(&rows),
        Err(e) => {
            tracing::debug!("snowflake: SHOW INDEXES IN TABLE {fq}: {e}");
            Vec::new()
        }
    };
    let fks = s.named_rows(&format!("SHOW IMPORTED KEYS IN TABLE {fq}")).await.unwrap_or_default();
    let this: Row = [("table_schema", schema), ("table_name", table.name.as_str())].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let foreign_keys = ddl::assemble(&[this], &[], &[], &[], &fks).pop().map(|t| t.foreign_keys).unwrap_or_default();
    Ok(IndexUsageReport {
        note: Some(if ixs.is_empty() { NO_INDEXES } else { NO_COUNTERS }.into()),
        indexes: ixs,
        foreign_keys,
        seek_scan_split: false,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kv: &[(&str, &str)]) -> Row {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn hybrid_indexes() {
        let r = indexes(&[
            row(&[("name", "IX_CODE"), ("is_unique", "N"), ("columns", "[CODE]"), ("included_columns", "[NOTE, \"Other Col\"]")]),
            row(&[("name", "SYS_INDEX_ORDERS_PRIMARY"), ("is_unique", "Y"), ("columns", "[ID]"), ("included_columns", "[]")]),
            row(&[("name", "SYS_INDEX_ORDERS_UNIQUE_EMAIL"), ("is_unique", "Y"), ("columns", "[EMAIL]")]),
        ]);
        assert_eq!(r.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["SYS_INDEX_ORDERS_PRIMARY", "IX_CODE", "SYS_INDEX_ORDERS_UNIQUE_EMAIL"]);
        assert!(r[0].primary_key && r[0].unique && r[0].kind == "PRIMARY KEY" && r[0].key_columns == vec!["ID"]);
        assert!(r[0].included_columns.is_empty());
        assert!(!r[1].unique && !r[1].primary_key);
        assert_eq!(r[1].included_columns, vec!["NOTE", "Other Col"]);
        assert!(r[2].unique && !r[2].primary_key);
    }

    #[test]
    fn foreign_keys_of_one_table() {
        let fks = vec![
            row(&[("fk_schema_name", "PUBLIC"), ("fk_table_name", "ORDERS"), ("fk_name", "FK_CUST"), ("fk_column_name", "CUST_ID"), ("key_sequence", "1"),
                ("pk_schema_name", "PUBLIC"), ("pk_table_name", "CUSTOMERS"), ("pk_column_name", "ID"), ("delete_rule", "NO ACTION")]),
        ];
        let this = row(&[("table_schema", "PUBLIC"), ("table_name", "ORDERS")]);
        let t = ddl::assemble(&[this], &[], &[], &[], &fks).pop().unwrap();
        assert_eq!(t.foreign_keys.len(), 1);
        assert_eq!((t.foreign_keys[0].columns.clone(), t.foreign_keys[0].ref_table.as_str()), (vec!["CUST_ID".to_string()], "CUSTOMERS"));
    }
}

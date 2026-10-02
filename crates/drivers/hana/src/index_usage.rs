//! A table's indexes (`Session::index_usage`).
//!
//! - The entries come from the same catalog read as the schema compare,
//!   narrowed to the table (`SYS.CONSTRAINTS`, `SYS.INDEXES`,
//!   `SYS.INDEX_COLUMNS`, `SYS.FULLTEXT_INDEXES`,
//!   `SYS.REFERENTIAL_CONSTRAINTS`), so an index has the same name here as
//!   in the drop script "Eliminar índice" generates (unnamed UNIQUE
//!   constraints included). The kind is HANA's `INDEX_TYPE` (`CPBTREE`,
//!   `BTREE`, `INVERTED VALUE`, `INVERTED HASH`, `INVERTED INDIVIDUAL`) or
//!   `FULLTEXT`.
//! - The size: `M_RS_INDEXES.INDEX_SIZE` for row-store tables' indexes;
//!   column-store indexes live in their columns' dictionaries and have no
//!   size of their own. Where the monitoring view is refused, empty.
//! - HANA keeps no per-index usage counters (its index advice comes from
//!   the plan cache, not from counters on the indexes), so
//!   `stats_available` is false and the note says so.

use crate::{int, text, HanaSession};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result, TableSchema};
use std::collections::HashMap;

pub const NOTE: &str = "SAP HANA no registra cuántas veces se usa cada índice: se listan los índices con sus columnas (y el tamaño de los de tablas row store), sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución.";

pub(crate) const SIZES_SQL: &str = "SELECT INDEX_NAME, SUM(INDEX_SIZE) FROM M_RS_INDEXES
 WHERE SCHEMA_NAME = ? AND TABLE_NAME = ? GROUP BY INDEX_NAME";

pub(crate) async fn report(s: &HanaSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let tables = crate::schema::read_tables(s, Some(&table.name)).await?;
    let sizes: HashMap<String, u64> = match s.rows(SIZES_SQL, &[s.schema.as_str(), table.name.as_str()]).await {
        Ok(rows) => rows.iter().filter_map(|r| Some((r.first().and_then(text)?, u64::try_from(r.get(1).and_then(int)?).ok()?))).collect(),
        Err(e) => {
            tracing::debug!("hana: M_RS_INDEXES not read: {e}");
            HashMap::new()
        }
    };
    Ok(assemble(tables.iter().find(|t| t.name == table.name), &sizes))
}

/// The primary key first, then the indexes in the schema's order.
pub(crate) fn assemble(t: Option<&TableSchema>, sizes: &HashMap<String, u64>) -> IndexUsageReport {
    let Some(t) = t else { return IndexUsageReport { note: Some(NOTE.into()), writes_counted: false, ..Default::default() } };
    let kb = |name: &str| sizes.get(name).map(|b| b.div_ceil(1024));
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        let name = pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into());
        indexes.push(IndexUsage {
            size_kb: kb(&name),
            name,
            kind: "PRIMARY KEY".into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            ..Default::default()
        });
    }
    indexes.extend(t.indexes.iter().map(|ix| IndexUsage {
        name: ix.name.clone(),
        kind: ix.kind.clone().unwrap_or_else(|| "INDEX".into()),
        unique: ix.unique,
        key_columns: ix.columns.clone(),
        filter: ix.filter.clone(),
        size_kb: kb(&ix.name),
        ..Default::default()
    }));
    IndexUsageReport { note: Some(NOTE.into()), indexes, foreign_keys: t.foreign_keys.clone(), writes_counted: false, ..Default::default() }.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::one_table;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};

    #[test]
    fn catalog_queries_narrow_to_the_table() {
        assert_eq!(
            one_table("SELECT x FROM SYS.TABLES\n WHERE SCHEMA_NAME = ? AND IS_SYSTEM_TABLE = 'FALSE'"),
            "SELECT x FROM SYS.TABLES\n WHERE SCHEMA_NAME = ? AND TABLE_NAME = ? AND IS_SYSTEM_TABLE = 'FALSE'"
        );
        assert_eq!(
            one_table("SELECT 1 FROM SYS.INDEXES i JOIN c ON c.SCHEMA_NAME = i.SCHEMA_NAME WHERE i.SCHEMA_NAME = ? AND x"),
            "SELECT 1 FROM SYS.INDEXES i JOIN c ON c.SCHEMA_NAME = i.SCHEMA_NAME WHERE i.SCHEMA_NAME = ? AND i.TABLE_NAME = ? AND x"
        );
        assert_eq!(one_table("SELECT * FROM SYS.FULLTEXT_INDEXES WHERE SCHEMA_NAME = ?"), "SELECT * FROM SYS.FULLTEXT_INDEXES WHERE SCHEMA_NAME = ? AND TABLE_NAME = ?");
    }

    #[test]
    fn key_indexes_sizes_and_foreign_keys() {
        let t = TableSchema {
            name: "T".into(),
            primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }),
            indexes: vec![
                IndexDef { name: "IX_A".into(), columns: vec!["A".into(), "B".into()], kind: Some("CPBTREE".into()), ..Default::default() },
                IndexDef { name: "UK_T_1".into(), columns: vec!["C".into()], unique: true, kind: Some("INVERTED VALUE".into()), ..Default::default() },
                IndexDef { name: "FTI".into(), columns: vec!["D".into()], kind: Some("FULLTEXT".into()), ..Default::default() },
            ],
            foreign_keys: vec![ForeignKeyDef { columns: vec!["P_ID".into()], ref_table: "P".into(), ref_columns: vec!["ID".into()], ..Default::default() }],
            ..Default::default()
        };
        let sizes = HashMap::from([("IX_A".to_string(), 5000u64)]);
        let r = assemble(Some(&t), &sizes);
        assert!(!r.stats_available && r.note.is_some());
        let got: Vec<(&str, &str, Option<u64>)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str(), i.size_kb)).collect();
        assert_eq!(got, [("PRIMARY KEY", "PRIMARY KEY", None), ("IX_A", "CPBTREE", Some(5)), ("UK_T_1", "INVERTED VALUE", None), ("FTI", "FULLTEXT", None)]);
        assert!(r.indexes[0].primary_key && r.indexes[2].unique);
        assert_eq!(r.foreign_keys, t.foreign_keys);
        assert!(assemble(None, &sizes).indexes.is_empty());
    }
}

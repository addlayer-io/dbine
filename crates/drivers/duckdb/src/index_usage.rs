//! A table's indexes (`Session::index_usage`).
//!
//! - The indexes, the primary key and the foreign keys come from the same
//!   catalog read as the schema compare (`duckdb_constraints()`,
//!   `duckdb_indexes()`), so an index has the same name here as in the drop
//!   script "Eliminar índice" generates. Every one is an ART index: the
//!   primary key's, a UNIQUE constraint's and each `CREATE INDEX`.
//! - DuckDB counts nothing per index (and doesn't report an index's size),
//!   so `stats_available` is false and the note says so. Most of its reads
//!   go through zonemaps, not indexes; the plan shows when one is used
//!   (`INDEX_SCAN`).
//!
//! The files preset has no indexes (its tables are views over files).

use dbine_driver::{IndexUsage, IndexUsageReport, TableSchema};

pub const NOTE: &str = "DuckDB no registra cuántas veces se usa cada índice ni su tamaño: se listan los índices ART con sus columnas, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución (INDEX_SCAN).";

/// The report for `t` (`None`: the table isn't there): the primary key
/// first, then the indexes in the schema's order.
pub fn report(t: Option<&TableSchema>) -> IndexUsageReport {
    let Some(t) = t else { return IndexUsageReport { note: Some(NOTE.into()), writes_counted: false, ..Default::default() } };
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        indexes.push(IndexUsage {
            name: pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into()),
            kind: "ART".into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            ..Default::default()
        });
    }
    indexes.extend(t.indexes.iter().map(|ix| IndexUsage {
        name: ix.name.clone(),
        kind: ix.kind.clone().unwrap_or_else(|| "ART".into()),
        unique: ix.unique,
        key_columns: ix.columns.clone(),
        filter: ix.filter.clone(),
        ..Default::default()
    }));
    IndexUsageReport { note: Some(NOTE.into()), indexes, foreign_keys: t.foreign_keys.clone(), writes_counted: false, ..Default::default() }.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};

    #[test]
    fn key_first_then_indexes() {
        let t = TableSchema {
            name: "t".into(),
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            indexes: vec![
                IndexDef { name: "t_code_key".into(), columns: vec!["code".into()], unique: true, kind: Some("ART".into()), ..Default::default() },
                IndexDef { name: "ix_a".into(), columns: vec!["a".into(), "(lower(b))".into()], ..Default::default() },
            ],
            foreign_keys: vec![ForeignKeyDef { columns: vec!["p_id".into()], ref_table: "p".into(), ref_columns: vec!["id".into()], ..Default::default() }],
            ..Default::default()
        };
        let r = report(Some(&t));
        assert!(!r.stats_available && r.note.is_some());
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["PRIMARY KEY", "t_code_key", "ix_a"]);
        assert!(r.indexes[0].primary_key && r.indexes[0].unique);
        assert!(r.indexes[1].unique && !r.indexes[1].primary_key);
        assert_eq!(r.indexes[2].key_columns, ["a", "(lower(b))"]);
        assert!(r.indexes.iter().all(|i| i.kind == "ART" && !i.unused && i.read_share.is_none()));
        assert_eq!(r.foreign_keys, t.foreign_keys);
        assert!(report(None).indexes.is_empty());
    }
}

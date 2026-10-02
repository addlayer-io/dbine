//! A table's indexes (`Session::index_usage`).
//!
//! - The entries come from the same catalog read as the schema compare
//!   (only that table): the primary key (the MergeTree's sparse index, one
//!   mark per granule), the data-skipping indexes (`minmax`, `set`,
//!   `bloom_filter`…, kind with its GRANULARITY) and the projections, so
//!   each one has the same name here as in the drop script "Eliminar
//!   índice" generates.
//! - The size: the primary key's `primary_key_bytes_in_memory`
//!   (`system.parts`), a skip index's `data_compressed_bytes`
//!   (`system.data_skipping_indices`) and a projection's `bytes_on_disk`
//!   (`system.projection_parts`), active parts only; what the server
//!   doesn't answer stays empty.
//! - ClickHouse counts nothing per index (the granules a query skipped are
//!   only in `EXPLAIN indexes = 1`), so `stats_available` is false and the
//!   note says so. It has no foreign keys.
//!
//! Timeplus reads the same system tables.

use crate::schema::is_projection;
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result, TableSchema};
use serde_json::Value;
use std::collections::HashMap;

/// The note, with the engine's display name.
pub(crate) fn note(flavor: Flavor) -> String {
    let engine = match flavor {
        Flavor::ClickHouse => "ClickHouse",
        Flavor::Timeplus => "Timeplus Proton",
    };
    format!(
        "{engine} no registra cuántas veces se usa cada índice: se listan la clave primaria, los índices de salto y las proyecciones con su tamaño, sin contadores. Para ver cuántos gránulos descarta cada índice en una consulta, usá EXPLAIN indexes = 1."
    )
}

pub(crate) const PK_SIZE_SQL: &str = "SELECT sum(primary_key_bytes_in_memory) FROM system.parts
 WHERE database = {db:String} AND table = {t:String} AND active";
pub(crate) const SKIP_SIZE_SQL: &str = "SELECT name, data_compressed_bytes FROM system.data_skipping_indices
 WHERE database = {db:String} AND table = {t:String}";
pub(crate) const PROJECTION_SIZE_SQL: &str = "SELECT name, sum(bytes_on_disk) FROM system.projection_parts
 WHERE database = {db:String} AND table = {t:String} AND active GROUP BY name";

fn bytes(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn named_sizes(rows: &[Vec<Value>]) -> HashMap<String, u64> {
    rows.iter().filter_map(|r| Some((text(r.first()?), bytes(r.get(1)?)?))).collect()
}

pub(crate) async fn report(s: &ClickHouseSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let t = s.catalog(Some(&table.name)).await?.into_iter().next();
    let Some(t) = t else { return Ok(IndexUsageReport { note: Some(note(s.flavor)), writes_counted: false, ..Default::default() }) };
    let db = s.database.clone();
    let params = [("db", db.as_str()), ("t", table.name.as_str())];
    let pk = s.rows(PK_SIZE_SQL, &params).await.ok().and_then(|r| r.first().and_then(|r| r.first()).and_then(bytes));
    let skip = s.rows(SKIP_SIZE_SQL, &params).await.map(|r| named_sizes(&r)).unwrap_or_default();
    let projections = s.rows(PROJECTION_SIZE_SQL, &params).await.map(|r| named_sizes(&r)).unwrap_or_default();
    Ok(assemble(s.flavor, &t, pk, &skip, &projections))
}

/// The sorting key first, then the skip indexes and the projections.
pub(crate) fn assemble(flavor: Flavor, t: &TableSchema, pk_bytes: Option<u64>, skip: &HashMap<String, u64>, projections: &HashMap<String, u64>) -> IndexUsageReport {
    let kb = |b: u64| b.div_ceil(1024);
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        indexes.push(IndexUsage {
            name: "PRIMARY KEY".into(),
            kind: "SPARSE".into(),
            primary_key: true,
            key_columns: pk.columns.clone(),
            size_kb: pk_bytes.map(kb),
            ..Default::default()
        });
    }
    for ix in &t.indexes {
        let projection = is_projection(ix);
        let size = if projection { projections.get(&ix.name) } else { skip.get(&ix.name) };
        indexes.push(IndexUsage {
            name: ix.name.clone(),
            kind: ix.kind.clone().unwrap_or_default().to_uppercase(),
            key_columns: ix.columns.clone(),
            size_kb: size.copied().map(kb),
            ..Default::default()
        });
    }
    IndexUsageReport { note: Some(note(flavor)), indexes, writes_counted: false, ..Default::default() }.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{IndexDef, KeyDef};
    use serde_json::json;

    #[test]
    fn key_skip_indexes_and_projections() {
        let t = TableSchema {
            name: "t".into(),
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into(), "ts".into()] }),
            indexes: vec![
                IndexDef { name: "ix_a".into(), columns: vec!["a".into()], kind: Some("minmax".into()), ..Default::default() },
                IndexDef { name: "ix_b".into(), columns: vec!["b".into()], kind: Some("bloom_filter GRANULARITY 4".into()), ..Default::default() },
                IndexDef { name: "p_by_a".into(), columns: vec!["(SELECT * ORDER BY a)".into()], kind: Some("PROJECTION".into()), ..Default::default() },
            ],
            ..Default::default()
        };
        let skip = named_sizes(&[vec![json!("ix_a"), json!("2048")], vec![json!("ix_b"), json!(10)]]);
        let proj = named_sizes(&[vec![json!("p_by_a"), json!("5000")]]);
        let r = assemble(Flavor::ClickHouse, &t, Some(4096), &skip, &proj);
        assert!(!r.stats_available && r.note.is_some() && r.foreign_keys.is_empty());
        let got: Vec<(&str, &str, Option<u64>)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str(), i.size_kb)).collect();
        assert_eq!(
            got,
            [("PRIMARY KEY", "SPARSE", Some(4)), ("ix_a", "MINMAX", Some(2)), ("ix_b", "BLOOM_FILTER GRANULARITY 4", Some(1)), ("p_by_a", "PROJECTION", Some(5))]
        );
        assert!(r.indexes[0].primary_key && !r.indexes[0].unique, "a MergeTree key doesn't enforce uniqueness");
        assert_eq!(r.indexes[0].key_columns, ["id", "ts"]);
        // Nothing from the server: no sizes.
        let r = assemble(Flavor::Timeplus, &t, None, &HashMap::new(), &HashMap::new());
        assert!(r.indexes.iter().all(|i| i.size_kb.is_none()));
        assert!(r.note.as_deref().unwrap().starts_with("Timeplus Proton no registra"));
    }
}

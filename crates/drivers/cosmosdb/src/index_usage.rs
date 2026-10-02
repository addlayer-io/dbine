//! A container's indexes (`Session::index_usage`), from its definition:
//!
//! - The primary key: the partition key paths and `id` (an item is unique
//!   by both).
//! - The range indexes of the indexing policy's `includedPaths` (with the
//!   `excludedPaths` as their filter), then the unique keys, the
//!   composite indexes and the spatial, full-text and vector ones, named
//!   as the schema compare names them (`unique_1`, `composite_1`…).
//!
//! Cosmos DB has no per-index usage counters (its index metrics are per
//! query, `x-ms-cosmos-populateindexmetrics`) nor per-index sizes: the
//! report has no counters and says so. No foreign keys.

use crate::container_schema;
use dbine_driver::{IndexUsage, IndexUsageReport};
use serde_json::Value;

pub(crate) const NOTE: &str = "Cosmos DB no lleva la cuenta del uso de cada índice (las métricas de índices son por consulta): se listan sin contadores.";

fn paths(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().into_iter().flatten().filter_map(|p| p["path"].as_str()).map(str::to_string).collect()
}

/// The report for a container's definition (`GET …/colls/<name>`).
pub(crate) fn report(coll: &Value) -> IndexUsageReport {
    let mut out = Vec::new();
    let mut key: Vec<String> = coll["partitionKey"]["paths"].as_array().into_iter().flatten().filter_map(Value::as_str).map(crate::ddl::field_of).collect();
    key.push("id".into());
    out.push(IndexUsage { name: "id".into(), kind: "PRIMARY KEY".into(), unique: true, primary_key: true, key_columns: key, ..Default::default() });
    let policy = &coll["indexingPolicy"];
    let mode = policy["indexingMode"].as_str().unwrap_or("consistent");
    let mut note = NOTE.to_string();
    if mode.eq_ignore_ascii_case("none") {
        note.push_str(" La política de indexación está en «none»: solo se busca por id y clave de partición.");
    } else {
        let excluded = paths(policy, "excludedPaths");
        let filter = (!excluded.is_empty()).then(|| format!("excluye {}", excluded.join(", ")));
        for (n, p) in paths(policy, "includedPaths").into_iter().enumerate() {
            out.push(IndexUsage { name: format!("range_{}", n + 1), kind: "RANGE".into(), key_columns: vec![p], filter: filter.clone(), ..Default::default() });
        }
    }
    // The rest as the schema compare reads them (same names and columns).
    for ix in container_schema(coll, &[], None).indexes {
        let kind = match (ix.unique, ix.kind.as_deref()) {
            (true, _) => "UNIQUE".to_string(),
            (_, Some(k)) => k.to_ascii_uppercase(),
            _ => "RANGE".to_string(),
        };
        out.push(IndexUsage { name: ix.name, kind, unique: ix.unique, key_columns: ix.columns, ..Default::default() });
    }
    IndexUsageReport { stats_available: false, note: Some(note), indexes: out, seek_scan_split: false, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policy_becomes_indexes() {
        let coll = json!({
            "id": "pedidos",
            "partitionKey": {"paths": ["/cliente"], "kind": "Hash"},
            "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/codigo"]}]},
            "indexingPolicy": {
                "indexingMode": "consistent",
                "includedPaths": [{"path": "/*"}],
                "excludedPaths": [{"path": "/notas/?"}, {"path": "/\"_etag\"/?"}],
                "compositeIndexes": [[{"path": "/cliente", "order": "ascending"}, {"path": "/fecha", "order": "descending"}]],
                "spatialIndexes": [{"path": "/loc/*", "types": ["Point"]}]
            }
        });
        let r = report(&coll).derived();
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["id", "range_1", "unique_1", "composite_1", "spatial_1"]);
        assert!(r.indexes[0].primary_key);
        assert_eq!(r.indexes[0].key_columns, ["cliente", "id"]);
        assert_eq!(r.indexes[1].filter.as_deref(), Some("excluye /notas/?, /\"_etag\"/?"));
        assert!(r.indexes[2].unique && r.indexes[2].kind == "UNIQUE");
        assert_eq!(r.indexes[3].key_columns, ["cliente", "fecha DESC"]);
        assert_eq!(r.indexes[3].kind, "COMPOSITE");
        assert_eq!(r.indexes[4].kind, "SPATIAL");
        assert!(!r.stats_available);
    }

    #[test]
    fn indexing_mode_none() {
        let r = report(&json!({"id": "c", "partitionKey": {"paths": ["/a"]}, "indexingPolicy": {"indexingMode": "none", "includedPaths": [], "excludedPaths": []}}));
        assert_eq!(r.indexes.len(), 1);
        assert!(r.note.unwrap().contains("none"));
    }
}

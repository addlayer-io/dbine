//! A class's indexes (`Session::index_usage`), from the database's
//! metadata (`GET /database/{db}`), named as the schema compare names them:
//! their kind (UNIQUE, NOTUNIQUE, FULLTEXT, DICTIONARY, the `_HASH_INDEX`
//! variants, SPATIAL…), their fields and options. Records are addressed by
//! their `@rid`, which is not an index: there is no primary key entry.
//!
//! The foreign keys are the class's LINK properties with a linked class
//! (`ref_columns` `@rid`), as the ER diagram reads them.
//!
//! OrientDB counts no use per index (its profiler is in the Enterprise
//! edition and is server-wide): the report has no counters and says so.

use crate::{as_text, ddl};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport};
use serde_json::Value;

pub(crate) const NOTE: &str = "OrientDB no lleva la cuenta del uso de cada índice: se listan sin contadores.";

/// The report for `class` from the database's metadata.
pub(crate) fn report(meta: &Value, class: &str) -> Option<IndexUsageReport> {
    let c = meta.get("classes").and_then(Value::as_array)?.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(class))?;
    let indexes = meta
        .get("indexes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|ix| ix.pointer("/configuration/indexDefinition/className").and_then(Value::as_str) == Some(class))
        .map(|ix| {
            let (fields, _) = ddl::index_parts(ix);
            let ty = ix.pointer("/configuration/type").map(as_text).unwrap_or_default();
            IndexUsage { name: ix.get("name").map(as_text).unwrap_or_default(), unique: ty.starts_with("UNIQUE"), kind: ty, key_columns: fields, ..Default::default() }
        })
        .collect();
    let foreign_keys = c
        .get("properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("type").and_then(Value::as_str).is_some_and(|t| t.starts_with("LINK")))
        .filter_map(|p| {
            let lc = p.get("linkedClass").and_then(Value::as_str)?;
            Some(ForeignKeyDef {
                name: None,
                columns: vec![p.get("name").map(as_text).unwrap_or_default()],
                ref_schema: None,
                ref_table: lc.to_string(),
                ref_columns: vec!["@rid".into()],
                on_delete: None,
                on_update: None,
            })
        })
        .collect();
    Some(IndexUsageReport { stats_available: false, note: Some(NOTE.into()), indexes, foreign_keys, seek_scan_split: false, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn class_indexes_and_links() {
        let meta = json!({
            "classes": [
                {"name": "Pedido", "properties": [
                    {"name": "cliente", "type": "LINK", "linkedClass": "Cliente"},
                    {"name": "items", "type": "LINKLIST", "linkedClass": "Item"},
                    {"name": "fecha", "type": "DATE"}
                ]},
                {"name": "Cliente", "properties": []}
            ],
            "indexes": [
                {"name": "Pedido.fecha", "configuration": {"type": "NOTUNIQUE", "indexDefinition": {"className": "Pedido", "field": "fecha"}}},
                {"name": "Pedido.codigo", "configuration": {"type": "UNIQUE_HASH_INDEX", "indexDefinition": {"className": "Pedido", "field": "codigo"}}},
                {"name": "Cliente.nombre", "configuration": {"type": "NOTUNIQUE", "indexDefinition": {"className": "Cliente", "field": "nombre"}}}
            ]
        });
        let r = report(&meta, "Pedido").unwrap().derived();
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Pedido.fecha", "Pedido.codigo"]);
        assert!(r.indexes[1].unique && !r.indexes[0].unique);
        assert_eq!(r.indexes[0].kind, "NOTUNIQUE");
        assert_eq!(r.foreign_keys.len(), 2);
        assert_eq!(r.foreign_keys[0].ref_table, "Cliente");
        assert_eq!(r.foreign_keys[0].columns, ["cliente"]);
        assert!(!r.stats_available);
        assert!(report(&meta, "Nada").is_none());
    }
}

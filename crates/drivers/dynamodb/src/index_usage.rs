//! A table's indexes (`Session::index_usage`), from `DescribeTable`:
//!
//! - The primary key (partition key, then sort key), with the table's size.
//! - The global and local secondary indexes, named and keyed as the schema
//!   compare reads them, with their projection (`INCLUDE`'s attributes as
//!   included columns; `KEYS_ONLY` in the kind) and their size
//!   (`IndexSizeBytes`).
//!
//! DynamoDB counts no reads per index: the consumed capacity per GSI is in
//! CloudWatch (and in the response of each request), not in the table's
//! API, and LSIs share the table's. The report has no counters and says
//! so. No foreign keys.

use crate::key_order;
use aws_sdk_dynamodb::types::{Projection, TableDescription};
use dbine_driver::{IndexUsage, IndexUsageReport};

pub(crate) const NOTE: &str = "DynamoDB no cuenta las lecturas de cada índice (la capacidad consumida por GSI está en CloudWatch): se listan sin contadores.";

fn kb(bytes: Option<i64>) -> Option<u64> {
    bytes.map(|b| (b.max(0) as u64).div_ceil(1024))
}

/// The kind with the projection (`GSI`, `GSI KEYS_ONLY`, `LSI INCLUDE`…)
/// and the included attributes.
fn projection(kind: &str, p: Option<&Projection>) -> (String, Vec<String>) {
    match p.and_then(|p| p.projection_type()).map(|t| t.as_str()) {
        Some("INCLUDE") => (format!("{kind} INCLUDE"), p.map(|p| p.non_key_attributes().to_vec()).unwrap_or_default()),
        Some("KEYS_ONLY") => (format!("{kind} KEYS_ONLY"), Vec::new()),
        _ => (kind.to_string(), Vec::new()),
    }
}

pub(crate) fn report(d: &TableDescription) -> IndexUsageReport {
    let mut out = vec![IndexUsage {
        name: "PRIMARY".into(),
        kind: "PRIMARY KEY".into(),
        unique: true,
        primary_key: true,
        key_columns: key_order(d.key_schema()),
        size_kb: kb(d.table_size_bytes()),
        ..Default::default()
    }];
    for g in d.global_secondary_indexes() {
        let (kind, included) = projection("GSI", g.projection());
        out.push(IndexUsage {
            name: g.index_name().unwrap_or_default().to_string(),
            kind,
            key_columns: key_order(g.key_schema()),
            included_columns: included,
            size_kb: kb(g.index_size_bytes()),
            ..Default::default()
        });
    }
    for l in d.local_secondary_indexes() {
        let (kind, included) = projection("LSI", l.projection());
        out.push(IndexUsage {
            name: l.index_name().unwrap_or_default().to_string(),
            kind,
            key_columns: key_order(l.key_schema()),
            included_columns: included,
            size_kb: kb(l.index_size_bytes()),
            ..Default::default()
        });
    }
    IndexUsageReport { stats_available: false, note: Some(NOTE.into()), indexes: out, seek_scan_split: false, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::types::{GlobalSecondaryIndexDescription, KeySchemaElement, KeyType, LocalSecondaryIndexDescription, ProjectionType};

    fn key(name: &str, t: KeyType) -> KeySchemaElement {
        KeySchemaElement::builder().attribute_name(name).key_type(t).build().unwrap()
    }

    #[test]
    fn keys_gsis_and_lsis() {
        let d = TableDescription::builder()
            .key_schema(key("pk", KeyType::Hash))
            .key_schema(key("sk", KeyType::Range))
            .table_size_bytes(4096)
            .global_secondary_indexes(
                GlobalSecondaryIndexDescription::builder()
                    .index_name("by_cliente")
                    .key_schema(key("fecha", KeyType::Range))
                    .key_schema(key("cliente", KeyType::Hash))
                    .projection(Projection::builder().projection_type(ProjectionType::Include).non_key_attributes("total").build())
                    .index_size_bytes(1500)
                    .build(),
            )
            .local_secondary_indexes(
                LocalSecondaryIndexDescription::builder()
                    .index_name("by_fecha")
                    .key_schema(key("pk", KeyType::Hash))
                    .key_schema(key("fecha", KeyType::Range))
                    .projection(Projection::builder().projection_type(ProjectionType::KeysOnly).build())
                    .build(),
            )
            .build();
        let r = report(&d).derived();
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["PRIMARY", "by_cliente", "by_fecha"]);
        assert!(r.indexes[0].primary_key);
        assert_eq!(r.indexes[0].key_columns, ["pk", "sk"]);
        assert_eq!(r.indexes[0].size_kb, Some(4));
        assert_eq!(r.indexes[1].key_columns, ["cliente", "fecha"]);
        assert_eq!(r.indexes[1].kind, "GSI INCLUDE");
        assert_eq!(r.indexes[1].included_columns, ["total"]);
        assert_eq!(r.indexes[1].size_kb, Some(2));
        assert_eq!(r.indexes[2].kind, "LSI KEYS_ONLY");
        assert!(!r.stats_available);
    }
}

//! A table's indexes (`Session::index_usage`): Cassandra, ScyllaDB and
//! Amazon Keyspaces count no reads or writes per index, so the report
//! lists them without counters (`stats_available` false, with a note).
//!
//! - The primary key comes first, as the index it is (partition key, then
//!   the clustering columns, the descending ones with ` DESC`).
//! - The secondary indexes: `system_schema.indexes` (`target` is the
//!   column, or `keys(…)`, `values(…)`, `entries(…)`, `full(…)`; the kind
//!   is the class for custom ones: SAI, SASI). Keyspaces has no secondary
//!   indexes.
//! - CQL has no foreign keys.

use crate::{map_value, text, CassandraSession, Column};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};

/// One `system_schema.indexes` row: name, kind (`COMPOSITES`, `KEYS`,
/// `CUSTOM`), target, class name.
pub(crate) type IndexRow = (String, String, String, Option<String>);

pub(crate) const NOTE: &str = "Cassandra y ScyllaDB no cuentan el uso de cada índice: se listan sin contadores.";

/// A custom index's class as its kind (SAI, SASI or the class's short name).
fn kind_of(kind: &str, class: Option<&str>) -> String {
    match class {
        Some(c) if c.eq_ignore_ascii_case("sai") || c.ends_with("StorageAttachedIndex") => "SAI".into(),
        Some(c) if c.ends_with("SASIIndex") => "SASI".into(),
        Some(c) => c.rsplit('.').next().unwrap_or(c).to_string(),
        None if kind.eq_ignore_ascii_case("custom") => "CUSTOM".into(),
        None => "SECONDARY".into(),
    }
}

pub(crate) fn assemble(columns: &[Column], indexes: &[IndexRow]) -> Vec<IndexUsage> {
    let mut out = Vec::new();
    let key: Vec<String> = columns
        .iter()
        .filter(|c| c.kind == "partition_key" || c.kind == "clustering")
        .map(|c| if c.desc { format!("{} DESC", c.name) } else { c.name.clone() })
        .collect();
    if !key.is_empty() {
        out.push(IndexUsage { name: "PRIMARY KEY".into(), kind: "PRIMARY KEY".into(), unique: true, primary_key: true, key_columns: key, ..Default::default() });
    }
    for (name, kind, target, class) in indexes {
        out.push(IndexUsage {
            name: name.clone(),
            kind: kind_of(kind, class.as_deref()),
            key_columns: vec![target.trim_matches('"').to_string()],
            ..Default::default()
        });
    }
    out
}

impl CassandraSession {
    pub(crate) async fn index_usage_report(&self, obj: &ObjectRef) -> Result<Option<IndexUsageReport>> {
        let ks = self.ks(obj)?;
        let columns = self.table_columns(&ks, &obj.name).await?;
        if columns.is_empty() {
            return Ok(None);
        }
        // Keyspaces has no system_schema.indexes rows (nor secondary indexes).
        let indexes: Vec<IndexRow> = self
            .rows("SELECT index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = ? AND table_name = ?", (&ks, &obj.name))
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| (text(r, 0), text(r, 1), map_value(r, 2, "target").unwrap_or_default(), map_value(r, 2, "class_name")))
            .collect();
        Ok(Some(IndexUsageReport {
            stats_available: false,
            note: Some(NOTE.into()),
            indexes: assemble(&columns, &indexes),
            seek_scan_split: false,
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, kind: &str, position: i64, desc: bool) -> Column {
        Column { name: name.into(), typ: "int".into(), kind: kind.into(), position, desc }
    }

    #[test]
    fn primary_key_then_secondary() {
        let cols = vec![col("id", "partition_key", 0, false), col("linea", "clustering", 0, true), col("cliente", "regular", -1, false)];
        let ix = vec![
            ("ix_cliente".to_string(), "COMPOSITES".to_string(), "cliente".to_string(), None),
            ("ix_sai".to_string(), "CUSTOM".to_string(), "\"Fecha\"".to_string(), Some("StorageAttachedIndex".to_string())),
            ("ix_sasi".to_string(), "CUSTOM".to_string(), "x".to_string(), Some("org.apache.cassandra.index.sasi.SASIIndex".to_string())),
            ("ix_keys".to_string(), "KEYS".to_string(), "keys(m)".to_string(), None),
        ];
        let r = IndexUsageReport { indexes: assemble(&cols, &ix), note: Some(NOTE.into()), seek_scan_split: false, ..Default::default() }.derived();
        assert_eq!(r.indexes[0].name, "PRIMARY KEY");
        assert!(r.indexes[0].primary_key && r.indexes[0].unique);
        assert_eq!(r.indexes[0].key_columns, ["id", "linea DESC"]);
        let kinds: Vec<&str> = r.indexes.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, ["PRIMARY KEY", "SECONDARY", "SAI", "SASI", "SECONDARY"]);
        assert_eq!(r.indexes[2].key_columns, ["Fecha"]);
        assert_eq!(r.indexes[4].key_columns, ["keys(m)"]);
        assert!(r.indexes.iter().all(|i| !i.unused && i.read_share.is_none()));
    }
}

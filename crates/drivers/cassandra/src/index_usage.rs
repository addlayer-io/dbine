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

use crate::{map_entries, map_value, text, CassandraSession, Column};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};
use std::collections::BTreeMap;

/// One `system_schema.indexes` row: name, kind (`COMPOSITES`, `KEYS`,
/// `CUSTOM`), target, class name, and the rest of its options.
pub(crate) type IndexRow = (
    String,
    String,
    String,
    Option<String>,
    BTreeMap<String, String>,
);

pub(crate) const NOTE: &str =
    "Cassandra y ScyllaDB no cuentan el uso de cada índice: se listan sin contadores.";

/// A custom index's class as its kind (SAI, SASI or the class's short name).
fn kind_of(kind: &str, class: Option<&str>) -> String {
    match class {
        Some(c) if c.eq_ignore_ascii_case("sai") || c.ends_with("StorageAttachedIndex") => {
            "SAI".into()
        }
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
        .map(|c| {
            if c.desc {
                format!("{} DESC", c.name)
            } else {
                c.name.clone()
            }
        })
        .collect();
    if !key.is_empty() {
        out.push(IndexUsage {
            name: "PRIMARY KEY".into(),
            kind: "PRIMARY KEY".into(),
            unique: true,
            primary_key: true,
            key_columns: key,
            ..Default::default()
        });
    }
    for (name, kind, target, class, options) in indexes {
        // A ScyllaDB vector index with filter columns: the vector column,
        // then them.
        let key_columns = match crate::json_target(target) {
            Some((columns, _)) => columns,
            None => vec![target.trim_matches('"').to_string()],
        };
        // A vector index's settings, as the table's schema reads them.
        let (vector, _) = crate::index_vector(
            crate::index_kind(class.as_deref()).as_deref(),
            target,
            options.clone(),
            |c| {
                columns
                    .iter()
                    .any(|x| x.name == c && x.typ.trim().starts_with("vector<"))
            },
        );
        out.push(IndexUsage {
            name: name.clone(),
            kind: kind_of(kind, class.as_deref()),
            key_columns,
            vector,
            ..Default::default()
        });
    }
    out
}

impl CassandraSession {
    pub(crate) async fn index_usage_report(
        &self,
        obj: &ObjectRef,
    ) -> Result<Option<IndexUsageReport>> {
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
            .map(|r| {
                let options = map_entries(r, 2)
                    .into_iter()
                    .filter(|(k, _)| k != "target" && k != "class_name")
                    .collect();
                (text(r, 0), text(r, 1), map_value(r, 2, "target").unwrap_or_default(), map_value(r, 2, "class_name"), options)
            })
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
        Column {
            name: name.into(),
            typ: "int".into(),
            kind: kind.into(),
            position,
            desc,
        }
    }

    #[test]
    fn primary_key_then_secondary() {
        let cols = vec![
            col("id", "partition_key", 0, false),
            col("linea", "clustering", 0, true),
            col("cliente", "regular", -1, false),
        ];
        let ix = vec![
            (
                "ix_cliente".to_string(),
                "COMPOSITES".to_string(),
                "cliente".to_string(),
                None,
                BTreeMap::new(),
            ),
            (
                "ix_sai".to_string(),
                "CUSTOM".to_string(),
                "\"Fecha\"".to_string(),
                Some("StorageAttachedIndex".to_string()),
                BTreeMap::new(),
            ),
            (
                "ix_sasi".to_string(),
                "CUSTOM".to_string(),
                "x".to_string(),
                Some("org.apache.cassandra.index.sasi.SASIIndex".to_string()),
                BTreeMap::new(),
            ),
            (
                "ix_keys".to_string(),
                "KEYS".to_string(),
                "keys(m)".to_string(),
                None,
                BTreeMap::new(),
            ),
        ];
        let r = IndexUsageReport {
            indexes: assemble(&cols, &ix),
            note: Some(NOTE.into()),
            seek_scan_split: false,
            ..Default::default()
        }
        .derived();
        assert_eq!(r.indexes[0].name, "PRIMARY KEY");
        assert!(r.indexes[0].primary_key && r.indexes[0].unique);
        assert_eq!(r.indexes[0].key_columns, ["id", "linea DESC"]);
        let kinds: Vec<&str> = r.indexes.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["PRIMARY KEY", "SECONDARY", "SAI", "SASI", "SECONDARY"]
        );
        assert_eq!(r.indexes[2].key_columns, ["Fecha"]);
        assert_eq!(r.indexes[4].key_columns, ["keys(m)"]);
        assert!(r
            .indexes
            .iter()
            .all(|i| !i.unused && i.read_share.is_none()));
    }

    /// Vector indexes carry their settings, as the schema reads them: an
    /// SAI index on a vector column (Cassandra), and a ScyllaDB
    /// vector_index with filter and partition columns (its JSON target).
    #[test]
    fn vector_indexes_carry_their_settings() {
        use dbine_driver::vector::{metric, param};
        let mut cols = vec![
            col("id", "partition_key", 0, false),
            col("lang", "regular", -1, false),
            col("emb", "regular", -1, false),
        ];
        cols[2].typ = "vector<float, 3>".into();
        let opts = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let ix = vec![
            (
                "ix_emb".to_string(),
                "CUSTOM".to_string(),
                "emb".to_string(),
                Some("org.apache.cassandra.index.sai.StorageAttachedIndex".to_string()),
                opts(&[
                    ("similarity_function", "DOT_PRODUCT"),
                    ("maximum_node_connections", "32"),
                ]),
            ),
            (
                "ix_scylla".to_string(),
                "CUSTOM".to_string(),
                r#"{"tc":"emb","fc":["lang"],"pk":["id"]}"#.to_string(),
                Some("vector_index".to_string()),
                opts(&[
                    ("similarity_function", "euclidean"),
                    ("quantization", "i8"),
                    ("index_version", "x"),
                ]),
            ),
            // SAI on a text column: no vector.
            (
                "ix_lang".to_string(),
                "CUSTOM".to_string(),
                "lang".to_string(),
                Some("StorageAttachedIndex".to_string()),
                opts(&[("case_sensitive", "false")]),
            ),
        ];
        let r = assemble(&cols, &ix);
        let sai = r[1].vector.as_ref().unwrap();
        assert_eq!(sai.metric.as_deref(), Some(metric::IP));
        assert_eq!(sai.params.get(param::M).map(String::as_str), Some("32"));
        let scylla = &r[2];
        assert_eq!(scylla.key_columns, ["emb", "lang"]);
        let v = scylla.vector.as_ref().unwrap();
        assert_eq!(v.metric.as_deref(), Some(metric::L2));
        assert_eq!(
            v.params.get(param::QUANTIZATION).map(String::as_str),
            Some("i8")
        );
        assert!(!v.params.contains_key("pk") && !v.params.contains_key("index_version"));
        assert!(r[3].vector.is_none() && r[0].vector.is_none());
    }
}

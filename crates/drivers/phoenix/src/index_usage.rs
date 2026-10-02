//! A table's indexes (`Session::index_usage`).
//!
//! - The entries come from the same `SYSTEM.CATALOG` read as the schema
//!   compare: the primary key (the HBase row key) and the secondary
//!   indexes, `GLOBAL` (their own table) or `LOCAL` (in the data table's
//!   regions), so each one has the same name here as in the drop script
//!   "Eliminar índice" generates. A covered column (`INCLUDE`) is an index
//!   column without a key position.
//! - Phoenix counts nothing per index and has no foreign keys; an index's
//!   size is in HBase, not in the catalog. So `stats_available` is false
//!   and the note says so.
//!
//! The generic Avatica preset has no index metadata in its protocol
//! (`supports_index_usage` is false).

use dbine_driver::{IndexUsage, IndexUsageReport, TableSchema};
use serde_json::Value;
use std::collections::BTreeMap;

pub const NOTE: &str = "Phoenix no registra cuántas veces se usa cada índice ni su tamaño (vive en HBase): se listan la clave de fila y los índices con sus columnas, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución.";

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

/// Each index's covered columns, by (schema, index name), from the rows of
/// `ddl::CATALOG_QUERY`: index columns without `KEY_SEQ`, in ordinal order,
/// without their `FAMILY:` prefix.
pub fn covered(rows: &[Vec<Value>]) -> BTreeMap<(String, String), Vec<String>> {
    let kinds: BTreeMap<(String, String), String> =
        rows.iter().filter(|r| text(r.get(4)).is_empty()).map(|r| ((text(r.first()), text(r.get(1))), text(r.get(2)))).collect();
    let mut out: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    for r in rows {
        let key = (text(r.first()), text(r.get(1)));
        let name = text(r.get(4));
        if name.is_empty() || kinds.get(&key).map(String::as_str) != Some("i") || r.get(9).and_then(Value::as_i64).is_some() {
            continue;
        }
        let ord = r.get(11).and_then(Value::as_i64).unwrap_or(0);
        out.entry(key).or_default().push((ord, name.rsplit_once(':').map_or(name.clone(), |(_, c)| c.to_string())));
    }
    out.into_iter()
        .map(|(k, mut v)| {
            v.sort();
            (k, v.into_iter().map(|(_, c)| c).collect())
        })
        .collect()
}

/// The report for `t` (`None`: the table isn't there): the row key first,
/// then the indexes in the schema's order.
pub fn report(t: Option<&TableSchema>, covered: &BTreeMap<(String, String), Vec<String>>) -> IndexUsageReport {
    let Some(t) = t else { return IndexUsageReport { note: Some(NOTE.into()), writes_counted: false, ..Default::default() } };
    let schema = t.schema.clone().unwrap_or_default();
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        indexes.push(IndexUsage {
            name: pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into()),
            kind: "ROW KEY".into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            ..Default::default()
        });
    }
    indexes.extend(t.indexes.iter().map(|ix| IndexUsage {
        name: ix.name.clone(),
        kind: ix.kind.clone().unwrap_or_default().to_uppercase(),
        key_columns: ix.columns.clone(),
        included_columns: covered.get(&(schema.clone(), ix.name.clone())).cloned().unwrap_or_default(),
        ..Default::default()
    }));
    IndexUsageReport { note: Some(NOTE.into()), indexes, writes_counted: false, ..Default::default() }.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::from_catalog;
    use serde_json::{json, Value};

    /// A `CATALOG_QUERY` row: schema, name, type, family, column, type name,
    /// size, digits, nullable, key seq, default, ordinal, pk name, data table, index type.
    #[allow(clippy::too_many_arguments)]
    fn row(t: &str, ty: &str, col: Option<&str>, key: Option<i64>, ord: i64, pk: &str, data: &str, ix_type: Option<i64>) -> Vec<Value> {
        vec![
            json!("S"),
            json!(t),
            json!(ty),
            Value::Null,
            col.map_or(Value::Null, |c| json!(c)),
            json!("INTEGER"),
            Value::Null,
            Value::Null,
            json!(1),
            key.map_or(Value::Null, |k| json!(k)),
            Value::Null,
            json!(ord),
            json!(pk),
            json!(data),
            ix_type.map_or(Value::Null, |k| json!(k)),
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    }

    #[test]
    fn row_key_indexes_and_covered_columns() {
        let rows = vec![
            row("T", "u", None, None, 0, "PK_T", "", None),
            row("T", "u", Some("ID"), Some(1), 1, "PK_T", "", None),
            row("T", "u", Some("A"), None, 2, "PK_T", "", None),
            row("T", "u", Some("B"), None, 3, "PK_T", "", None),
            row("IX_A", "i", None, None, 0, "", "T", Some(1)),
            row("IX_A", "i", Some("0:A"), Some(1), 1, "", "T", Some(1)),
            row("IX_A", "i", Some(":ID"), Some(2), 2, "", "T", Some(1)),
            row("IX_A", "i", Some("0:B"), None, 3, "", "T", Some(1)),
            row("IX_B", "i", None, None, 0, "", "T", Some(2)),
            row("IX_B", "i", Some("_INDEX_ID"), Some(1), 1, "", "T", Some(2)),
            [row("IX_B", "i", Some("0:B"), Some(2), 2, "", "T", Some(2)), vec![json!(1)]].concat(),
            row("IX_B", "i", Some(":ID"), Some(3), 3, "", "T", Some(2)),
        ];
        let tables = from_catalog(rows.clone());
        let r = report(tables.iter().find(|t| t.name == "T"), &covered(&rows));
        assert!(!r.stats_available && r.note.is_some() && r.foreign_keys.is_empty());
        let got: Vec<(&str, &str)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str())).collect();
        assert_eq!(got, [("PK_T", "ROW KEY"), ("IX_A", "GLOBAL"), ("IX_B", "LOCAL")]);
        assert!(r.indexes[0].primary_key);
        assert_eq!(r.indexes[1].key_columns, ["A"]);
        assert_eq!(r.indexes[1].included_columns, ["B"]);
        assert_eq!(r.indexes[2].key_columns, ["B DESC"], "SORT_ORDER 1 is DESC");
        assert!(r.indexes[2].included_columns.is_empty());
        assert!(report(None, &BTreeMap::new()).indexes.is_empty());
    }
}

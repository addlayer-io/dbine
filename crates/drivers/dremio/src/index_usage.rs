//! Dremio has no indexes; its reflections are the closest thing (a copy of
//! the table, raw or aggregated, sorted and partitioned, that the planner
//! picks instead of the table). `Session::index_usage` lists a table's
//! reflections as its indexes, and `database_schema` keeps them in
//! `TableSchema::indexes` so the schema sync can drop and create them
//! (`ALTER TABLE … DROP REFLECTION` / `CREATE RAW | AGGREGATE REFLECTION`).
//!
//! From `sys.reflections`:
//! - kind: `RAW REFLECTION` or `AGGREGATION REFLECTION`;
//! - key columns: the display columns (raw) or the dimensions
//!   (aggregation); included columns: the measures;
//! - size: `current_footprint_bytes`;
//! - reads (`seeks`): `accelerated_count`, the queries the reflection
//!   answered. Dremio has one counter (no seeks against scans: the report
//!   says `seek_scan_split: false`), no write counter (a reflection's
//!   refreshes aren't counted: `writes_counted: false`, so there's no
//!   writes per read and none is "sin uso") and doesn't say since when it
//!   counts;
//! - last write: `last_refresh_from_table`, when it last refreshed.
//!
//! No keys: Dremio has no primary or foreign keys to read.

use dbine_driver::{IndexDef, IndexUsage, IndexUsageReport, TableSchema};
use serde_json::{Map, Value};

pub const SQL: &str = "SELECT reflection_name, type, dataset_name, display_columns, dimensions, measures, sort_columns,
       partition_columns, distribution_columns, current_footprint_bytes, accelerated_count, last_refresh_from_table, status
  FROM sys.reflections";

pub const RAW: &str = "RAW";
pub const AGGREGATION: &str = "AGGREGATION";
pub const PARTITION_BY: &str = "partition_by";
pub const LOCALSORT_BY: &str = "localsort_by";
pub const DISTRIBUTE_BY: &str = "distribute_by";

pub const NOTE: &str = "Dremio no tiene índices: se listan las reflexiones de la tabla. Las lecturas son las consultas que cada reflexión aceleró (sys.reflections); Dremio no cuenta escrituras ni dice desde cuándo cuenta.";

type Row = Map<String, Value>;

fn s(r: &Row, k: &str) -> String {
    match r.get(k) {
        Some(Value::String(v)) => v.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn n(r: &Row, k: &str) -> u64 {
    r.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())).unwrap_or(0)
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect()
}

/// `"$scratch".t`, `space.folder."my t"` → its parts.
pub fn dataset_path(name: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Whether a reflection row is on `schema.table` (Dremio's names don't
/// care about case).
pub fn on_table(r: &Row, schema: Option<&str>, table: &str) -> bool {
    let want: Vec<&str> = schema.filter(|s| !s.is_empty()).map(|s| s.split('.').collect::<Vec<_>>()).unwrap_or_default().into_iter().chain([table]).collect();
    let got = dataset_path(&s(r, "dataset_name"));
    got.len() == want.len() && got.iter().zip(&want).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// The reflection as an index of the table (for the schema sync).
pub fn index_def(r: &Row) -> IndexDef {
    let raw = s(r, "type") == RAW;
    let mut ix = IndexDef {
        name: s(r, "reflection_name"),
        columns: list(&s(r, if raw { "display_columns" } else { "dimensions" })),
        include: if raw { Vec::new() } else { list(&s(r, "measures")) },
        kind: Some(if raw { RAW } else { AGGREGATION }.into()),
        ..Default::default()
    };
    for (k, col) in [(PARTITION_BY, "partition_columns"), (LOCALSORT_BY, "sort_columns"), (DISTRIBUTE_BY, "distribution_columns")] {
        let v = list(&s(r, col));
        if !v.is_empty() {
            ix.options.insert(k.into(), v.join(", "));
        }
    }
    ix
}

pub fn usage(r: &Row) -> IndexUsage {
    let def = index_def(r);
    let status = s(r, "status");
    let mut kind = format!("{} REFLECTION", def.kind.as_deref().unwrap_or(RAW));
    if !status.is_empty() && status != "CAN_ACCELERATE" {
        kind.push_str(&format!(" ({status})"));
    }
    IndexUsage {
        name: def.name,
        kind,
        key_columns: def.columns,
        included_columns: def.include,
        size_kb: r.get("current_footprint_bytes").filter(|v| !v.is_null()).map(|_| n(r, "current_footprint_bytes").div_ceil(1024)),
        seeks: n(r, "accelerated_count"),
        last_write: Some(s(r, "last_refresh_from_table")).filter(|v| !v.is_empty()).map(|v| v.chars().take(19).collect()),
        ..Default::default()
    }
}

pub fn report(rows: &[Row], schema: Option<&str>, table: &str) -> IndexUsageReport {
    let mut indexes: Vec<IndexUsage> = rows.iter().filter(|r| on_table(r, schema, table)).map(usage).collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    IndexUsageReport { since: None, stats_available: true, note: Some(NOTE.into()), indexes, foreign_keys: Vec::new(), seek_scan_split: false, writes_counted: false }
}

/// Each table's reflections into its `indexes`.
pub fn attach(tables: &mut [TableSchema], rows: &[Row]) {
    for t in tables {
        let mut ixs: Vec<IndexDef> = rows.iter().filter(|r| on_table(r, t.schema.as_deref(), &t.name)).map(index_def).collect();
        ixs.sort_by(|a, b| a.name.cmp(&b.name));
        t.indexes = ixs;
    }
}

/// `CREATE … REFLECTION` for an index of `table` (already quoted).
pub fn create(table: &str, ix: &IndexDef) -> String {
    let q = |cols: &[String]| cols.iter().map(|c| crate::ddl::q(c)).collect::<Vec<_>>().join(", ");
    let opt = |k: &str| ix.options.get(k).map(|v| list(v)).filter(|v| !v.is_empty());
    let mut sql = if ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(AGGREGATION)) {
        let mut s = format!("ALTER TABLE {table} CREATE AGGREGATE REFLECTION {} USING DIMENSIONS ({})", crate::ddl::q(&ix.name), q(&ix.columns));
        if !ix.include.is_empty() {
            s.push_str(&format!(" MEASURES ({})", q(&ix.include)));
        }
        s
    } else {
        format!("ALTER TABLE {table} CREATE RAW REFLECTION {} USING DISPLAY ({})", crate::ddl::q(&ix.name), q(&ix.columns))
    };
    for (k, word) in [(PARTITION_BY, "PARTITION BY"), (LOCALSORT_BY, "LOCALSORT BY"), (DISTRIBUTE_BY, "DISTRIBUTE BY")] {
        if let Some(v) = opt(k) {
            sql.push_str(&format!(" {word} ({})", q(&v)));
        }
    }
    sql.push(';');
    sql
}

pub fn drop(table: &str, ix: &IndexDef) -> String {
    format!("ALTER TABLE {table} DROP REFLECTION {};", crate::ddl::q(&ix.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    fn rows() -> Vec<Row> {
        vec![
            row(json!({"reflection_name": "r_used", "type": "RAW", "dataset_name": "\"$scratch\".iu_probe", "display_columns": "id, code, note",
                "dimensions": "", "measures": "", "sort_columns": "id", "partition_columns": "code", "distribution_columns": "",
                "current_footprint_bytes": 8205, "accelerated_count": 2, "last_refresh_from_table": "2026-10-01 23:09:12.406", "status": "CAN_ACCELERATE"})),
            row(json!({"reflection_name": "r_agg", "type": "AGGREGATION", "dataset_name": "\"$scratch\".IU_PROBE", "display_columns": "",
                "dimensions": "code", "measures": "id", "sort_columns": "", "partition_columns": "", "distribution_columns": "",
                "current_footprint_bytes": null, "accelerated_count": 0, "last_refresh_from_table": null, "status": "DISABLED"})),
            row(json!({"reflection_name": "other", "type": "RAW", "dataset_name": "sp.\"my.f\".iu_probe", "display_columns": "a",
                "accelerated_count": 9, "status": "CAN_ACCELERATE"})),
        ]
    }

    #[test]
    fn paths() {
        assert_eq!(dataset_path("\"$scratch\".t"), vec!["$scratch", "t"]);
        assert_eq!(dataset_path("sp.\"my.f\".\"a\"\"b\""), vec!["sp", "my.f", "a\"b"]);
    }

    #[test]
    fn reflections_as_indexes() {
        let r = report(&rows(), Some("$scratch"), "iu_probe").derived();
        assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["r_agg", "r_used"]);
        let (agg, raw) = (&r.indexes[0], &r.indexes[1]);
        assert_eq!((raw.kind.as_str(), raw.key_columns.len(), raw.size_kb, raw.seeks), ("RAW REFLECTION", 3, Some(9), 2));
        assert_eq!(raw.last_write.as_deref(), Some("2026-10-01 23:09:12"));
        assert_eq!((agg.kind.as_str(), agg.key_columns.clone(), agg.included_columns.clone(), agg.size_kb), ("AGGREGATION REFLECTION (DISABLED)", vec!["code".to_string()], vec!["id".to_string()], None));
        // One counter: a share, but no seek health, and nothing "unused" (no writes counted).
        assert_eq!((raw.read_share, agg.read_share), (Some(1.0), Some(0.0)));
        assert!(!r.writes_counted);
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none() && !i.unused && i.writes_per_read.is_none()));
        assert!(report(&rows(), Some("sp.my.f"), "iu_probe").indexes.is_empty(), "a dotted folder name is one part");
    }

    #[test]
    fn create_and_drop_statements() {
        let mut t = TableSchema { schema: Some("$scratch".into()), name: "iu_probe".into(), ..Default::default() };
        attach(std::slice::from_mut(&mut t), &rows());
        let table = crate::ddl::path(t.schema.as_deref(), &t.name);
        assert_eq!(
            create(&table, &t.indexes[1]),
            "ALTER TABLE \"$scratch\".\"iu_probe\" CREATE RAW REFLECTION \"r_used\" USING DISPLAY (\"id\", \"code\", \"note\") PARTITION BY (\"code\") LOCALSORT BY (\"id\");"
        );
        assert_eq!(
            create(&table, &t.indexes[0]),
            "ALTER TABLE \"$scratch\".\"iu_probe\" CREATE AGGREGATE REFLECTION \"r_agg\" USING DIMENSIONS (\"code\") MEASURES (\"id\");"
        );
        assert_eq!(drop(&table, &t.indexes[0]), "ALTER TABLE \"$scratch\".\"iu_probe\" DROP REFLECTION \"r_agg\";");
    }
}

//! A table's indexes and keys (`Session::index_usage`).
//!
//! Flight SQL has commands for a table's primary key (`GetPrimaryKeys`) and
//! foreign keys (`GetImportedKeys`) but none for indexes, and no usage
//! counters. So:
//!
//! - DuckDB behind the server (GizmoSQL): its own catalog functions over
//!   SQL, like the DuckDB driver: `duckdb_constraints()` (primary key,
//!   UNIQUE constraints, foreign keys) and `duckdb_indexes()` (the ART
//!   indexes). DuckDB counts nothing per index.
//! - Any other engine (or DuckDB's functions refused): the key and the
//!   foreign keys from the Flight SQL commands, where the server answers
//!   them. Dremio and DataFusion (InfluxDB 3) have no indexes.
//!
//! `stats_available` is false and the note says why. Flight SQL has no
//! schema sync, so "Eliminar índice" isn't offered; `DROP INDEX` runs as
//! any statement.

use crate::{cells, text, Engine, FlightSession};
use arrow_flight::sql::{CommandGetImportedKeys, CommandGetPrimaryKeys};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result};
use serde_json::Value;
use std::collections::HashMap;

pub const NOTE_DUCKDB: &str = "DuckDB no registra cuántas veces se usa cada índice ni su tamaño: se listan los índices ART con sus columnas, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución (INDEX_SCAN).";
pub const NOTE_OTHER: &str = "Flight SQL informa la clave primaria y las claves foráneas de la tabla, pero no sus índices ni cuántas veces se usan.";

type Row = HashMap<String, Value>;

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn get(r: &Row, k: &str) -> String {
    r.get(k).map(text).unwrap_or_default()
}

/// DuckDB's constraints of the table (primary key, UNIQUE, foreign keys)
/// and its other indexes. `catalog`: the session's database, if any.
pub(crate) fn duckdb_sql(catalog: Option<&str>, schema: &str, table: &str) -> (String, String) {
    let mut filter = format!("schema_name = {} AND table_name = {}", lit(schema), lit(table));
    if let Some(c) = catalog.filter(|c| !c.is_empty()) {
        filter.push_str(&format!(" AND database_name = {}", lit(c)));
    }
    (
        format!(
            "SELECT constraint_type, constraint_name, array_to_string(constraint_column_names, chr(31)) AS cols, referenced_table,
                    array_to_string(referenced_column_names, chr(31)) AS ref_cols
             FROM duckdb_constraints()
             WHERE {filter} AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')
             ORDER BY constraint_index"
        ),
        format!("SELECT index_name, is_unique, sql FROM duckdb_indexes() WHERE {filter} AND NOT is_primary ORDER BY index_name"),
    )
}

fn list(s: &str) -> Vec<String> {
    s.split('\u{1f}').filter(|c| !c.is_empty()).map(str::to_string).collect()
}

/// The keys of `CREATE INDEX … ON t(a, "b c", (lower(d)))`.
pub(crate) fn index_keys(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let Some(on) = upper.find(" ON ") else { return Vec::new() };
    let Some(open) = sql[on..].find('(').map(|i| on + i) else { return Vec::new() };
    let Some(close) = sql.rfind(')').filter(|c| *c > open) else { return Vec::new() };
    let (mut out, mut depth, mut cur, mut quoted) = (Vec::new(), 0, String::new(), false);
    for ch in sql[open + 1..close].chars() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter()
        .map(|k| {
            let k = k.trim();
            match k.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                Some(n) if !n.replace("\"\"", "").contains('"') => n.replace("\"\"", "\""),
                _ => k.to_string(),
            }
        })
        .filter(|k| !k.is_empty())
        .collect()
}

/// The report from DuckDB's constraint and index rows.
pub(crate) fn from_duckdb(schema: &str, constraints: &[Row], indexes: &[Row]) -> IndexUsageReport {
    let mut out = Vec::new();
    let mut foreign_keys = Vec::new();
    for r in constraints {
        let cols = list(&get(r, "cols"));
        match get(r, "constraint_type").as_str() {
            "PRIMARY KEY" => out.insert(
                0,
                IndexUsage { name: "PRIMARY KEY".into(), kind: "ART".into(), unique: true, primary_key: true, key_columns: cols, ..Default::default() },
            ),
            "UNIQUE" => out.push(IndexUsage { name: get(r, "constraint_name"), kind: "ART".into(), unique: true, key_columns: cols, ..Default::default() }),
            _ => foreign_keys.push(ForeignKeyDef {
                columns: cols,
                ref_schema: Some(schema.to_string()),
                ref_table: get(r, "referenced_table"),
                ref_columns: list(&get(r, "ref_cols")),
                ..Default::default()
            }),
        }
    }
    for r in indexes {
        let unique = matches!(r.get("is_unique"), Some(Value::Bool(true))) || get(r, "is_unique") == "true";
        out.push(IndexUsage { name: get(r, "index_name"), kind: "ART".into(), unique, key_columns: index_keys(&get(r, "sql")), ..Default::default() });
    }
    IndexUsageReport { note: Some(NOTE_DUCKDB.into()), indexes: out, foreign_keys, ..Default::default() }.derived()
}

/// The report from `GetPrimaryKeys` and `GetImportedKeys` rows.
pub(crate) fn from_commands(pk: &[Row], imported: &[Row]) -> IndexUsageReport {
    let mut indexes = Vec::new();
    let mut key: Vec<(i64, String)> = pk.iter().map(|r| (get(r, "key_sequence").parse().unwrap_or(0), get(r, "column_name"))).collect();
    if !key.is_empty() {
        key.sort();
        let name = pk.iter().map(|r| get(r, "key_name")).find(|n| !n.is_empty()).unwrap_or_else(|| "PRIMARY KEY".into());
        indexes.push(IndexUsage {
            name,
            kind: "PRIMARY KEY".into(),
            unique: true,
            primary_key: true,
            key_columns: key.into_iter().map(|(_, c)| c).collect(),
            ..Default::default()
        });
    }
    // One row per column pair; grouped by key name (or referenced table).
    let mut rows: Vec<&Row> = imported.iter().collect();
    rows.sort_by_key(|r| (get(r, "fk_key_name"), get(r, "pk_table_name"), get(r, "key_sequence").parse::<i64>().unwrap_or(0)));
    let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
    for r in rows {
        let name = Some(get(r, "fk_key_name")).filter(|n| !n.is_empty());
        let ref_table = get(r, "pk_table_name");
        match foreign_keys.last_mut().filter(|f| f.name == name && f.ref_table == ref_table && name.is_some()) {
            Some(f) => {
                f.columns.push(get(r, "fk_column_name"));
                f.ref_columns.push(get(r, "pk_column_name"));
            }
            None => foreign_keys.push(ForeignKeyDef {
                name,
                columns: vec![get(r, "fk_column_name")],
                ref_schema: Some(get(r, "pk_db_schema_name")).filter(|s| !s.is_empty()),
                ref_table,
                ref_columns: vec![get(r, "pk_column_name")],
                ..Default::default()
            }),
        }
    }
    IndexUsageReport { note: Some(NOTE_OTHER.into()), indexes, foreign_keys, ..Default::default() }.derived()
}

impl FlightSession {
    async fn maps(&self, sql: &str) -> Result<Vec<Row>> {
        let (cols, rows) = self.rows(sql).await?;
        Ok(rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect())
    }

    async fn info_rows(&self, info: Result<arrow_flight::FlightInfo>) -> Vec<Row> {
        let Ok(info) = info else { return Vec::new() };
        let Ok(batches) = self.batches(info).await else { return Vec::new() };
        let mut out = Vec::new();
        for b in batches {
            let names: Vec<String> = b.schema().fields().iter().map(|f| f.name().clone()).collect();
            for r in 0..b.num_rows() {
                out.push(names.iter().cloned().zip(b.columns().iter().map(|a| cells::cell(a.as_ref(), r))).collect());
            }
        }
        out
    }

    pub(crate) async fn index_report(&self, table: &ObjectRef) -> Result<IndexUsageReport> {
        if self.server.engine() == Engine::DuckDb {
            let schema = table.schema().unwrap_or("main");
            let (constraints, indexes) = duckdb_sql(self.catalog.as_deref(), schema, &table.name);
            if let (Ok(c), Ok(i)) = (self.maps(&constraints).await, self.maps(&indexes).await) {
                return Ok(from_duckdb(schema, &c, &i));
            }
        }
        let (catalog, db_schema, name) = (self.catalog.clone(), table.schema().map(str::to_string), table.name.clone());
        let mut c = self.conn.client();
        let pk = self
            .cancel
            .run(async { c.get_primary_keys(CommandGetPrimaryKeys { catalog: catalog.clone(), db_schema: db_schema.clone(), table: name.clone() }).await.map_err(crate::flight_error) })
            .await;
        let pk = self.info_rows(pk).await;
        let mut c = self.conn.client();
        let fk = self
            .cancel
            .run(async { c.get_imported_keys(CommandGetImportedKeys { catalog, db_schema, table: name }).await.map_err(crate::flight_error) })
            .await;
        let fk = self.info_rows(fk).await;
        Ok(from_commands(&pk, &fk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(pairs: &[(&str, Value)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn duckdb_queries_and_keys() {
        let (c, i) = duckdb_sql(Some("db"), "main", "o'k");
        assert!(c.contains("schema_name = 'main' AND table_name = 'o''k' AND database_name = 'db'"), "{c}");
        assert!(i.contains("FROM duckdb_indexes()") && i.contains("NOT is_primary"), "{i}");
        assert_eq!(index_keys("CREATE INDEX ix ON t(a, \"b c\", (lower(d)));"), ["a", "b c", "(lower(d))"]);
        assert_eq!(index_keys("CREATE INDEX ix ON \"main\".\"t\" (\"a\")"), ["a"]);
    }

    #[test]
    fn duckdb_rows() {
        let us = '\u{1f}';
        let constraints = [
            row(&[("constraint_type", json!("UNIQUE")), ("constraint_name", json!("t_code_key")), ("cols", json!("code"))]),
            row(&[("constraint_type", json!("PRIMARY KEY")), ("constraint_name", json!("t_id_pkey")), ("cols", json!(format!("id{us}k")))]),
            row(&[("constraint_type", json!("FOREIGN KEY")), ("cols", json!("p_id")), ("referenced_table", json!("p")), ("ref_cols", json!("id"))]),
        ];
        let indexes = [row(&[("index_name", json!("ix_a")), ("is_unique", json!(false)), ("sql", json!("CREATE INDEX ix_a ON t(a);"))])];
        let r = from_duckdb("main", &constraints, &indexes);
        assert!(!r.stats_available && r.note.as_deref() == Some(NOTE_DUCKDB));
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["PRIMARY KEY", "t_code_key", "ix_a"]);
        assert!(r.indexes[0].primary_key && r.indexes[0].key_columns == ["id", "k"]);
        assert!(r.indexes[1].unique && r.indexes[2].key_columns == ["a"]);
        assert_eq!((r.foreign_keys[0].columns.clone(), r.foreign_keys[0].ref_table.as_str()), (vec!["p_id".to_string()], "p"));
    }

    #[test]
    fn command_rows() {
        let pk = [
            row(&[("column_name", json!("b")), ("key_sequence", json!(2)), ("key_name", json!("pk_t"))]),
            row(&[("column_name", json!("a")), ("key_sequence", json!(1)), ("key_name", json!("pk_t"))]),
        ];
        let fk = [
            row(&[("fk_key_name", json!("fk1")), ("pk_table_name", json!("p")), ("pk_db_schema_name", json!("s")), ("fk_column_name", json!("x2")), ("pk_column_name", json!("b")), ("key_sequence", json!(2))]),
            row(&[("fk_key_name", json!("fk1")), ("pk_table_name", json!("p")), ("pk_db_schema_name", json!("s")), ("fk_column_name", json!("x1")), ("pk_column_name", json!("a")), ("key_sequence", json!(1))]),
            row(&[("fk_key_name", Value::Null), ("pk_table_name", json!("q")), ("fk_column_name", json!("y")), ("pk_column_name", json!("id")), ("key_sequence", json!(1))]),
        ];
        let r = from_commands(&pk, &fk);
        assert_eq!(r.note.as_deref(), Some(NOTE_OTHER));
        assert_eq!((r.indexes[0].name.as_str(), r.indexes[0].key_columns.clone()), ("pk_t", vec!["a".to_string(), "b".to_string()]));
        assert_eq!(r.foreign_keys.len(), 2);
        let fk1 = r.foreign_keys.iter().find(|f| f.name.as_deref() == Some("fk1")).unwrap();
        assert_eq!((fk1.columns.clone(), fk1.ref_columns.clone(), fk1.ref_schema.as_deref()), (vec!["x1".to_string(), "x2".to_string()], vec!["a".to_string(), "b".to_string()], Some("s")));
        assert!(from_commands(&[], &[]).indexes.is_empty());
    }
}

//! A table's indexes (`Session::index_usage`). Shared with libSQL, which
//! runs the same catalog queries over HTTP.
//!
//! - The indexes, the primary key and the foreign keys come from the same
//!   catalog read as the schema compare (`read_schema_with`), so an index
//!   has the same name here as in the drop script that "Eliminar índice"
//!   generates (UNIQUE constraints' automatic indexes included).
//! - The primary key: a rowid table whose key is an `INTEGER PRIMARY KEY`
//!   is keyed by the rowid itself (`ROWID`, no separate index); a `WITHOUT
//!   ROWID` table is stored in its key's B-tree (`CLUSTERED`); any other key
//!   has an automatic index (`B-TREE`).
//! - The size: the `dbstat` virtual table (pages × page size); where the
//!   build doesn't have it, the size stays empty.
//! - The counters: SQLite keeps none (no engine counts how often an index is
//!   read), so `stats_available` is false and the note says so.

use crate::schema::{self, Rows, DESC};
use dbine_driver::{IndexUsage, IndexUsageReport, TableSchema};
use serde_json::Value;
use std::collections::HashMap;

/// What the UI says about the missing counters.
pub const NOTE: &str = "SQLite no registra cuántas veces se usa cada índice: se listan los índices con sus columnas y tamaño, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución.";

/// `table`'s rows of `pragma_index_list`: catalog name, origin (`c`, `u`, `pk`).
pub fn index_list_sql(table: &str) -> String {
    format!("SELECT il.name, il.origin FROM pragma_index_list({}) il ORDER BY il.name", literal(table))
}

/// Used bytes per B-tree (`dbstat`, one aggregated row per object).
pub fn sizes_sql(names: &[String]) -> String {
    let list: Vec<String> = names.iter().map(|n| literal(n)).collect();
    format!("SELECT name, SUM(pgsize) FROM dbstat WHERE name IN ({}) GROUP BY name", list.join(", "))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn int(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// The report for `table`, from catalog queries run by `q` (a local
/// connection or libSQL over HTTP). A failing `dbstat` leaves sizes empty.
pub fn report_with<E>(q: &mut dyn FnMut(&str) -> Result<Rows, E>, table: &str) -> Result<IndexUsageReport, E> {
    let Some(t) = schema::read_schema_with(q)?.into_iter().find(|t| t.name == table) else {
        return Ok(IndexUsageReport { note: Some(NOTE.into()), ..Default::default() });
    };
    let list: Vec<(String, String)> = q(&index_list_sql(table))?.iter().map(|r| (text(&r[0]), text(&r[1]))).collect();
    let without_rowid = t.options.get("without_rowid").is_some_and(|v| v == "true");
    // Catalog names of the B-trees whose size each entry reports.
    let mut btrees: Vec<String> = list.iter().map(|(n, _)| n.clone()).collect();
    btrees.push(table.to_string());
    let sizes: HashMap<String, u64> = q(&sizes_sql(&btrees))
        .map(|rows| rows.iter().filter_map(|r| Some((text(&r[0]), int(&r[1])?))).collect())
        .unwrap_or_default();
    Ok(assemble(&t, &list, without_rowid, &sizes))
}


/// The entries: the primary key first, then the indexes in the schema's
/// order. `list`: `pragma_index_list` (catalog name, origin) by name, the
/// order the schema read numbers UNIQUE constraints' indexes in.
pub fn assemble(t: &TableSchema, list: &[(String, String)], without_rowid: bool, sizes: &HashMap<String, u64>) -> IndexUsageReport {
    let kb = |name: &str| sizes.get(name).map(|b| b.div_ceil(1024));
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        let auto = list.iter().find(|(_, origin)| origin == "pk").map(|(n, _)| n.clone());
        let (kind, btree) = match (&auto, without_rowid) {
            (_, true) => ("CLUSTERED", t.name.clone()),
            (Some(n), false) => ("B-TREE", n.clone()),
            (None, false) => ("ROWID", t.name.clone()),
        };
        indexes.push(IndexUsage {
            name: auto.filter(|_| !without_rowid).unwrap_or_else(|| "PRIMARY KEY".into()),
            kind: kind.into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            size_kb: kb(&btree),
            ..Default::default()
        });
    }
    // The schema's indexes pair with the catalog's other ones by position.
    let catalog: Vec<&String> = list.iter().filter(|(_, o)| o != "pk").map(|(n, _)| n).collect();
    for (i, ix) in t.indexes.iter().enumerate() {
        let desc: Vec<&str> = ix.options.get(DESC).map(|d| d.split(", ").collect()).unwrap_or_default();
        let catalog_name = if catalog.len() == t.indexes.len() { catalog[i].as_str() } else { ix.name.as_str() };
        indexes.push(IndexUsage {
            name: ix.name.clone(),
            kind: "B-TREE".into(),
            unique: ix.unique,
            key_columns: ix.columns.iter().map(|c| if desc.contains(&c.as_str()) { format!("{c} DESC") } else { c.clone() }).collect(),
            filter: ix.filter.clone(),
            size_kb: kb(catalog_name),
            ..Default::default()
        });
    }
    IndexUsageReport { note: Some(NOTE.into()), indexes, foreign_keys: t.foreign_keys.clone(), ..Default::default() }.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn report(c: &Connection, table: &str) -> IndexUsageReport {
        report_with(&mut |sql| schema::query_rows(c, sql), table).unwrap()
    }

    #[test]
    fn queries_quote_the_names() {
        assert_eq!(index_list_sql("a'b"), "SELECT il.name, il.origin FROM pragma_index_list('a''b') il ORDER BY il.name");
        assert_eq!(sizes_sql(&["x".into(), "y'".into()]), "SELECT name, SUM(pgsize) FROM dbstat WHERE name IN ('x', 'y''') GROUP BY name");
    }

    #[test]
    fn keys_indexes_and_foreign_keys() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
             CREATE TABLE c (id TEXT PRIMARY KEY, p_id INTEGER REFERENCES p(id), a INTEGER, b TEXT);
             CREATE INDEX ix_a ON c (a DESC, b);
             CREATE INDEX ix_b ON c (b) WHERE b IS NOT NULL;
             CREATE TABLE w (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;",
        )
        .unwrap();
        let r = report(&c, "c");
        assert!(!r.stats_available);
        assert_eq!(r.note.as_deref(), Some(NOTE));
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["sqlite_autoindex_c_1", "ix_a", "ix_b"]);
        let pk = &r.indexes[0];
        assert!(pk.primary_key && pk.unique);
        assert_eq!((pk.kind.as_str(), pk.key_columns.clone()), ("B-TREE", vec!["id".to_string()]));
        assert_eq!(r.indexes[1].key_columns, ["a DESC", "b"]);
        assert_eq!(r.indexes[2].filter.as_deref(), Some("b IS NOT NULL"));
        assert!(r.indexes.iter().all(|i| i.size_kb.is_some_and(|k| k > 0)), "dbstat: {:?}", r.indexes);
        assert_eq!(r.foreign_keys.len(), 1);
        assert_eq!((r.foreign_keys[0].columns.clone(), r.foreign_keys[0].ref_table.as_str()), (vec!["p_id".to_string()], "p"));
        assert!(r.indexes.iter().all(|i| !i.unused && i.read_share.is_none()));

        // The rowid key and a UNIQUE constraint's index, named as the drop script names it.
        let r = report(&c, "p");
        assert_eq!((r.indexes[0].name.as_str(), r.indexes[0].kind.as_str()), ("PRIMARY KEY", "ROWID"));
        assert_eq!(r.indexes[1].name, "p_1_key");
        assert!(r.indexes[1].unique && !r.indexes[1].primary_key);

        let r = report(&c, "w");
        assert_eq!((r.indexes[0].name.as_str(), r.indexes[0].kind.as_str()), ("PRIMARY KEY", "CLUSTERED"));
        assert_eq!(r.indexes.len(), 1);

        assert!(report(&c, "missing").indexes.is_empty());
    }

    #[test]
    fn dropping_an_index_through_the_sync_script() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER); CREATE INDEX ix_a ON t (a); CREATE INDEX ix_b ON t (b);").unwrap();
        let old = schema::read_schema(&c).unwrap().remove(0);
        let mut new = old.clone();
        new.indexes.retain(|i| i.name != "ix_b");
        let script = schema::sync_script(&[dbine_driver::TableChange::Alter { old, new }]).unwrap();
        for s in &script.statements {
            c.execute_batch(s).unwrap();
        }
        let names: Vec<String> = report(&c, "t").indexes.into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["PRIMARY KEY", "ix_a"]);
    }
}

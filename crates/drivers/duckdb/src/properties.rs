//! "Propiedades" of an attached database
//! ([`dbine_driver::Session::database_properties`]): facts only. DuckDB has
//! no settings stored per database: `SET` and `PRAGMA` settings belong to
//! the instance or the session, and how a database is attached (read-only,
//! type, encryption) is fixed by its `ATTACH`.

use crate::DuckDbSession;
use dbine_driver::{DatabaseProperties, Error, PropertyInfo, Result};

const STORAGE: &str = "Almacenamiento";

/// One query's first row, every column as text (NULL: `None`).
fn row(c: &duckdb::Connection, sql: &str, database: &str, ncols: usize) -> Result<Option<Vec<Option<String>>>> {
    let mut stmt = c.prepare(sql).map_err(Error::query)?;
    let mut rows = stmt
        .query_map([database], |r| (0..ncols).map(|i| r.get::<_, Option<String>>(i)).collect::<duckdb::Result<Vec<_>>>())
        .map_err(Error::query)?;
    rows.next().transpose().map_err(Error::query)
}

fn yes_no(v: Option<String>) -> Option<String> {
    v.map(|v| if v == "true" { "sí".into() } else { "no".into() })
}

pub(crate) fn read(c: &duckdb::Connection, database: &str) -> Result<DatabaseProperties> {
    let base = row(
        c,
        "SELECT COALESCE(path, ''), type, readonly::VARCHAR FROM duckdb_databases() WHERE database_name = ?1 AND NOT internal",
        database,
        3,
    )?
    .ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
    let mut info = Vec::new();
    let mut fact = |group: &str, label: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            info.push(PropertyInfo { group: group.into(), label: label.into(), value });
        }
    };
    let path = base[0].clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "(en memoria)".into());
    fact("", "Archivo", Some(path));
    fact("", "Tipo", base[1].clone());
    fact("", "Solo lectura", yes_no(base[2].clone()));
    // Objects, per kind.
    if let Some(r) = row(
        c,
        "SELECT (SELECT COUNT(*) FROM duckdb_schemas() WHERE database_name = ?1 AND NOT internal)::VARCHAR,
                (SELECT COUNT(*) FROM duckdb_tables() WHERE database_name = ?1 AND NOT internal)::VARCHAR,
                (SELECT COUNT(*) FROM duckdb_views() WHERE database_name = ?1 AND NOT internal)::VARCHAR,
                (SELECT COUNT(*) FROM duckdb_sequences() WHERE database_name = ?1)::VARCHAR,
                (SELECT COALESCE(SUM(estimated_size), 0) FROM duckdb_tables() WHERE database_name = ?1 AND NOT internal)::VARCHAR",
        database,
        5,
    )? {
        for (i, label) in ["Esquemas", "Tablas", "Vistas", "Secuencias", "Filas (estimadas)"].into_iter().enumerate() {
            fact("", label, r[i].clone());
        }
    }
    // Sizes: only databases DuckDB stores itself (an attached SQLite or
    // Postgres database isn't in `pragma_database_size`).
    if let Ok(Some(r)) = row(
        c,
        "SELECT database_size, block_size::VARCHAR, total_blocks::VARCHAR, used_blocks::VARCHAR, free_blocks::VARCHAR, wal_size, memory_usage
           FROM pragma_database_size() WHERE database_name = ?1",
        database,
        7,
    ) {
        fact("", "Tamaño", r[0].clone());
        for (i, label) in [
            (1, "Tamaño de bloque (bytes)"),
            (2, "Bloques"),
            (3, "Bloques usados"),
            (4, "Bloques libres"),
            (5, "Tamaño del WAL"),
            (6, "Memoria que usa"),
        ] {
            fact(STORAGE, label, r[i].clone());
        }
    }
    Ok(DatabaseProperties { info, ..Default::default() })
}

impl DuckDbSession {
    pub(crate) async fn properties(&self, database: &str) -> Result<DatabaseProperties> {
        let database = database.to_string();
        self.with(move |c| read(c, &database)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_of_a_database() {
        let c = crate::tests::memory();
        c.execute_batch("CREATE TABLE t AS SELECT range AS i FROM range(10); CREATE VIEW v AS SELECT 1").unwrap();
        let p = read(&c, "memory").unwrap();
        assert!(p.fields.is_empty() && p.values.is_empty());
        let get = |l: &str| p.info.iter().find(|i| i.label == l).map(|i| i.value.clone());
        assert_eq!(get("Archivo").as_deref(), Some("(en memoria)"));
        assert_eq!(get("Tablas").as_deref(), Some("1"));
        assert_eq!(get("Vistas").as_deref(), Some("1"));
        assert_eq!(get("Solo lectura").as_deref(), Some("no"));
        assert!(read(&c, "nope").is_err());
    }
}

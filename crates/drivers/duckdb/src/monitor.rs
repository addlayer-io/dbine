//! Monitor for an embedded DuckDB: memory by component
//! (`duckdb_memory()`), file and WAL sizes (`PRAGMA database_size`),
//! spilled temporary files, threads and settings. DuckDB runs inside the
//! app, so there are no server-side sessions or CPU figures.

use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use duckdb::Connection;
use serde_json::Value;

/// `1.2 MiB`, `512 bytes`, `12.7 GiB`, `1.5 GB` in bytes.
pub fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(s.len());
    let n: f64 = s[..split].trim().parse().ok()?;
    let unit = s[split..].trim().to_ascii_lowercase();
    let mult = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some(n * mult)
}

fn strings(c: &Connection, sql: &str, ncols: usize) -> duckdb::Result<Vec<Vec<Option<String>>>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map([], |r| (0..ncols).map(|i| r.get::<_, Option<String>>(i)).collect::<duckdb::Result<Vec<_>>>())?;
    rows.collect()
}

fn f(v: &Option<String>) -> Option<f64> {
    v.as_deref().and_then(dbine_driver::monitor::num)
}

fn mb(bytes: Option<f64>) -> Value {
    bytes.map(|b| ((b / 1_048_576.0) * 100.0).round() / 100.0).into()
}

/// `sessions`: DBine sessions sharing this database instance.
pub fn snapshot(c: &Connection, sessions: usize) -> MonitorSnapshot {
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();
    let mut probe = |what: &str, sql: &str, n: usize| match strings(c, sql, n) {
        Ok(r) => Some(r),
        Err(e) => {
            notes.push(format!("No se pudo leer {what}: {e}"));
            None
        }
    };

    let settings = probe(
        "la configuración",
        "SELECT name, value FROM duckdb_settings()
          WHERE name IN ('memory_limit', 'threads', 'temp_directory', 'max_temp_directory_size', 'access_mode',
                         'external_threads', 'default_order', 'enable_external_access', 'max_memory')",
        2,
    )
    .unwrap_or_default();
    let setting = |n: &str| settings.iter().find(|r| r[0].as_deref() == Some(n)).and_then(|r| r[1].clone());
    let memory = probe("la memoria (duckdb_memory)", "SELECT tag, memory_usage_bytes::VARCHAR, temporary_storage_bytes::VARCHAR FROM duckdb_memory() ORDER BY memory_usage_bytes DESC", 3);
    let sizes = probe(
        "el tamaño de las bases (database_size)",
        "SELECT database_name, database_size, block_size::VARCHAR, total_blocks::VARCHAR, used_blocks::VARCHAR,
                free_blocks::VARCHAR, wal_size, memory_usage, memory_limit
           FROM pragma_database_size()",
        9,
    );
    let temp = probe("los archivos temporales", "SELECT path, size::VARCHAR FROM duckdb_temporary_files()", 2);
    let tables = probe(
        "las tablas",
        "SELECT database_name || '.' || schema_name || '.' || table_name, estimated_size::VARCHAR, column_count::VARCHAR,
                index_count::VARCHAR
           FROM duckdb_tables() WHERE NOT internal ORDER BY estimated_size DESC LIMIT 20",
        4,
    );
    let extensions = probe("las extensiones", "SELECT extension_name FROM duckdb_extensions() WHERE loaded ORDER BY 1", 1);

    let mem_used: Option<f64> = memory.as_ref().map(|rows| rows.iter().filter_map(|r| f(&r[1])).sum());
    let temp_mem: Option<f64> = memory.as_ref().map(|rows| rows.iter().filter_map(|r| f(&r[2])).sum());
    let limit = setting("memory_limit").as_deref().and_then(parse_size);
    let col = |r: &Vec<Option<String>>, i: usize| f(&r[i]);
    let (mut used, mut free, mut wal) = (None::<f64>, None::<f64>, None::<f64>);
    for r in sizes.iter().flatten() {
        let bs = col(r, 2).unwrap_or(0.0);
        *used.get_or_insert(0.0) += col(r, 4).unwrap_or(0.0) * bs;
        *free.get_or_insert(0.0) += col(r, 5).unwrap_or(0.0) * bs;
        *wal.get_or_insert(0.0) += r[6].as_deref().and_then(parse_size).unwrap_or(0.0);
    }
    let temp_files: Option<f64> = temp.as_ref().map(|rows| rows.iter().filter_map(|r| f(&r[1])).sum());

    let m = &mut snap.metrics;
    m.push(Metric::new("mem_used", "Memoria de DuckDB", "Memoria", U::Bytes, mem_used).max(limit));
    m.push(Metric::new("mem_temp", "Datos volcados a disco (temporales)", "Memoria", U::Bytes, temp_mem));
    m.push(Metric::new("connections", "Sesiones de DBine sobre la base", "Conexiones", U::Count, Some(sessions as f64)));
    m.push(Metric::new("storage_used", "Espacio usado", "Almacenamiento", U::Bytes, used));
    m.push(Metric::new("free_space", "Bloques libres", "Almacenamiento", U::Bytes, free));
    m.push(Metric::new("wal_size", "Tamaño del WAL", "Almacenamiento", U::Bytes, wal));
    m.push(Metric::new("temp_files", "Archivos temporales", "Disco", U::Bytes, temp_files).max(setting("max_temp_directory_size").as_deref().and_then(parse_size)));

    for (label, name) in [
        ("Hilos (threads)", "threads"),
        ("Límite de memoria", "memory_limit"),
        ("Carpeta temporal", "temp_directory"),
        ("Máximo en disco temporal", "max_temp_directory_size"),
        ("Modo de acceso", "access_mode"),
        ("Acceso a archivos externos", "enable_external_access"),
    ] {
        if let Some(v) = setting(name).filter(|v| !v.is_empty()) {
            snap.info.push((label.into(), v));
        }
    }
    if let Some(ext) = &extensions {
        let names: Vec<String> = ext.iter().filter_map(|r| r[0].clone()).collect();
        snap.info.push(("Extensiones cargadas".into(), names.join(", ")));
    }

    if let Some(rows) = memory {
        let mut t = MonitorTable::new("memory", "Memoria por componente", &["Componente", "En memoria (MB)", "Temporal en disco (MB)"]);
        t.rows = rows
            .into_iter()
            .filter(|r| f(&r[1]).unwrap_or(0.0) > 0.0 || f(&r[2]).unwrap_or(0.0) > 0.0)
            .map(|r| vec![r[0].clone().into(), mb(f(&r[1])), mb(f(&r[2]))])
            .collect();
        snap.tables.push(t);
    }
    if let Some(rows) = sizes {
        let mut t = MonitorTable::new(
            "databases",
            "Bases adjuntas y tamaños",
            &["Base", "Tamaño", "Bloques usados", "Bloques libres", "WAL", "Memoria"],
        );
        t.rows = rows
            .into_iter()
            .map(|r| vec![r[0].clone().into(), r[1].clone().into(), f(&r[4]).into(), f(&r[5]).into(), r[6].clone().into(), r[7].clone().into()])
            .collect();
        snap.tables.push(t);
    }
    if let Some(rows) = tables {
        let mut t = MonitorTable::new("top_objects", "Tablas más grandes (filas estimadas)", &["Tabla", "Filas (estimadas)", "Columnas", "Índices"]);
        t.rows = rows.into_iter().map(|r| vec![r[0].clone().into(), f(&r[1]).into(), f(&r[2]).into(), f(&r[3]).into()]).collect();
        snap.tables.push(t);
    }
    if let Some(rows) = temp.filter(|r| !r.is_empty()) {
        let mut t = MonitorTable::new("temp_files", "Archivos temporales", &["Archivo", "Tamaño (MB)"]);
        t.rows = rows.into_iter().map(|r| vec![r[0].clone().into(), mb(f(&r[1]))]).collect();
        snap.tables.push(t);
    }
    snap.notes.push(
        "DuckDB corre dentro de DBine: no hay servidor, sesiones remotas ni uso de CPU propio que informar.".into(),
    );
    snap.notes.extend(notes);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512 bytes"), Some(512.0));
        assert_eq!(parse_size("1.5 KiB"), Some(1536.0));
        assert_eq!(parse_size("2 GB"), Some(2e9));
        assert_eq!(parse_size("12.7 GiB").map(|b| (b / 1_073_741_824.0 * 10.0).round()), Some(127.0));
        assert_eq!(parse_size("0 bytes"), Some(0.0));
        assert_eq!(parse_size("unlimited"), None);
    }

    #[test]
    fn snapshot_of_a_memory_database() {
        let c = crate::tests::memory();
        c.execute_batch("CREATE TABLE t AS SELECT range AS i FROM range(100000)").unwrap();
        let s = snapshot(&c, 1);
        let v = |k: &str| s.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
        assert!(v("mem_used").unwrap() > 0.0, "{:?}", s);
        assert!(s.metrics.iter().find(|m| m.key == "mem_used").unwrap().max.is_some());
        assert!(s.tables.iter().any(|t| t.key == "top_objects" && !t.rows.is_empty()));
        assert!(s.info.iter().any(|(k, _)| k.starts_with("Hilos")));
        assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
    }
}

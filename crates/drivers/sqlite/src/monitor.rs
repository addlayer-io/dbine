//! Monitor for a SQLite database: there's no server, so it's the file
//! (pages, free pages, WAL), the settings that matter (journal mode,
//! cache, synchronous…) and the space per table. The pragma part is
//! shared with the libSQL driver, which runs it over HTTP; the local driver
//! adds the file sizes and SQLite's memory counters.

use crate::schema::Rows;
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use serde_json::Value;

/// `dbstat` walks every page: only for databases up to this many pages
/// (400 MB with 4 KiB pages).
const DBSTAT_MAX_PAGES: f64 = 100_000.0;

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Settings as `PRAGMA x` gives them (one row, one column).
const SETTINGS: &[(&str, &str)] = &[
    ("journal_mode", "Modo del diario (journal_mode)"),
    ("synchronous", "Sincronización (synchronous)"),
    ("auto_vacuum", "Auto vacuum"),
    ("encoding", "Codificación"),
    ("user_version", "Versión del esquema (user_version)"),
    ("application_id", "ID de aplicación"),
    ("foreign_keys", "Claves foráneas activas"),
    ("locking_mode", "Modo de bloqueo"),
    ("wal_autocheckpoint", "Checkpoint automático del WAL (páginas)"),
    ("mmap_size", "Memoria mapeada (mmap_size)"),
    ("busy_timeout", "Espera por bloqueos (ms)"),
    ("temp_store", "Almacenamiento temporal"),
];

/// The part every SQLite-compatible engine answers with plain SQL.
/// `q` runs a statement and returns its rows; failures leave a note.
pub fn pragma_snapshot<E: std::fmt::Display>(q: &mut dyn FnMut(&str) -> Result<Rows, E>) -> MonitorSnapshot {
    let mut snap = MonitorSnapshot::default();
    let mut one = |sql: &str, notes: &mut Vec<String>| -> Option<Value> {
        match q(sql) {
            Ok(rows) => rows.into_iter().next().and_then(|r| r.into_iter().next()),
            Err(e) => {
                notes.push(format!("No se pudo leer «{sql}»: {e}"));
                None
            }
        }
    };
    let mut notes = Vec::new();
    let page_size = one("PRAGMA page_size", &mut notes).as_ref().and_then(num).unwrap_or(4096.0);
    let pages = one("PRAGMA page_count", &mut notes).as_ref().and_then(num);
    let free = one("PRAGMA freelist_count", &mut notes).as_ref().and_then(num);
    let cache = one("PRAGMA cache_size", &mut notes).as_ref().and_then(num);
    let version = one("SELECT sqlite_version()", &mut notes).as_ref().and_then(text);
    // Negative: KiB; positive: pages.
    let cache_bytes = cache.map(|c| if c < 0.0 { -c * 1024.0 } else { c * page_size });
    let used = pages.map(|p| (p - free.unwrap_or(0.0)) * page_size);

    let m = &mut snap.metrics;
    m.push(Metric::new("storage_used", "Espacio usado por datos", "Almacenamiento", U::Bytes, used).max(pages.map(|p| p * page_size)));
    m.push(Metric::new("free_space", "Páginas libres (recuperables con VACUUM)", "Almacenamiento", U::Bytes, free.map(|f| f * page_size)));
    m.push(Metric::new("pages", "Páginas", "Almacenamiento", U::Count, pages));
    m.push(Metric::new("cache_size", "Tamaño máximo de la caché", "Memoria", U::Bytes, cache_bytes));

    if let Some(v) = version {
        snap.info.push(("Versión de SQLite".into(), v));
    }
    snap.info.push(("Tamaño de página".into(), format!("{page_size} bytes")));
    for (pragma, label) in SETTINGS {
        if let Ok(rows) = q(&format!("PRAGMA {pragma}")) {
            if let Some(v) = rows.into_iter().next().and_then(|r| r.into_iter().next()).and_then(|v| text(&v)) {
                let v = match (*pragma, v.as_str()) {
                    ("synchronous", "0") => "OFF".into(),
                    ("synchronous", "1") => "NORMAL".into(),
                    ("synchronous", "2") => "FULL".into(),
                    ("synchronous", "3") => "EXTRA".into(),
                    ("auto_vacuum", "0") => "NONE".into(),
                    ("auto_vacuum", "1") => "FULL".into(),
                    ("auto_vacuum", "2") => "INCREMENTAL".into(),
                    ("temp_store", "0") => "DEFAULT".into(),
                    ("temp_store", "1") => "FILE".into(),
                    ("temp_store", "2") => "MEMORY".into(),
                    ("foreign_keys", "0") => "no".into(),
                    ("foreign_keys", "1") => "sí".into(),
                    _ => v,
                };
                snap.info.push((label.to_string(), v));
            }
        }
    }

    // Attached databases and their sizes.
    if let Ok(list) = q("SELECT name, file FROM pragma_database_list ORDER BY seq") {
        let mut t = MonitorTable::new("databases", "Bases adjuntas y tamaños", &["Base", "Archivo", "Páginas", "Libres", "Tamaño (MB)"]);
        for r in list {
            let Some(name) = r.first().and_then(text) else { continue };
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            let pc = q(&format!("PRAGMA {quoted}.page_count")).ok().and_then(|r| r.into_iter().next()).and_then(|r| r.first().and_then(num));
            let fc = q(&format!("PRAGMA {quoted}.freelist_count")).ok().and_then(|r| r.into_iter().next()).and_then(|r| r.first().and_then(num));
            let ps = q(&format!("PRAGMA {quoted}.page_size")).ok().and_then(|r| r.into_iter().next()).and_then(|r| r.first().and_then(num)).unwrap_or(page_size);
            let mb = pc.map(|p| ((p * ps / 1_048_576.0) * 100.0).round() / 100.0);
            t.rows.push(vec![name.into(), r.get(1).cloned().unwrap_or(Value::Null), pc.into(), fc.into(), mb.into()]);
        }
        snap.tables.push(t);
    }

    // Space per table and index, when the database is small enough to walk.
    if pages.is_some_and(|p| p <= DBSTAT_MAX_PAGES) {
        match q("SELECT name, SUM(ncell), SUM(pgsize), SUM(unused) FROM dbstat GROUP BY name ORDER BY SUM(pgsize) DESC LIMIT 20") {
            Ok(rows) => {
                let mut t = MonitorTable::new("top_objects", "Objetos más grandes", &["Objeto", "Celdas", "Tamaño (KB)", "Sin usar (KB)"]);
                t.rows = rows
                    .into_iter()
                    .map(|r| {
                        let kb = |i: usize| r.get(i).and_then(num).map(|b| (b / 1024.0 * 10.0).round() / 10.0);
                        vec![r.first().cloned().unwrap_or(Value::Null), r.get(1).cloned().unwrap_or(Value::Null), kb(2).into(), kb(3).into()]
                    })
                    .collect();
                snap.tables.push(t);
            }
            Err(_) => notes.push("Esta compilación de SQLite no trae la tabla dbstat: no se ve el espacio por tabla.".into()),
        }
    } else if pages.is_some() {
        notes.push("La base es grande: el espacio por tabla (dbstat) no se calcula en cada lectura.".into());
    }
    snap.notes.push("SQLite no es un servidor: no hay CPU, conexiones ni sesiones que informar.".into());
    snap.notes.extend(notes);
    snap
}

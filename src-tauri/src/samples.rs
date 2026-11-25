//! Sample databases ("Bases de ejemplo", docs/sample-databases.md): a folder
//! DBine adds once, so there is something to explore before connecting
//! anything of one's own.
//!
//! - **Tienda** (SQLite): a store's operational database: clients,
//!   products, orders and their items, invoices and events, with foreign
//!   keys, indexes, views (one over another) and a trigger.
//! - **Analítica** (DuckDB): the same model and data, with differences on
//!   purpose so that comparing finds something: clients that changed city
//!   and e-mail and ten that only it has, some prices up 10%, a `stock`
//!   column, no `eventos` table and one more view.
//! - **Archivos** (the CSV / Parquet / JSON files engine): `metas.csv`
//!   (monthly sales targets, written here) and `eventos.parquet` (written by
//!   DuckDB).
//! - A **tour** notebook that crosses the two databases and the files and
//!   ends in charts, written in the UI's language (the UI sends its texts).
//!
//! Installed once, new installations and updated ones alike
//! (`samples.installed`, a preference that travels with the cloud backup;
//! the ids are fixed, so two computers never get two folders). Deleting the
//! folder, or "Quitar las bases de ejemplo" ([`remove_in`]), never brings it
//! back.
//!
//! The databases are made when their connection first opens ([`ensure`],
//! from `AppState::connect_config`), so nothing is downloaded at startup:
//! SQLite here and at once; DuckDB through its driver, which downloads on
//! first use like any DuckDB connection. One that's gone (deleted, or a
//! backup restored on another computer) is made again the same way.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::{ConnectionFolder, SavedConnection, StateStore};
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::State;

pub const FOLDER_ID: &str = "dbine-samples";
pub const TIENDA_ID: &str = "dbine-sample-tienda";
pub const ANALITICA_ID: &str = "dbine-sample-analitica";
pub const ARCHIVOS_ID: &str = "dbine-sample-archivos";
/// The preference that says they were installed (whatever happened since).
pub const INSTALLED: &str = "samples.installed";

const TIENDA_FILE: &str = "tienda.sqlite";
const ANALITICA_FILE: &str = "analitica.duckdb";
const ARCHIVOS_DIR: &str = "archivos";
const METAS_FILE: &str = "metas.csv";
const EVENTOS_FILE: &str = "eventos.parquet";

/// Sizes: enough to look real, a few MB on disk.
const CLIENTES: u32 = 2_000;
const NUEVOS: u32 = 10;
const PRODUCTOS: u32 = 200;
const PEDIDOS: u32 = 10_000;
const ITEMS: u32 = PEDIDOS * 3;
const EVENTOS: u32 = 20_000;

static DIR: OnceLock<PathBuf> = OnceLock::new();
/// One maker at a time: two sessions opening the same new database at once
/// (the explorer's lanes) would both make it.
static MAKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Where the sample files live (the app's data folder, `samples/`).
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

fn dir() -> PathBuf {
    DIR.get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("dbine-samples"))
}

/// The names and the tour's text, in the UI's language.
#[derive(Debug, Clone, Deserialize)]
pub struct Texts {
    pub folder: String,
    pub tienda: String,
    pub analitica: String,
    pub archivos: String,
    /// The tour's file name, without the extension.
    pub tour_file: String,
    pub tour: TourTexts,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TourTexts {
    /// Markdown: what's in the folder and what to try.
    pub intro: String,
    /// Markdown before the clients of both databases.
    pub differences: String,
    /// The city shown for a client that only Analítica has.
    pub only_in_analitica: String,
    /// Markdown before sales against targets.
    pub targets: String,
}

fn failed(what: &str, e: impl std::fmt::Display) -> CommandError {
    CommandError::State(format!(
        "no se pudo preparar la base de ejemplo {what}: {e}"
    ))
}

// -- install and remove -----------------------------------------------------------------

/// Adds the folder, its three connections and the tour, once. `Some(tour)`
/// when it did now; `None` when it was already done (or removed since).
pub fn install_in(store: &StateStore, dir: &Path, texts: &Texts) -> CommandResult<Option<PathBuf>> {
    if store.get_setting(INSTALLED)?.and_then(|v| v.as_bool()) == Some(true) {
        return Ok(None);
    }
    std::fs::create_dir_all(dir).map_err(|e| failed(&texts.folder, e))?;
    store.save_folder(&ConnectionFolder {
        id: FOLDER_ID.into(),
        name: texts.folder.clone(),
        parent_id: None,
        color: None,
    })?;
    for (id, name, driver, file) in [
        (TIENDA_ID, &texts.tienda, "sqlite", TIENDA_FILE),
        (ANALITICA_ID, &texts.analitica, "duckdb", ANALITICA_FILE),
        (ARCHIVOS_ID, &texts.archivos, "duckdb_files", ARCHIVOS_DIR),
    ] {
        store.save_connection(&SavedConnection {
            id: id.into(),
            name: name.clone(),
            color: None,
            config: ConnectionConfig {
                driver: driver.into(),
                host: dir.join(file).to_string_lossy().into_owned(),
                ..Default::default()
            },
            save_password: false,
            folder_id: Some(FOLDER_ID.into()),
            tags: Vec::new(),
            mcp_level: None,
            updated_at: String::new(),
        })?;
    }
    let tour = write_tour(dir, texts, true)?;
    store.set_setting(INSTALLED, Some(&serde_json::Value::Bool(true)))?;
    Ok(Some(tour))
}

/// Deletes the sample connections, the folder and the files; the
/// preference stays, so they don't come back. A connection the user moved
/// into the folder moves up, as when deleting any folder.
pub fn remove_in(store: &StateStore, dir: &Path) -> CommandResult<()> {
    for id in [TIENDA_ID, ANALITICA_ID, ARCHIVOS_ID] {
        store.delete_connection(id)?;
    }
    if store.list_folders()?.iter().any(|f| f.id == FOLDER_ID) {
        store.delete_folder(FOLDER_ID)?;
    }
    store.set_setting(INSTALLED, Some(&serde_json::Value::Bool(true)))?;
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            tracing::warn!(?dir, "samples: could not delete the files: {e}");
        }
    }
    Ok(())
}

/// The tour notebook: written when missing (or always with `fresh`).
pub fn write_tour(dir: &Path, texts: &Texts, fresh: bool) -> CommandResult<PathBuf> {
    let path = dir.join(format!("{}.ipynb", file_name(&texts.tour_file)));
    if !fresh && path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).map_err(|e| failed(&texts.folder, e))?;
    let nb = tour(texts);
    let text = crate::notebook::serialize(&nb, crate::notebook::PreviewLimits::new(None, None));
    write_atomic(&path, text.as_bytes()).map_err(|e| failed(&texts.folder, e))?;
    Ok(path)
}

/// A file name from the UI's text: letters, digits, spaces and `-_.()`;
/// "Recorrido" if nothing is left.
fn file_name(s: &str) -> String {
    let s: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || " -_.()".contains(*c))
        .collect();
    let s = s.trim().trim_matches('.').trim();
    if s.is_empty() {
        r"Recorrido".into()
    } else {
        s.chars().take(80).collect()
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let part = part_of(path);
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path)
}

/// `name.ext` → `.name.ext.part`, next to it.
fn part_of(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.part"))
}

// -- the tour ---------------------------------------------------------------------------

fn tour(texts: &Texts) -> crate::notebook::Notebook {
    let target = |name: &str| serde_json::json!({ "connection": name });
    // DataFrame SQL (the file says `{"dataframes": true}`).
    let frames = serde_json::json!({ "results": true });
    let md = |text: &str| serde_json::json!({ "kind": "markdown", "source": text });
    let sql = |name: &str,
               target: serde_json::Value,
               source: String,
               chart: Option<serde_json::Value>| {
        serde_json::json!({
            "kind": "sql", "language": "sql", "name": name, "target": target,
            "source": source, "chart": chart,
        })
    };
    let chart = |kind: &str, x: &str, y: &[&str]| {
        serde_json::json!({
            "view": "chart", "active": 0,
            "charts": [{ "title": "", "type": kind, "x": x, "y": y, "agg": "sum", "sort": "none", "split": null }],
        })
    };
    let only = texts.tour.only_in_analitica.replace('\'', "''");
    let cells = vec![
        md(&texts.tour.intro),
        md(&texts.tour.differences),
        sql(
            "clientes_tienda",
            target(&texts.tienda),
            r"SELECT id, nombre, ciudad, email FROM clientes".into(),
            None,
        ),
        sql(
            "clientes_analitica",
            target(&texts.analitica),
            r"SELECT id, nombre, ciudad, email FROM clientes".into(),
            None,
        ),
        sql(
            "diferencias",
            frames.clone(),
            format!(
                r"SELECT coalesce(t.ciudad, '{only}') AS ciudad, count(*) AS clientes
FROM clientes_tienda t
FULL JOIN clientes_analitica a ON a.id = t.id
WHERE t.id IS NULL OR a.id IS NULL OR t.ciudad <> a.ciudad OR t.email <> a.email
GROUP BY 1
ORDER BY clientes DESC"
            ),
            Some(chart("bar", "ciudad", &["clientes"])),
        ),
        md(&texts.tour.targets),
        sql(
            "metas",
            target(&texts.archivos),
            r"SELECT mes, categoria, meta FROM metas".into(),
            None,
        ),
        sql(
            "ventas",
            target(&texts.tienda),
            r"SELECT mes, categoria, ventas FROM v_ventas_mensuales".into(),
            None,
        ),
        sql(
            "ventas_vs_metas",
            frames,
            r"SELECT v.mes, round(sum(v.ventas)) AS ventas, sum(m.meta) AS meta
FROM ventas v
JOIN metas m ON m.mes = v.mes AND m.categoria = v.categoria
GROUP BY v.mes
ORDER BY v.mes"
                .into(),
            Some(chart("line", "mes", &["ventas", "meta"])),
        ),
    ];
    serde_json::from_value(serde_json::json!({
        "target": target(&texts.tienda),
        "save_results": true,
        "cells": cells,
        "origin": "dbine",
        "extra_metadata": {},
    }))
    .expect("the tour is a notebook")
}

// -- making the files -------------------------------------------------------------------

/// Before a sample connection opens: makes its database (or files) if they
/// aren't there. Any other connection, or a sample one the user pointed
/// somewhere else, is left alone.
pub async fn ensure(connection_id: &str, cfg: &ConnectionConfig) -> CommandResult<()> {
    let expected = match connection_id {
        TIENDA_ID => TIENDA_FILE,
        ANALITICA_ID => ANALITICA_FILE,
        ARCHIVOS_ID => ARCHIVOS_DIR,
        _ => return Ok(()),
    };
    let path = PathBuf::from(cfg.host.trim());
    if path.file_name().and_then(|n| n.to_str()) != Some(expected) {
        return Ok(());
    }
    let _making = MAKING.lock().await;
    match connection_id {
        TIENDA_ID if !path.exists() => make_tienda(&path).await,
        ANALITICA_ID if !path.exists() || !ready_mark(&path).exists() => {
            make_analitica(&path).await
        }
        ARCHIVOS_ID => make_archivos(&path).await,
        _ => Ok(()),
    }
}

fn parent_ready(path: &Path, what: &str) -> CommandResult<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| failed(what, e))?;
    }
    Ok(())
}

/// Analítica is ready once its script ran to the end.
fn ready_mark(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.ok"))
}

async fn make_tienda(path: &Path) -> CommandResult<()> {
    parent_ready(path, TIENDA_FILE)?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> CommandResult<()> {
        let part = part_of(&path);
        let _ = std::fs::remove_file(&part);
        let c = rusqlite::Connection::open(&part).map_err(|e| failed(TIENDA_FILE, e))?;
        for st in tienda_sql() {
            c.execute_batch(&st).map_err(|e| failed(TIENDA_FILE, e))?;
        }
        drop(c);
        std::fs::rename(&part, &path).map_err(|e| failed(TIENDA_FILE, e))
    })
    .await
    .map_err(|e| failed(TIENDA_FILE, e))?
}

async fn make_analitica(path: &Path) -> CommandResult<()> {
    parent_ready(path, ANALITICA_FILE)?;
    let cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut s = dbine_drivers::open_session(&cfg, None).await?;
    for st in analitica_sql() {
        run(s.as_mut(), &st, ANALITICA_FILE).await?;
    }
    drop(s);
    std::fs::write(ready_mark(path), b"").map_err(|e| failed(ANALITICA_FILE, e))
}

async fn make_archivos(dir: &Path) -> CommandResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| failed(ARCHIVOS_DIR, e))?;
    let metas = dir.join(METAS_FILE);
    if !metas.exists() {
        write_atomic(&metas, metas_csv().as_bytes()).map_err(|e| failed(METAS_FILE, e))?;
    }
    let eventos = dir.join(EVENTOS_FILE);
    if !eventos.exists() {
        // Written next to the folder, then moved in: the folder never shows
        // a half-written file.
        let part = dir
            .parent()
            .map(|p| p.join(format!(".{EVENTOS_FILE}.part")))
            .unwrap_or_else(|| part_of(&eventos));
        let _ = std::fs::remove_file(&part);
        let mut cfg = ConnectionConfig {
            driver: "duckdb".into(),
            ..Default::default()
        };
        cfg.options.insert("isolated".into(), "true".into());
        let mut s = dbine_drivers::open_session(&cfg, None).await?;
        let to = part.to_string_lossy().replace('\'', "''");
        run(
            s.as_mut(),
            &format!(r"COPY ({}) TO '{to}' (FORMAT PARQUET)", eventos_select()),
            EVENTOS_FILE,
        )
        .await?;
        drop(s);
        std::fs::rename(&part, &eventos).map_err(|e| failed(EVENTOS_FILE, e))?;
    }
    Ok(())
}

async fn run(s: &mut dyn Session, sql: &str, what: &str) -> CommandResult<()> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1, &mut out)
        .await
        .map_err(|e| failed(what, e))?;
    match out.error.take() {
        Some(e) => Err(failed(what, e)),
        None => Ok(()),
    }
}

// -- the data ---------------------------------------------------------------------------

const CIUDADES: &[&str] = &[
    r"Buenos Aires",
    r"Córdoba",
    r"Rosario",
    r"Mendoza",
    r"La Plata",
    r"Mar del Plata",
    r"San Miguel de Tucumán",
    r"Salta",
    r"Santa Fe",
    r"Neuquén",
    r"Bahía Blanca",
    r"Posadas",
];
const NOMBRES: &[&str] = &[
    r"Juan",
    r"María",
    r"Carlos",
    r"Lucía",
    r"Diego",
    r"Sofía",
    r"Martín",
    r"Valentina",
    r"Pablo",
    r"Camila",
];
const APELLIDOS: &[&str] = &[
    r"González",
    r"Rodríguez",
    r"Gómez",
    r"Fernández",
    r"López",
    r"Díaz",
    r"Martínez",
    r"Pérez",
    r"García",
    r"Sánchez",
];
const ARTICULOS: &[&str] = &[
    r"Yerba",
    r"Gaseosa",
    r"Detergente",
    r"Auriculares",
    r"Sartén",
    r"Cuaderno",
    r"Shampoo",
    r"Taladro",
];
pub(crate) const CATEGORIAS: &[&str] = &[
    r"Almacén",
    r"Bebidas",
    r"Limpieza",
    r"Electrónica",
    r"Hogar",
    r"Librería",
    r"Perfumería",
    r"Ferretería",
];
const ESTADOS: &[&str] = &[
    r"pendiente",
    r"pagado",
    r"enviado",
    r"entregado",
    r"cancelado",
];
const TIPOS: &[&str] = &[
    r"login",
    r"compra",
    r"reclamo",
    r"pago",
    r"envio",
    r"consulta",
];
const EV_ESTADOS: &[&str] = &[r"ok", r"pendiente", r"error"];

/// Element `expr` (0-based) of a list, as portable SQL.
fn pick(list: &[&str], expr: &str) -> String {
    let whens: Vec<String> = list
        .iter()
        .enumerate()
        .map(|(i, v)| format!("WHEN {i} THEN '{}'", v.replace('\'', "''")))
        .collect();
    format!("CASE ({expr}) {} END", whens.join(" "))
}

/// The months the orders span (2025-01 … 2026-08), as `YYYY-MM`.
pub(crate) fn months() -> Vec<String> {
    (0..20)
        .map(|i| format!("{}-{:02}", 2025 + i / 12, i % 12 + 1))
        .collect()
}

/// Monthly targets per category, near what the orders sell.
fn metas_csv() -> String {
    let mut s = String::from("mes,categoria,meta\n");
    for (i, mes) in months().iter().enumerate() {
        for (c, cat) in CATEGORIAS.iter().enumerate() {
            let meta = 2_600_000 + 15_000 * i as u64 + 40_000 * c as u64;
            s.push_str(&format!("{mes},{cat},{meta}\n"));
        }
    }
    s
}

fn tienda_sql() -> Vec<String> {
    let g = |k: u32| {
        format!(
            "(WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < {k}) SELECT n FROM g)"
        )
    };
    vec![
        format!(
            r"CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre VARCHAR(100) NOT NULL, email VARCHAR(150) NOT NULL, ciudad VARCHAR(60) NOT NULL, activo BOOLEAN NOT NULL, creado TIMESTAMP NOT NULL);
INSERT INTO clientes SELECT n, {} || ' ' || {}, 'cliente' || n || '@ejemplo.com.ar', {}, n % 7 <> 0, datetime('2023-01-01 09:00:00', '+' || (n * 431) || ' minutes') FROM {};",
            pick(NOMBRES, "n % 10"),
            pick(APELLIDOS, "(n / 10) % 10"),
            pick(CIUDADES, "n % 12"),
            g(CLIENTES)
        ),
        format!(
            r"CREATE TABLE productos (id INTEGER PRIMARY KEY, nombre VARCHAR(120) NOT NULL, categoria VARCHAR(40) NOT NULL, precio DECIMAL(12,2) NOT NULL);
INSERT INTO productos SELECT n, {} || ' ' || n, {}, round(100 + (n * 37) % 9900 + 0.99, 2) FROM {};",
            pick(ARTICULOS, "n % 8"),
            pick(CATEGORIAS, "n % 8"),
            g(PRODUCTOS)
        ),
        format!(
            r"CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER NOT NULL REFERENCES clientes (id), fecha DATE NOT NULL, estado VARCHAR(20) NOT NULL, total DECIMAL(14,2) NOT NULL DEFAULT 0);
INSERT INTO pedidos (id, cliente_id, fecha, estado, total) SELECT n, 1 + (n * 7) % {CLIENTES}, date('2025-01-01', '+' || (n % 600) || ' days'), {}, 0 FROM {};
CREATE INDEX ix_pedidos_cliente ON pedidos (cliente_id);
CREATE INDEX ix_pedidos_fecha ON pedidos (fecha);",
            pick(ESTADOS, "n % 5"),
            g(PEDIDOS)
        ),
        format!(
            r"CREATE TABLE pedido_items (pedido_id INTEGER NOT NULL REFERENCES pedidos (id), producto_id INTEGER NOT NULL REFERENCES productos (id), cantidad INTEGER NOT NULL, precio DECIMAL(12,2) NOT NULL, PRIMARY KEY (pedido_id, producto_id));
INSERT INTO pedido_items SELECT 1 + (n - 1) / 3, p.id, 1 + n % 5, p.precio FROM {} JOIN productos p ON p.id = 1 + ((1 + (n - 1) / 3) * 13 + ((n - 1) % 3) * 101) % {PRODUCTOS};
UPDATE pedidos SET total = (SELECT sum(cantidad * precio) FROM pedido_items WHERE pedido_id = pedidos.id);",
            g(ITEMS)
        ),
        r"CREATE TABLE facturas (id INTEGER PRIMARY KEY, pedido_id INTEGER NOT NULL REFERENCES pedidos (id), numero VARCHAR(20) NOT NULL, fecha DATE NOT NULL, monto DECIMAL(14,2) NOT NULL);
INSERT INTO facturas SELECT id, id, 'A-0001-' || substr('00000000' || id, -8), date(fecha, '+1 day'), total FROM pedidos;"
            .into(),
        format!(
            r"CREATE TABLE eventos (id INTEGER PRIMARY KEY, fecha TIMESTAMP NOT NULL, tipo VARCHAR(30) NOT NULL, cliente_id INTEGER NOT NULL, detalle VARCHAR(200) NOT NULL, estado VARCHAR(20) NOT NULL);
INSERT INTO eventos SELECT n, datetime('2025-01-01 00:00:00', '+' || (n * 2600) || ' seconds'), {tipo}, 1 + n % {CLIENTES}, CASE WHEN n % 1000 = 0 THEN 'revisar cliente_' || (1 + n % {CLIENTES}) || ' con cobranzas' ELSE 'evento de ' || {tipo} || ' registrado sin novedades' END, {} FROM {};
CREATE INDEX ix_eventos_cliente ON eventos (cliente_id);",
            pick(EV_ESTADOS, "n % 3"),
            g(EVENTOS),
            tipo = pick(TIPOS, "n % 6"),
        ),
        r"CREATE TABLE auditoria (id INTEGER PRIMARY KEY AUTOINCREMENT, tabla VARCHAR(60) NOT NULL, accion VARCHAR(120) NOT NULL, fecha TIMESTAMP NOT NULL);
CREATE TRIGGER tr_pedidos_audit AFTER UPDATE ON pedidos BEGIN INSERT INTO auditoria (tabla, accion, fecha) VALUES ('pedidos', 'update ' || NEW.id || ' ' || OLD.estado || ' -> ' || NEW.estado, datetime('now')); END;
CREATE VIEW v_ventas_mensuales AS SELECT strftime('%Y-%m', p.fecha) AS mes, pr.categoria, sum(i.cantidad * i.precio) AS ventas, count(DISTINCT p.id) AS pedidos FROM pedidos p JOIN pedido_items i ON i.pedido_id = p.id JOIN productos pr ON pr.id = i.producto_id GROUP BY 1, 2;
CREATE VIEW v_clientes_activos AS SELECT id, nombre, email, ciudad, creado FROM clientes WHERE activo;
CREATE VIEW v_kpis AS SELECT mes, sum(ventas) AS ventas_totales, sum(pedidos) AS pedidos FROM v_ventas_mensuales GROUP BY mes;"
            .into(),
    ]
}

/// The same model and data as Tienda (DuckDB's SQL), with its differences.
/// Drops first, so a run cut short is simply run again.
fn analitica_sql() -> Vec<String> {
    let r = |from: u32, to: u32| format!("range({from}, {}) t(n)", to + 1);
    let precio = r"round(100 + (n * 37) % 9900 + 0.99, 2)";
    vec![
        r"DROP VIEW IF EXISTS v_ventas_por_ciudad".into(),
        r"DROP VIEW IF EXISTS v_kpis".into(),
        r"DROP VIEW IF EXISTS v_ventas_mensuales".into(),
        r"DROP VIEW IF EXISTS v_clientes_activos".into(),
        r"DROP MACRO IF EXISTS fn_total_pedido".into(),
        r"DROP TABLE IF EXISTS facturas".into(),
        r"DROP TABLE IF EXISTS pedido_items".into(),
        r"DROP TABLE IF EXISTS pedidos".into(),
        r"DROP TABLE IF EXISTS productos".into(),
        r"DROP TABLE IF EXISTS clientes".into(),
        r"DROP TABLE IF EXISTS auditoria".into(),
        r"DROP SEQUENCE IF EXISTS seq_auditoria".into(),
        r"CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre VARCHAR(100) NOT NULL, email VARCHAR(150) NOT NULL, ciudad VARCHAR(60) NOT NULL, activo BOOLEAN NOT NULL, creado TIMESTAMP NOT NULL)".into(),
        // Every 50th client moved to Ushuaia with a new e-mail.
        format!(
            r"INSERT INTO clientes SELECT n, {} || ' ' || {}, CASE WHEN n % 50 = 0 THEN 'nuevo' || n ELSE 'cliente' || n END || '@ejemplo.com.ar', CASE WHEN n % 50 = 0 THEN 'Ushuaia' ELSE {} END, n % 7 <> 0, TIMESTAMP '2023-01-01 09:00:00' + to_minutes((n * 431)::BIGINT) FROM {}",
            pick(NOMBRES, "n % 10"),
            pick(APELLIDOS, "(n // 10) % 10"),
            pick(CIUDADES, "n % 12"),
            r(1, CLIENTES)
        ),
        // And ten clients only Analítica has.
        format!(
            r"INSERT INTO clientes SELECT n, 'Cliente nuevo ' || n, 'nuevo' || n || '@ejemplo.com.ar', 'Río Gallegos', true, TIMESTAMP '2026-06-01 10:00:00' + to_hours((n - {CLIENTES})::BIGINT) FROM {}",
            r(CLIENTES + 1, CLIENTES + NUEVOS)
        ),
        // A column Tienda doesn't have, and every 50th price up 10%.
        r"CREATE TABLE productos (id INTEGER PRIMARY KEY, nombre VARCHAR(120) NOT NULL, categoria VARCHAR(40) NOT NULL, precio DECIMAL(12,2) NOT NULL, stock INTEGER NOT NULL)".into(),
        format!(
            r"INSERT INTO productos SELECT n, {} || ' ' || n, {}, round({precio} * CASE WHEN n % 50 = 0 THEN 1.1 ELSE 1 END, 2), (n * 17) % 300 FROM {}",
            pick(ARTICULOS, "n % 8"),
            pick(CATEGORIAS, "n % 8"),
            r(1, PRODUCTOS)
        ),
        r"CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER NOT NULL REFERENCES clientes (id), fecha DATE NOT NULL, estado VARCHAR(20) NOT NULL, total DECIMAL(14,2) NOT NULL DEFAULT 0)".into(),
        format!(
            r"INSERT INTO pedidos (id, cliente_id, fecha, estado, total) SELECT n, 1 + (n * 7) % {CLIENTES}, (DATE '2025-01-01' + to_days((n % 600)::INTEGER))::DATE, {}, 0 FROM {}",
            pick(ESTADOS, "n % 5"),
            r(1, PEDIDOS)
        ),
        r"CREATE TABLE pedido_items (pedido_id INTEGER NOT NULL REFERENCES pedidos (id), producto_id INTEGER NOT NULL REFERENCES productos (id), cantidad INTEGER NOT NULL, precio DECIMAL(12,2) NOT NULL, PRIMARY KEY (pedido_id, producto_id))".into(),
        // The items keep the list price Tienda has: the orders match.
        format!(
            r"INSERT INTO pedido_items SELECT 1 + (n - 1) // 3, p.id, 1 + n % 5, p.precio FROM {} JOIN (SELECT m AS id, round(100 + (m * 37) % 9900 + 0.99, 2) AS precio FROM range(1, {}) q(m)) p ON p.id = 1 + ((1 + (n - 1) // 3) * 13 + ((n - 1) % 3) * 101) % {PRODUCTOS}",
            r(1, ITEMS),
            PRODUCTOS + 1
        ),
        r"UPDATE pedidos SET total = x.t FROM (SELECT pedido_id, sum(cantidad * precio) AS t FROM pedido_items GROUP BY pedido_id) x WHERE x.pedido_id = pedidos.id".into(),
        r"CREATE TABLE facturas (id INTEGER PRIMARY KEY, pedido_id INTEGER NOT NULL REFERENCES pedidos (id), numero VARCHAR(20) NOT NULL, fecha DATE NOT NULL, monto DECIMAL(14,2) NOT NULL)".into(),
        r"INSERT INTO facturas SELECT id, id, 'A-0001-' || lpad(id::VARCHAR, 8, '0'), fecha + 1, total FROM pedidos".into(),
        r"CREATE SEQUENCE seq_auditoria START 1".into(),
        r"CREATE TABLE auditoria (id BIGINT PRIMARY KEY DEFAULT nextval('seq_auditoria'), tabla VARCHAR(60) NOT NULL, accion VARCHAR(120) NOT NULL, fecha TIMESTAMP NOT NULL)".into(),
        r"CREATE MACRO fn_total_pedido(p_id) AS (SELECT coalesce(sum(cantidad * precio), 0) FROM pedido_items WHERE pedido_id = p_id)".into(),
        r"CREATE VIEW v_ventas_mensuales AS SELECT strftime(p.fecha, '%Y-%m') AS mes, pr.categoria, sum(i.cantidad * i.precio) AS ventas, count(DISTINCT p.id) AS pedidos FROM pedidos p JOIN pedido_items i ON i.pedido_id = p.id JOIN productos pr ON pr.id = i.producto_id GROUP BY 1, 2".into(),
        r"CREATE VIEW v_clientes_activos AS SELECT id, nombre, email, ciudad, creado FROM clientes WHERE activo".into(),
        r"CREATE VIEW v_kpis AS SELECT mes, sum(ventas) AS ventas_totales, sum(pedidos) AS pedidos FROM v_ventas_mensuales GROUP BY mes".into(),
        r"CREATE VIEW v_ventas_por_ciudad AS SELECT c.ciudad, count(*) AS pedidos, sum(p.total) AS ventas FROM pedidos p JOIN clientes c ON c.id = p.cliente_id GROUP BY c.ciudad".into(),
        r"CHECKPOINT".into(),
    ]
}

/// Tienda's events, as DuckDB selects them (for the Parquet file).
fn eventos_select() -> String {
    let tipo = pick(TIPOS, "n % 6");
    format!(
        r"SELECT n AS id, TIMESTAMP '2025-01-01 00:00:00' + to_seconds((n * 2600)::BIGINT) AS fecha, {tipo} AS tipo, 1 + n % {CLIENTES} AS cliente_id, CASE WHEN n % 1000 = 0 THEN 'revisar cliente_' || (1 + n % {CLIENTES}) || ' con cobranzas' ELSE 'evento de ' || {tipo} || ' registrado sin novedades' END AS detalle, {} AS estado FROM range(1, {}) t(n)",
        pick(EV_ESTADOS, "n % 3"),
        EVENTOS + 1
    )
}

// -- commands ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SamplesArgs {
    pub texts: Texts,
}

/// Adds the sample folder once; the tour's path when it did now.
#[tauri::command]
pub async fn samples_install(
    state: State<'_, AppState>,
    args: SamplesArgs,
) -> CommandResult<Option<String>> {
    let _making = MAKING.lock().await;
    Ok(install_in(&state.store, &dir(), &args.texts)?.map(|p| p.to_string_lossy().into_owned()))
}

/// The tour's path, written again if it was deleted.
#[tauri::command]
pub async fn samples_tour(args: SamplesArgs) -> CommandResult<String> {
    Ok(write_tour(&dir(), &args.texts, false)?
        .to_string_lossy()
        .into_owned())
}

/// "Quitar las bases de ejemplo": their sessions closed first (an open
/// file can't be deleted on Windows).
#[tauri::command]
pub async fn samples_remove(state: State<'_, AppState>) -> CommandResult<()> {
    for id in [TIENDA_ID, ANALITICA_ID, ARCHIVOS_ID] {
        state.close_connection_sessions(id);
        state.cache_forget(id);
    }
    let _making = MAKING.lock().await;
    remove_in(&state.store, &dir())
}

#[cfg(test)]
#[path = "samples_tests.rs"]
mod tests;

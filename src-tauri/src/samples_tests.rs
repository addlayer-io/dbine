//! The sample databases: installed once and never again, removed for good,
//! the tour pointing at the three connections, and each database made on
//! first open with the data and the differences it promises. The DuckDB
//! ones load DuckDB as the app does (downloaded once into the components
//! folder, or `DBINE_DUCKDB_LIB`), like the notebook's tests.

use super::*;
use crate::notebook::{CellKind, NotebookCell};

fn texts() -> Texts {
    Texts {
        folder: "Bases de ejemplo".into(),
        tienda: "Tienda (SQLite)".into(),
        analitica: "Analítica (DuckDB)".into(),
        archivos: "Archivos (CSV y Parquet)".into(),
        tour_file: "Recorrido por DBine".into(),
        tour: TourTexts {
            intro: "# Recorrido".into(),
            differences: "## Diferencias".into(),
            only_in_analitica: "(solo en Analítica)".into(),
            targets: "## Ventas contra metas".into(),
        },
    }
}

/// A folder of the test's own, gone before and after.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("dbine-samples-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample_ids(store: &StateStore) -> Vec<String> {
    let mut v: Vec<String> = store
        .list_connections()
        .unwrap()
        .into_iter()
        .filter(|c| c.id.starts_with("dbine-sample-"))
        .map(|c| c.id)
        .collect();
    v.sort();
    v
}

fn config(store: &StateStore, id: &str) -> ConnectionConfig {
    store.get_connection(id).unwrap().unwrap().config
}

#[test]
fn installs_the_folder_its_connections_and_the_tour_once() {
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("once");
    let tour = install_in(&store, &dir.0, &texts())
        .unwrap()
        .expect("installed");
    assert!(tour.exists(), "{tour:?}");
    assert_eq!(tour.file_name().unwrap(), "Recorrido por DBine.ipynb");

    let folder = store
        .list_folders()
        .unwrap()
        .into_iter()
        .find(|f| f.id == FOLDER_ID)
        .expect("the folder");
    assert_eq!(folder.name, "Bases de ejemplo");
    assert_eq!(sample_ids(&store), [ANALITICA_ID, ARCHIVOS_ID, TIENDA_ID]);
    for (id, name, driver, file) in [
        (TIENDA_ID, "Tienda (SQLite)", "sqlite", TIENDA_FILE),
        (ANALITICA_ID, "Analítica (DuckDB)", "duckdb", ANALITICA_FILE),
        (
            ARCHIVOS_ID,
            "Archivos (CSV y Parquet)",
            "duckdb_files",
            ARCHIVOS_DIR,
        ),
    ] {
        let c = store.get_connection(id).unwrap().unwrap();
        assert_eq!(
            (
                c.name.as_str(),
                c.config.driver.as_str(),
                c.folder_id.as_deref()
            ),
            (name, driver, Some(FOLDER_ID))
        );
        assert_eq!(PathBuf::from(&c.config.host), dir.0.join(file));
    }
    // Nothing is made before a connection opens.
    assert!(!dir.0.join(TIENDA_FILE).exists());
    assert!(!dir.0.join(ANALITICA_FILE).exists());
    assert_eq!(
        store.get_setting(INSTALLED).unwrap(),
        Some(serde_json::Value::Bool(true))
    );

    // Again (another window, the next start): nothing more.
    assert_eq!(install_in(&store, &dir.0, &texts()).unwrap(), None);
    assert_eq!(sample_ids(&store).len(), 3);
    assert_eq!(
        store
            .list_folders()
            .unwrap()
            .iter()
            .filter(|f| f.id == FOLDER_ID)
            .count(),
        1
    );
}

#[test]
fn deleted_samples_never_come_back() {
    // Deleted by hand: the folder and its connections.
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("deleted");
    install_in(&store, &dir.0, &texts()).unwrap().unwrap();
    store.delete_folder(FOLDER_ID).unwrap();
    for id in [TIENDA_ID, ANALITICA_ID, ARCHIVOS_ID] {
        store.delete_connection(id).unwrap();
    }
    assert_eq!(install_in(&store, &dir.0, &texts()).unwrap(), None);
    assert!(sample_ids(&store).is_empty());
    assert!(store.list_folders().unwrap().is_empty());

    // "Quitar las bases de ejemplo": the files go too; a connection the user
    // put in the folder stays, moved up.
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("removed");
    install_in(&store, &dir.0, &texts()).unwrap().unwrap();
    let mut mine = store.get_connection(TIENDA_ID).unwrap().unwrap();
    mine.id = "mine".into();
    mine.name = "Mía".into();
    store.save_connection(&mine).unwrap();
    remove_in(&store, &dir.0).unwrap();
    assert!(sample_ids(&store).is_empty());
    assert!(store.list_folders().unwrap().is_empty());
    assert!(!dir.0.exists());
    let kept = store.get_connection("mine").unwrap().expect("not deleted");
    assert_eq!(kept.folder_id, None);
    assert_eq!(install_in(&store, &dir.0, &texts()).unwrap(), None);
    assert!(sample_ids(&store).is_empty());
}

fn cell<'a>(cells: &'a [NotebookCell], name: &str) -> &'a NotebookCell {
    cells
        .iter()
        .find(|c| c.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no cell {name}"))
}

#[test]
fn the_tour_runs_on_the_three_sample_connections() {
    let dir = TempDir::new("tour");
    let t = texts();
    let path = write_tour(&dir.0, &t, true).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains(r#""dataframes": true"#),
        "DataFrame SQL as the file writes it"
    );
    let nb = crate::notebook::parse(&raw).unwrap();
    assert_eq!(nb.origin, "dbine");
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);
    let conn = |c: &NotebookCell| c.target.as_ref().and_then(|t| t.connection.clone());
    let frames = |c: &NotebookCell| c.target.as_ref().is_some_and(|t| t.results);
    assert_eq!(
        nb.target.as_ref().and_then(|t| t.connection.as_deref()),
        Some("Tienda (SQLite)")
    );
    for (name, on) in [
        ("clientes_tienda", "Tienda (SQLite)"),
        ("clientes_analitica", "Analítica (DuckDB)"),
        ("metas", "Archivos (CSV y Parquet)"),
        ("ventas", "Tienda (SQLite)"),
    ] {
        let c = cell(&nb.cells, name);
        assert_eq!(c.kind, CellKind::Sql);
        assert_eq!(conn(c).as_deref(), Some(on), "{name}");
    }
    // The DataFrame SQL cells use the cells above, and end in charts.
    for (name, uses, kind) in [
        (
            "diferencias",
            ["clientes_tienda", "clientes_analitica"],
            "bar",
        ),
        ("ventas_vs_metas", ["ventas", "metas"], "line"),
    ] {
        let c = cell(&nb.cells, name);
        assert!(frames(c), "{name}");
        for u in uses {
            assert!(c.source.contains(u), "{name} uses {u}");
        }
        let chart = c.chart.as_ref().expect("a chart");
        assert_eq!(chart["view"], "chart");
        assert_eq!(chart["charts"][0]["type"], kind);
    }
    assert_eq!(
        nb.cells.last().unwrap().name.as_deref(),
        Some("ventas_vs_metas")
    );
    assert_eq!(nb.cells[0].kind, CellKind::Markdown);
    assert_eq!(nb.cells[0].source, "# Recorrido");
    assert!(cell(&nb.cells, "diferencias")
        .source
        .contains("'(solo en Analítica)'"));

    // Kept when there; written again when deleted.
    std::fs::write(&path, "{}").unwrap();
    assert_eq!(write_tour(&dir.0, &t, false).unwrap(), path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    std::fs::remove_file(&path).unwrap();
    write_tour(&dir.0, &t, false).unwrap();
    assert!(crate::notebook::parse(&std::fs::read_to_string(&path).unwrap()).is_ok());
    // A name with a path in it stays a name.
    assert_eq!(file_name("../../x/y"), "xy");
    assert_eq!(file_name("  "), "Recorrido");
}

fn count(c: &rusqlite::Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn tienda_is_made_on_first_open_and_left_alone_after() {
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("tienda");
    install_in(&store, &dir.0, &texts()).unwrap().unwrap();
    let cfg = config(&store, TIENDA_ID);
    ensure(TIENDA_ID, &cfg).await.unwrap();
    let file = dir.0.join(TIENDA_FILE);
    assert!(file.exists());
    assert!(!part_of(&file).exists(), "no half-made file left");
    let size = std::fs::metadata(&file).unwrap().len();
    assert!(size < 8 * 1024 * 1024, "{size} bytes");

    let c = rusqlite::Connection::open(&file).unwrap();
    for (table, n) in [
        ("clientes", CLIENTES),
        ("productos", PRODUCTOS),
        ("pedidos", PEDIDOS),
        ("pedido_items", ITEMS),
        ("facturas", PEDIDOS),
        ("eventos", EVENTOS),
    ] {
        assert_eq!(
            count(&c, &format!("SELECT count(*) FROM {table}")),
            n as i64,
            "{table}"
        );
    }
    // A view over a view, the trigger, the foreign keys.
    assert_eq!(
        count(&c, "SELECT count(*) FROM v_kpis"),
        months().len() as i64
    );
    assert_eq!(
        count(&c, "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'tr_pedidos_audit'"),
        1
    );
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM pragma_foreign_key_list('pedido_items')"
        ),
        2
    );
    assert_eq!(
        count(&c, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
    // Totals and invoices agree with the items.
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM pedidos p WHERE abs(p.total - (SELECT sum(cantidad * precio) FROM pedido_items WHERE pedido_id = p.id)) > 0.001"
        ),
        0
    );
    assert_eq!(count(&c, "SELECT count(*) FROM facturas f JOIN pedidos p ON p.id = f.pedido_id WHERE f.monto <> p.total"), 0);
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM eventos WHERE detalle LIKE 'revisar cliente_%'"
        ),
        (EVENTOS / 1000) as i64
    );
    drop(c);

    // Opened again: untouched.
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    ensure(TIENDA_ID, &cfg).await.unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    // Deleted: made again on the next open.
    std::fs::remove_file(&file).unwrap();
    ensure(TIENDA_ID, &cfg).await.unwrap();
    assert!(file.exists());

    // Pointed somewhere else by the user, or another connection: left alone.
    let mut other = cfg.clone();
    other.host = dir.0.join("mia.sqlite").to_string_lossy().into_owned();
    ensure(TIENDA_ID, &other).await.unwrap();
    assert!(!dir.0.join("mia.sqlite").exists());
    std::fs::remove_file(&file).unwrap();
    ensure("another", &cfg).await.unwrap();
    assert!(!file.exists());
}

async fn session(driver: &str, host: &str) -> Box<dyn Session> {
    let mut cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        ..Default::default()
    };
    if host.is_empty() {
        cfg.options.insert("isolated".into(), "true".into());
    }
    dbine_drivers::open_session(&cfg, None).await.unwrap()
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<serde_json::Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.pop().map(|r| r.rows).unwrap_or_default()
}

async fn one(s: &mut dyn Session, sql: &str) -> serde_json::Value {
    rows(s, sql).await.remove(0).remove(0)
}

fn n(v: &serde_json::Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v}"))
}

#[tokio::test]
async fn analitica_and_archivos_are_made_through_duckdb_and_the_tour_queries_work() {
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("duckdb");
    let t = texts();
    install_in(&store, &dir.0, &t).unwrap().unwrap();
    for id in [TIENDA_ID, ANALITICA_ID, ARCHIVOS_ID] {
        ensure(id, &config(&store, id)).await.unwrap();
    }
    let duck = dir.0.join(ANALITICA_FILE);
    assert!(duck.exists() && ready_mark(&duck).exists());
    let files = dir.0.join(ARCHIVOS_DIR);
    let mut found: Vec<String> = std::fs::read_dir(&files)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(
        found,
        [EVENTOS_FILE, METAS_FILE],
        "nothing half-written in the folder"
    );

    // Analítica: the same orders, the promised differences.
    let mut a = session("duckdb", &duck.to_string_lossy()).await;
    assert_eq!(
        n(&one(a.as_mut(), "SELECT count(*) FROM clientes").await),
        (CLIENTES + NUEVOS) as f64
    );
    assert_eq!(
        n(&one(
            a.as_mut(),
            "SELECT count(*) FROM clientes WHERE ciudad = 'Ushuaia'"
        )
        .await),
        (CLIENTES / 50) as f64
    );
    assert_eq!(n(&one(a.as_mut(), "SELECT count(*) FROM information_schema.columns WHERE table_name = 'productos' AND column_name = 'stock'").await), 1.0);
    assert_eq!(
        n(&one(
            a.as_mut(),
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'eventos'"
        )
        .await),
        0.0
    );
    assert_eq!(
        n(&one(a.as_mut(), "SELECT count(*) FROM v_ventas_por_ciudad").await),
        13.0
    );
    assert_eq!(
        n(&one(a.as_mut(), "SELECT count(*) FROM v_kpis").await),
        months().len() as f64
    );
    let duck_total = n(&one(a.as_mut(), "SELECT sum(total) FROM pedidos").await);
    drop(a);
    let tienda = rusqlite::Connection::open(dir.0.join(TIENDA_FILE)).unwrap();
    let sqlite_total: f64 = tienda
        .query_row("SELECT sum(total) FROM pedidos", [], |r| r.get(0))
        .unwrap();
    assert!(
        (duck_total - sqlite_total).abs() < 0.01,
        "{duck_total} vs {sqlite_total}"
    );

    // Opened again: not made twice.
    let before = std::fs::metadata(ready_mark(&duck))
        .unwrap()
        .modified()
        .unwrap();
    ensure(ANALITICA_ID, &config(&store, ANALITICA_ID))
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(ready_mark(&duck))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );

    // Archivos: the files engine sees both, the month as text.
    let mut f = session("duckdb_files", &files.to_string_lossy()).await;
    assert_eq!(
        n(&one(f.as_mut(), "SELECT count(*) FROM eventos").await),
        EVENTOS as f64
    );
    assert_eq!(
        n(&one(f.as_mut(), "SELECT count(*) FROM metas").await),
        (months().len() * CATEGORIAS.len()) as f64
    );
    assert_eq!(
        one(f.as_mut(), "SELECT typeof(mes) FROM metas LIMIT 1").await,
        "VARCHAR"
    );
    drop(f);

    // The tour's DataFrame SQL, on the cells' results as the notebook names them.
    let nb = tour(&t);
    let mut d = session("duckdb", "").await;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut values = Vec::new();
    let mut st = tienda
        .prepare("SELECT id, nombre, ciudad, email FROM clientes")
        .unwrap();
    let mut q = st.query([]).unwrap();
    while let Some(r) = q.next().unwrap() {
        let (id, nombre, ciudad, email): (i64, String, String, String) = (
            r.get(0).unwrap(),
            r.get(1).unwrap(),
            r.get(2).unwrap(),
            r.get(3).unwrap(),
        );
        values.push(format!(
            "({id}, {}, {}, {})",
            quote(&nombre),
            quote(&ciudad),
            quote(&email)
        ));
    }
    rows(
        d.as_mut(),
        &format!(
        "CREATE TABLE clientes_tienda AS SELECT * FROM (VALUES {}) v(id, nombre, ciudad, email)",
        values.join(", ")
    ),
    )
    .await;
    let at = quote(&duck.to_string_lossy());
    rows(d.as_mut(), &format!("ATTACH {at} AS a (READ_ONLY)")).await;
    rows(
        d.as_mut(),
        "CREATE TABLE clientes_analitica AS SELECT id, nombre, ciudad, email FROM a.clientes",
    )
    .await;
    let diff = rows(d.as_mut(), &cell(&nb.cells, "diferencias").source).await;
    let total: f64 = diff.iter().map(|r| n(&r[1])).sum();
    assert_eq!(total, (CLIENTES / 50 + NUEVOS) as f64, "{diff:?}");
    assert!(
        diff.iter()
            .any(|r| r[0] == "(solo en Analítica)" && n(&r[1]) == NUEVOS as f64),
        "{diff:?}"
    );

    let mut ventas = Vec::new();
    let mut st = tienda
        .prepare("SELECT mes, categoria, ventas FROM v_ventas_mensuales")
        .unwrap();
    let mut q = st.query([]).unwrap();
    while let Some(r) = q.next().unwrap() {
        let (mes, cat, v): (String, String, f64) =
            (r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap());
        ventas.push(format!("({}, {}, {v})", quote(&mes), quote(&cat)));
    }
    rows(
        d.as_mut(),
        &format!(
            "CREATE TABLE ventas AS SELECT * FROM (VALUES {}) v(mes, categoria, ventas)",
            ventas.join(", ")
        ),
    )
    .await;
    rows(
        d.as_mut(),
        &format!(
            "CREATE TABLE metas AS SELECT * FROM read_csv_auto({})",
            quote(&files.join(METAS_FILE).to_string_lossy())
        ),
    )
    .await;
    let vs = rows(d.as_mut(), &cell(&nb.cells, "ventas_vs_metas").source).await;
    assert_eq!(vs.len(), months().len(), "{vs:?}");
    assert!(
        vs.iter().all(|r| n(&r[1]) > 0.0 && n(&r[2]) > 0.0),
        "{vs:?}"
    );
}

#[tokio::test]
async fn opening_a_sample_connection_makes_its_database() {
    let store = StateStore::open_in_memory().unwrap();
    let dir = TempDir::new("open");
    install_in(&store, &dir.0, &texts()).unwrap().unwrap();
    let state = AppState::new(store);
    let file = dir.0.join(TIENDA_FILE);
    assert!(!file.exists());
    let cfg = state.connect_config(TIENDA_ID).await.unwrap();
    assert!(file.exists(), "made by connect_config");
    assert_eq!(PathBuf::from(cfg.host), file);
}

#[test]
fn the_ui_sends_the_texts_as_the_commands_read_them() {
    // web/src/api/samples.ts: { args: { texts } }, snake_case like the rest of the API.
    let args: SamplesArgs = serde_json::from_value(serde_json::json!({ "texts": {
        "folder": "Sample databases", "tienda": "Store (SQLite)", "analitica": "Analytics (DuckDB)",
        "archivos": "Files (CSV and Parquet)", "tour_file": "DBine tour",
        "tour": { "intro": "i", "differences": "d", "only_in_analitica": "(only in Analytics (DuckDB))", "targets": "t" },
    }}))
    .unwrap();
    assert_eq!(args.texts.tour_file, "DBine tour");
    assert_eq!(
        args.texts.tour.only_in_analitica,
        "(only in Analytics (DuckDB))"
    );
}

//! Tests for the notebook's results (`notebook::local`). The pure ones always
//! run; the DuckDB ones load DuckDB as the app does (downloaded once into
//! the components folder, or `DBINE_DUCKDB_LIB`). The live test against
//! the `dbine-test-*` containers is ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored notebook_local_live --test-threads=1 --nocapture
//! ```

use super::*;
use serde_json::json;
use std::cell::Cell as StdCell;

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn col(name: &str, ty: &str) -> ResultColumn {
    ResultColumn { name: name.into(), type_name: ty.into() }
}

fn src(name: &str, sql: &str) -> LocalSource {
    LocalSource { name: name.into(), local: false, sql: sql.into(), connection_id: Some("conn".into()), database: None, run: None, rows: None, files_key: String::new() }
}

fn with_rows(mut s: LocalSource, run: u64, columns: Vec<ResultColumn>, rows: Vec<Vec<Value>>) -> LocalSource {
    s.run = Some(run);
    s.rows = Some(LiveRows { columns, rows });
    s
}

// -- names ------------------------------------------------------------------------------------

#[test]
fn cell_names_are_plain_identifiers() {
    for ok in ["c1", "ventas_pg", "_tmp", "C2", "a".repeat(63).as_str()] {
        assert!(valid_name(ok), "{ok}");
    }
    for bad in ["", "1c", "ventas-pg", "ventas pg", "año", "select", "FROM", "a\"b", "x;drop", "a".repeat(64).as_str()] {
        assert!(!valid_name(bad), "{bad}");
    }
}

#[test]
fn references_are_names_outside_strings_and_comments() {
    let known = names(&["c1", "c2", "ventas", "clientes"]);
    let sql = "-- c2 in a comment\nSELECT v.*, 'c2' AS s, \"CLIENTES\".x /* ventas c2 */\nFROM Ventas v JOIN \"clientes\" ON clientes.id = v.c1";
    // `v.c1` is a column; clientes (a qualifier first) and ventas are tables, each once, in order.
    assert_eq!(references(sql, &known), names(&["clientes", "ventas"]));
    assert_eq!(references("SELECT * FROM c1, C1, c2", &known), names(&["c1", "c2"]));
    assert_eq!(references("SELECT $$c1$$, E'c2' FROM t", &known), Vec::<String>::new());
    assert_eq!(references("SELECT * FROM c3", &known), Vec::<String>::new());
}

#[test]
fn needed_follows_results_cells_and_only_looks_up() {
    let mut c3 = src("c3", "SELECT * FROM c1 JOIN c2 USING (id)");
    c3.local = true;
    // c4 is below the cell that names it in c3's SQL: not a source of c3.
    let mut c5 = src("c5", "SELECT * FROM c3, c4");
    c5.local = true;
    let sources = vec![src("c1", "SELECT 1"), src("c2", "SELECT 2"), c3, src("c4", "SELECT c5 FROM x"), c5];
    assert_eq!(needed("SELECT * FROM c3", &sources), vec![0, 1, 2]);
    assert_eq!(needed("SELECT * FROM c5", &sources), vec![0, 1, 2, 3, 4]);
    assert_eq!(needed("SELECT * FROM c4 JOIN c1 ON true", &sources), vec![3, 0]);
}

// -- types ------------------------------------------------------------------------------------

fn ty(type_name: &str, values: &[Value]) -> DuckType {
    infer(type_name, values.iter())
}

#[test]
fn result_types_map_to_duckdb_types() {
    assert_eq!(ty("int4", &[json!(1), json!(null)]), DuckType::BigInt);
    assert_eq!(ty("bigint", &[json!("9007199254740993")]), DuckType::BigInt);
    assert_eq!(ty("UInt64", &[json!("18446744073709551615")]), DuckType::HugeInt);
    assert_eq!(ty("Nullable(Int32)", &[json!(3)]), DuckType::BigInt);
    assert_eq!(ty("numeric(10,2)", &[json!("12.50")]), DuckType::Decimal(10, 2));
    assert_eq!(ty("money", &[json!("1.2345")]), DuckType::Decimal(19, 4));
    assert_eq!(ty("numeric", &[json!("1e3")]), DuckType::Double);
    assert_eq!(ty("Decimal", &[json!("12.50"), json!("1.5")]), DuckType::Decimal(4, 2));
    assert_eq!(ty("float8", &[json!(1.5), json!(2)]), DuckType::Double);
    assert_eq!(ty("bit", &[json!(true), json!(0), json!(1)]), DuckType::Boolean);
    assert_eq!(ty("date", &[json!("2026-10-18")]), DuckType::Date);
    assert_eq!(ty("datetime2", &[json!("2026-10-18 14:32:05.1230000")]), DuckType::Timestamp);
    assert_eq!(ty("timestamptz", &[json!("2026-10-18 14:32:05+00:00")]), DuckType::TimestampTz);
    assert_eq!(ty("uniqueidentifier", &[json!("6F9619FF-8B86-D011-B42D-00C04FC964FF")]), DuckType::Uuid);
    assert_eq!(ty("jsonb", &[json!({"a": 1}), json!("[1,2]")]), DuckType::Json);
    assert_eq!(ty("nvarchar(50)", &[json!("x")]), DuckType::Varchar);
    // The type name says one thing, the values another: text.
    assert_eq!(ty("date", &[json!("ayer")]), DuckType::Varchar);
    assert_eq!(ty("int", &[json!("n/a")]), DuckType::Varchar);
    // No type name (MongoDB, Redis…): what the values are.
    assert_eq!(ty("", &[json!(1), json!(2)]), DuckType::BigInt);
    assert_eq!(ty("", &[json!(1), json!(2.5)]), DuckType::Double);
    assert_eq!(ty("", &[json!(true)]), DuckType::Boolean);
    assert_eq!(ty("", &[json!({"x": 1}), json!([1])]), DuckType::Json);
    assert_eq!(ty("", &[json!(1), json!("a")]), DuckType::Varchar);
    // PostgreSQL gives no type names: its numerics, dates and UUIDs come as text.
    assert_eq!(ty("", &[json!("12.50"), json!("-7.125"), json!(3)]), DuckType::Decimal(5, 3));
    assert_eq!(ty("", &[json!("9223372036854775808")]), DuckType::HugeInt);
    assert_eq!(ty("", &[json!("42")]), DuckType::BigInt);
    assert_eq!(ty("", &[json!("007"), json!("12")]), DuckType::Varchar);
    assert_eq!(ty("", &[json!("1e3")]), DuckType::Varchar);
    assert_eq!(ty("", &[json!("2026-10-01"), json!(null)]), DuckType::Date);
    assert_eq!(ty("", &[json!("2026-10-01 10:00:00")]), DuckType::Timestamp);
    assert_eq!(ty("", &[json!("2026-10-01 10:00:00+02")]), DuckType::TimestampTz);
    assert_eq!(ty("", &[json!("6f9619ff-8b86-d011-b42d-00c04fc964ff")]), DuckType::Uuid);
    assert_eq!(ty("Guid", &[json!("6F9619FF-8B86-D011-B42D-00C04FC964FF")]), DuckType::Uuid);
    assert_eq!(ty("", &[json!(null)]), DuckType::Varchar);
    // All null: the type name decides.
    assert_eq!(ty("int", &[json!(null)]), DuckType::BigInt);
}

#[test]
fn values_become_cells_of_their_column_type() {
    assert_eq!(to_cell(&json!(null), DuckType::BigInt), Cell::Null);
    assert_eq!(to_cell(&json!("42"), DuckType::BigInt), Cell::Int(42));
    assert_eq!(to_cell(&json!(1), DuckType::Boolean), Cell::Bool(true));
    assert_eq!(to_cell(&json!(2), DuckType::Double), Cell::Float(2.0));
    assert_eq!(to_cell(&json!("12.50"), DuckType::Decimal(10, 2)), Cell::Text("12.50".into()));
    assert_eq!(to_cell(&json!({"a": 1}), DuckType::Json), Cell::Text("{\"a\":1}".into()));
    assert_eq!(to_cell(&json!(true), DuckType::Varchar), Cell::Text("true".into()));
}

#[test]
fn column_names_are_unique_ignoring_case() {
    let cols = vec![col("id", ""), col("ID", ""), col("", ""), col("id", ""), col("x", "")];
    assert_eq!(column_names(&cols), names(&["id", "ID_2", "col3", "id_3", "x"]));
}

#[test]
fn a_table_is_loaded_again_only_when_its_source_changes() {
    let a = with_rows(src("c1", "SELECT 1"), 3, vec![], vec![]);
    let k = source_key(&a, 100, &[]);
    assert_eq!(k, source_key(&a.clone(), 100, &[]));
    // Ran again.
    assert_ne!(k, source_key(&with_rows(src("c1", "SELECT 1"), 4, vec![], vec![]), 100, &[]));
    // Its text changed.
    assert_ne!(k, source_key(&with_rows(src("c1", "SELECT 2"), 3, vec![], vec![]), 100, &[]));
    // Another connection, another cap, a source's key changed.
    let mut b = a.clone();
    b.connection_id = Some("other".into());
    assert_ne!(k, source_key(&b, 100, &[]));
    assert_ne!(k, source_key(&a, 200, &[]));
    assert_ne!(source_key(&a, 100, &["x".into()]), source_key(&a, 100, &["y".into()]));
    // Without rows of a run, its query runs again: a key of its own.
    assert_ne!(k, source_key(&src("c1", "SELECT 1"), 100, &[]));
    // Whitespace around the text doesn't count; the name's case neither.
    assert_eq!(source_key(&src("C1", " SELECT 1\n"), 100, &[]), source_key(&src("c1", "SELECT 1"), 100, &[]));
}

// -- DuckDB -----------------------------------------------------------------------------------

async fn rows_of(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.into_iter().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default()
}

/// The error a statement gets (it must fail).
async fn refused(s: &mut dyn Session, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 10, &mut out).await {
        Err(e) => e.to_string(),
        Ok(()) => out.error.unwrap_or_else(|| panic!("{sql} ran")),
    }
}

#[tokio::test]
async fn notebook_local_sandbox_refuses_files_network_and_settings() {
    let mut s = open_sandboxed().await.expect("DuckDB");
    let dir = std::env::temp_dir().join(format!("dbine-nbx-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("secret.csv");
    std::fs::write(&csv, "a,b\n1,2\n").unwrap();
    let out_csv = dir.join("out.csv");
    let db = dir.join("other.duckdb");
    let p = |p: &std::path::Path| p.to_string_lossy().replace('\'', "''");
    for sql in [
        format!("SELECT * FROM read_csv('{}')", p(&csv)),
        format!("SELECT * FROM '{}'", p(&csv)),
        format!("SELECT * FROM read_text('{}')", p(&csv)),
        format!("COPY (SELECT 1 AS x) TO '{}'", p(&out_csv)),
        format!("ATTACH '{}' AS other", p(&db)),
        "INSTALL httpfs".to_string(),
        "LOAD httpfs".to_string(),
        "SELECT * FROM read_csv('https://example.com/x.csv')".to_string(),
        "SET enable_external_access = true".to_string(),
        "RESET lock_configuration".to_string(),
        "SET lock_configuration = false".to_string(),
        "SET temp_directory = '/tmp'".to_string(),
        "EXPORT DATABASE '/tmp/dbine-nbx-export'".to_string(),
        "SELECT * FROM glob('/*')".to_string(),
    ] {
        let e = refused(s.as_mut(), &sql).await;
        eprintln!("{sql} -> {e}");
        let why = e.to_lowercase();
        assert!(why.contains("disabled") || why.contains("locked") || why.contains("permission"), "{sql}: {e}");
    }
    assert!(!out_csv.exists(), "COPY TO wrote a file");
    assert!(!db.exists(), "ATTACH created a file");
    assert!(!std::path::Path::new("/tmp/dbine-nbx-export").exists());
    // Still a working database: tables in memory.
    rows_of(s.as_mut(), "CREATE TABLE t AS SELECT 1 AS x").await;
    assert_eq!(rows_of(s.as_mut(), "SELECT x FROM t").await, vec![vec![json!(1)]]);
    // In memory: no file named after it in the working directory.
    let stray = std::fs::read_dir(".").unwrap().filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with(":memory:"));
    assert!(!stray);
    // Two notebooks: two databases.
    let mut other = open_sandboxed().await.unwrap();
    assert!(!refused(other.as_mut(), "SELECT * FROM t").await.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_local_tables_load_with_types_and_cache() {
    let mut s = open_sandboxed().await.expect("DuckDB");
    let pg = with_rows(
        src("ventas", "SELECT * FROM ventas"),
        1,
        vec![col("id", "int4"), col("cliente", "int4"), col("total", "numeric(10,2)"), col("dia", "date"), col("extra", "jsonb")],
        vec![
            vec![json!(1), json!(10), json!("12.50"), json!("2026-10-01"), json!({"canal": "web"})],
            vec![json!(2), json!(20), json!("7.25"), json!("2026-10-02"), json!(null)],
            vec![json!(3), json!(10), json!("1.00"), json!(null), json!([1, 2])],
        ],
    );
    let ms = src("clientes", "SELECT id, nombre FROM clientes");
    let reruns = StdCell::new(0);
    let fetch = |_i: usize| {
        reruns.set(reruns.get() + 1);
        async { Ok((vec![col("id", "int"), col("nombre", "nvarchar(50)")], vec![vec![json!(10), json!("Ana")], vec![json!(20), json!("Luis")]], false)) }
    };
    let sources = vec![pg.clone(), ms.clone()];
    let mut tables = HashMap::new();
    let sql = "SELECT c.nombre, sum(v.total) AS total FROM ventas v JOIN clientes c ON c.id = v.cliente GROUP BY 1 ORDER BY 1";
    let mut out = QueryOutcome::default();
    let loaded = materialize(s.as_mut(), &mut tables, &HashSet::new(), sql, &sources, 1_000, &mut out, fetch).await.unwrap();
    assert_eq!(loaded.iter().map(|l| (l.name.as_str(), l.rows, l.cached, l.origin)).collect::<Vec<_>>(), vec![("ventas", 3, false, "run"), ("clientes", 2, false, "query")]);
    assert_eq!(reruns.get(), 1);
    assert!(out.log.is_empty(), "{:?}", out.log);
    let types = rows_of(s.as_mut(), "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'ventas' ORDER BY ordinal_position").await;
    let types: Vec<String> = types.iter().map(|r| r[1].as_str().unwrap().to_string()).collect();
    assert_eq!(types, ["BIGINT", "BIGINT", "DECIMAL(10,2)", "DATE", "JSON"]);
    assert_eq!(rows_of(s.as_mut(), sql).await, vec![vec![json!("Ana"), json!("13.50")], vec![json!("Luis"), json!("7.25")]]);

    // Again, nothing changed: both cached, no query runs again.
    let present = existing_tables(s.as_mut()).await.unwrap();
    let mut out = QueryOutcome::default();
    let loaded = materialize(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, &mut out, fetch).await.unwrap();
    assert!(loaded.iter().all(|l| l.cached), "{loaded:?}");
    assert_eq!(reruns.get(), 1);

    // The source ran again (another run): only that one is loaded again.
    let pg2 = with_rows(src("ventas", "SELECT * FROM ventas"), 2, vec![col("id", "int4"), col("cliente", "int4"), col("total", "numeric(10,2)")], vec![vec![json!(9), json!(20), json!("100.00")]]);
    let sources = vec![pg2, ms.clone()];
    let mut out = QueryOutcome::default();
    let loaded = materialize(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, &mut out, fetch).await.unwrap();
    assert_eq!(loaded.iter().map(|l| l.cached).collect::<Vec<_>>(), vec![false, true]);
    assert_eq!(rows_of(s.as_mut(), sql).await, vec![vec![json!("Luis"), json!("100.00")]]);

    // A table the cell's SQL dropped is loaded again.
    rows_of(s.as_mut(), "DROP TABLE clientes").await;
    let present = existing_tables(s.as_mut()).await.unwrap();
    let mut out = QueryOutcome::default();
    let loaded = materialize(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, &mut out, fetch).await.unwrap();
    assert_eq!(loaded.iter().map(|l| l.cached).collect::<Vec<_>>(), vec![true, false]);
    assert_eq!(reruns.get(), 2);
}

#[tokio::test]
async fn notebook_local_caps_rows_and_falls_back_to_text() {
    let mut s = open_sandboxed().await.expect("DuckDB");
    let rows: Vec<Vec<Value>> = (0..5).map(|i| vec![json!(i), json!(if i == 3 { "2026-13-45" } else { "2026-10-01" })]).collect();
    let many = with_rows(src("c1", "SELECT …"), 1, vec![col("n", "int"), col("d", "date")], rows);
    let mut c2 = src("c2", "SELECT n * 10 AS n10 FROM c1 ORDER BY n");
    c2.local = true;
    let sources = vec![many, c2];
    let never = |_i: usize| async { Err::<Fetched, _>(CommandError::Internal("no".into())) };
    let mut out = QueryOutcome::default();
    let loaded = materialize(s.as_mut(), &mut HashMap::new(), &HashSet::new(), "SELECT * FROM c2", &sources, 4, &mut out, never).await.unwrap();
    // c1: 4 of 5 rows; its date "2026-13-45" isn't a date: text.
    assert_eq!(loaded[0].rows, 4);
    assert!(loaded[0].capped && loaded[0].as_text, "{loaded:?}");
    assert_eq!(loaded[1].origin, "local");
    // Only that column: n is still a number.
    assert_eq!(rows_of(s.as_mut(), "SELECT typeof(n), typeof(d) FROM c1 LIMIT 1").await, vec![vec![json!("BIGINT"), json!("VARCHAR")]]);
    assert_eq!(rows_of(s.as_mut(), "SELECT count(*) FROM c2").await, vec![vec![json!(4)]]);
    let texts: Vec<&str> = out.log.iter().map(|m| m.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("«c1»: sus columnas d tienen valores")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("«c1» se cargó con las primeras 4 filas")), "{texts:?}");

    // A results cell with two statements can't be a table.
    let mut bad = src("c3", "SELECT 1; SELECT 2");
    bad.local = true;
    let e = materialize(s.as_mut(), &mut HashMap::new(), &HashSet::new(), "SELECT * FROM c3", &[bad], 4, &mut QueryOutcome::default(), never).await.unwrap_err();
    assert!(e.to_string().contains("varias sentencias"), "{e}");
}

#[tokio::test]
async fn notebook_local_a_write_is_never_run_again() {
    // A source that writes, on an engine without a server read-only mode
    // for this path: the guard refuses it before it reaches the server.
    let dir = std::env::temp_dir().join(format!("dbine-nbx-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w.sqlite");
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let d = dbine_drivers::find("sqlite").unwrap();
    {
        let mut w = d.connect(&cfg, None).await.unwrap();
        rows_of(w.as_mut(), "CREATE TABLE t (x INTEGER)").await;
    }
    let ro = ConnectionConfig { read_only: true, ..cfg.clone() };
    let mut s = dbine_drivers::open_session(&ro, None).await.unwrap();
    let (cancel, cancelled) = (Notify::new(), std::sync::atomic::AtomicBool::new(false));
    let stop = Stop { cancel: &cancel, cancelled: &cancelled, interrupter: None };
    let e = read_source(d.as_ref(), s.as_mut(), "c1", "INSERT INTO t VALUES (1) RETURNING x", 10, &stop).await.unwrap_err();
    assert!(e.to_string().contains("no es solo una lectura") || e.to_string().contains("falló"), "{e}");
    let (cols, rows, capped) = read_source(d.as_ref(), s.as_mut(), "c1", "SELECT x FROM t", 10, &stop).await.unwrap();
    assert_eq!((cols.len(), rows.len(), capped), (1, 0, false));
    drop(s);
    let mut w = d.connect(&cfg, None).await.unwrap();
    assert_eq!(rows_of(w.as_mut(), "SELECT count(*) FROM t").await, vec![vec![json!(0)]]);
    let _ = std::fs::remove_dir_all(&dir);
}

// -- the chart over the whole result ---------------------------------------------------------

fn test_state() -> AppState {
    AppState::new(dbine_core::StateStore::open_in_memory().unwrap())
}

/// `ventas` (60 rows: total 1..=60, cliente 10 for the even ones, 20 for
/// the odd ones) and `clientes` (10 Ana, 20 Luis), both from their runs.
fn chart_sources(run: u64, rows: u64) -> Vec<LocalSource> {
    let ventas = with_rows(
        src("ventas", "SELECT * FROM ventas"),
        run,
        vec![col("id", "int4"), col("cliente", "int4"), col("total", "int4")],
        (1..=rows).map(|i| vec![json!(i), json!(if i % 2 == 0 { 10 } else { 20 }), json!(i)]).collect(),
    );
    let clientes = with_rows(src("clientes", "SELECT id, nombre FROM clientes"), 1, vec![col("id", "int"), col("nombre", "varchar")], vec![vec![json!(10), json!("Ana")], vec![json!(20), json!("Luis")]]);
    vec![ventas, clientes]
}

const CHART_CELL: &str = "SELECT c.nombre, v.total FROM ventas v JOIN clientes c ON c.id = v.cliente";

fn group(x: Option<&str>, measures: Vec<(Option<&str>, AggFn)>) -> ChartKind {
    ChartKind::Group(AggregateSpec { x: x.map(String::from), split: None, measures: measures.into_iter().map(|(c, f)| Measure { column: c.map(String::from), func: f }).collect(), max_groups: 1_000 })
}

#[tokio::test]
async fn notebook_local_whole_result_chart_aggregates_in_duckdb() {
    let state = test_state();
    let tab = "chart-tab";
    // The cell shows 5 rows of its 60.
    let r = run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(1, 60), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    assert!(r.outcome.error.is_none(), "{:?}", r.outcome.error);
    assert_eq!(r.outcome.results.iter().find(|x| !x.columns.is_empty()).unwrap().rows.len(), 5);

    // Grouped: DuckDB's GROUP BY over every row.
    let a = aggregate_local(&state, tab, CHART_CELL, &group(Some("nombre"), vec![(Some("total"), AggFn::Sum), (None, AggFn::Count)])).await.unwrap();
    assert_eq!(a.method, AggregateMethod::Server);
    assert_eq!(a.rows_scanned, Some(60));
    assert_eq!(a.categories, vec![Some("Ana".to_string()), Some("Luis".to_string())]);
    assert_eq!(a.series[0].values, vec![Some(930.0), Some(900.0)]);
    assert_eq!(a.series[1].values, vec![Some(30.0), Some(30.0)]);

    // Total (a KPI): one row of measures.
    let a = aggregate_local(&state, tab, CHART_CELL, &group(None, vec![(Some("total"), AggFn::Sum), (Some("total"), AggFn::Count)])).await.unwrap();
    assert_eq!((a.method, a.rows_scanned), (AggregateMethod::Server, Some(60)));
    assert_eq!(a.series.iter().map(|x| x.values[0]).collect::<Vec<_>>(), vec![Some(1830.0), Some(60.0)]);

    // Groups cut: the rows are still counted by DuckDB.
    let mut cut = group(Some("total"), vec![(None, AggFn::Count)]);
    if let ChartKind::Group(spec) = &mut cut {
        spec.max_groups = 10;
    }
    let a = aggregate_local(&state, tab, CHART_CELL, &cut).await.unwrap();
    assert!(a.groups_truncated);
    assert_eq!(a.rows_scanned, Some(60));

    // Histogram and box plot: every row, read from the same session.
    let a = aggregate_local(&state, tab, CHART_CELL, &ChartKind::Histogram { column: "total".into(), bins: 6 }).await.unwrap();
    let h = a.histogram.expect("histogram");
    assert_eq!(h.counts.iter().sum::<u64>(), 60);
    assert_eq!((h.edges[0], *h.edges.last().unwrap()), (1.0, 60.0));
    assert_eq!((a.rows_scanned, a.rows_capped), (Some(60), false));
    let a = aggregate_local(&state, tab, CHART_CELL, &ChartKind::Box { column: "total".into(), by: Some("nombre".into()), max_groups: 50 }).await.unwrap();
    let boxes = a.boxes.expect("boxes");
    assert_eq!(boxes.iter().map(|b| (b.category.clone(), b.count, b.min, b.max)).collect::<Vec<_>>(), vec![(Some("Ana".into()), 30, 2.0, 60.0), (Some("Luis".into()), 30, 1.0, 59.0)]);
    assert_eq!(boxes[0].median, 31.0);
}

#[tokio::test]
async fn notebook_local_whole_result_chart_refuses_and_asks_to_run() {
    let state = test_state();
    let tab = "chart-tab-2";
    let kpi = group(None, vec![(None, AggFn::Count)]);
    // Nothing ran in this tab: no session.
    let e = aggregate_local(&state, tab, CHART_CELL, &kpi).await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda"), "{e}");
    run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(1, 60), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    // A cell that didn't run in it.
    let e = aggregate_local(&state, tab, "SELECT * FROM ventas", &kpi).await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda"), "{e}");
    // The same refusals as a run, before anything is wrapped.
    let e = aggregate_local(&state, tab, "COPY (SELECT * FROM ventas) TO '/tmp/dbine-nbx-chart.csv'", &kpi).await.unwrap_err();
    assert!(matches!(e, CommandError::BadRequest(_)), "{e}");
    assert!(!std::path::Path::new("/tmp/dbine-nbx-chart.csv").exists());
    // Another cell loaded `ventas` again (its source ran again): asked to run this one again.
    run_local(&state, tab, "SELECT count(*) FROM ventas", 5, 1_000, &chart_sources(2, 80), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    let e = aggregate_local(&state, tab, CHART_CELL, &kpi).await.unwrap_err();
    assert!(e.to_string().contains("cambiaron"), "{e}");
    run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(2, 80), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    let a = aggregate_local(&state, tab, CHART_CELL, &kpi).await.unwrap();
    assert_eq!(a.rows_scanned, Some(80));
    // The tab closed (its session with it).
    state.sessions.remove(&session_key(tab));
    let e = aggregate_local(&state, tab, CHART_CELL, &kpi).await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda"), "{e}");
}

// -- "Exportar todo" and later cells over the whole result -------------------------------------

/// `sql` exported by [`export_local`] as CSV into a fresh file: its lines,
/// or the refusal (and then no file is left).
async fn export_csv(state: &AppState, tab: &str, sql: &str, name: &str) -> Result<Vec<String>, CommandError> {
    let path = std::env::temp_dir().join(format!("dbine-nbx-export-{name}-{}.csv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let exporter = Arc::new(Mutex::new(dbine_core::export::Exporter::new(&path, 0, dbine_core::export::ExportOptions::default())));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(exporter.clone())), ..Default::default() };
    let r = export_local(state, tab, sql, &format!("export:{name}"), &mut out).await;
    out.sink = None;
    // Only this export holds the session under its key, and only while it runs.
    assert!(state.sessions.get(&format!("export:{name}")).is_none());
    let written = exporter.lock().unwrap().finish();
    match r {
        Ok(()) => {
            written.unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            Ok(text.lines().map(String::from).collect())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

#[tokio::test]
async fn notebook_local_export_writes_every_row_of_the_cell() {
    let state = test_state();
    let tab = "export-tab";
    // The cell joins two DataFrames and shows 5 of its 60 rows.
    let r = run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(1, 60), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    let shown = r.outcome.results.iter().find(|x| !x.columns.is_empty()).unwrap();
    assert_eq!((shown.rows.len(), shown.total_rows), (5, 60));
    let lines = export_csv(&state, tab, CHART_CELL, "all").await.unwrap();
    assert_eq!(lines.len(), 61, "header + every row");
    assert_eq!(lines[0], "nombre,total");
    let mut totals: Vec<u64> = lines[1..].iter().map(|l| l.split(',').nth(1).unwrap().parse().unwrap()).collect();
    totals.sort_unstable();
    assert_eq!(totals, (1..=60).collect::<Vec<_>>());
}

#[tokio::test]
async fn notebook_local_export_refuses_and_asks_to_run() {
    let state = test_state();
    let tab = "export-tab-2";
    // Nothing ran in this tab.
    let e = export_csv(&state, tab, CHART_CELL, "none").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda para exportar"), "{e}");
    run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(1, 60), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    // A cell that didn't run in it.
    let e = export_csv(&state, tab, "SELECT * FROM ventas", "other").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda"), "{e}");
    // Writing a file from the SQL is refused, before anything runs.
    let copy = std::env::temp_dir().join("dbine-nbx-export-copy.csv");
    let _ = std::fs::remove_file(&copy);
    let e = export_csv(&state, tab, &format!("COPY (SELECT * FROM ventas) TO '{}'", copy.display()), "copy").await.unwrap_err();
    assert!(matches!(e, CommandError::BadRequest(_)), "{e}");
    assert!(!copy.exists());
    // Another cell loaded `ventas` again: the DataFrames this cell read are gone.
    run_local(&state, tab, "SELECT count(*) FROM ventas", 5, 1_000, &chart_sources(2, 80), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    let e = export_csv(&state, tab, CHART_CELL, "changed").await.unwrap_err();
    assert!(e.to_string().contains("ejecutala de nuevo para exportar"), "{e}");
    // Run again: every row of the new run.
    run_local(&state, tab, CHART_CELL, 5, 1_000, &chart_sources(2, 80), None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    assert_eq!(export_csv(&state, tab, CHART_CELL, "again").await.unwrap().len(), 81);
    // The tab closed (its session with it).
    state.sessions.remove(&session_key(tab));
    let e = export_csv(&state, tab, CHART_CELL, "closed").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutá la celda"), "{e}");
}

#[tokio::test]
async fn notebook_local_a_later_cell_reads_the_whole_result_not_the_rows_shown() {
    let state = test_state();
    let tab = "chain-tab";
    // c2: a DataFrame SQL cell joining two DataFrames; it showed 5 of its 60 rows.
    let mut sources = chart_sources(1, 60);
    run_local(&state, tab, CHART_CELL, 5, 1_000, &sources, None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    sources.push(LocalSource { local: true, connection_id: None, ..src("c2", CHART_CELL) });
    // c3 over c2: DuckDB builds c2 from its SQL, every row (up to the cap per table).
    let r = run_local(&state, tab, "SELECT count(*) AS n, sum(total) AS s FROM c2", 5, 1_000, &sources, None, None, super::super::access::Policy::default(), Default::default()).await.unwrap();
    assert!(r.outcome.error.is_none(), "{:?}", r.outcome.error);
    let row = &r.outcome.results.iter().find(|x| !x.columns.is_empty()).unwrap().rows[0];
    assert_eq!((row[0].as_u64(), row[1].as_f64()), (Some(60), Some(1830.0)));
    let c2 = r.loaded.iter().find(|t| t.name == "c2").unwrap();
    assert_eq!((c2.rows, c2.capped, c2.origin), (60, false, "local"));
}

// -- live: several engines into one DuckDB join ------------------------------------------------

fn url(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

fn server_cfg(driver: &str, u: &str) -> ConnectionConfig {
    let rest = u.split_once("://").map_or(u, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap_or(0),
        username: (!user.is_empty()).then(|| user.to_string()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    }
}

async fn fetch_live(cfg: &ConnectionConfig, name: &str, query: &str) -> Fetched {
    let d = dbine_drivers::find(&cfg.driver).unwrap();
    let ro = ConnectionConfig { read_only: true, ..cfg.clone() };
    let db = (!cfg.database.is_empty()).then_some(cfg.database.as_str());
    let mut s = dbine_drivers::open_session(&ro, db).await.unwrap_or_else(|e| panic!("{}: {e}", cfg.driver));
    let (cancel, cancelled) = (Notify::new(), std::sync::atomic::AtomicBool::new(false));
    let stop = Stop { cancel: &cancel, cancelled: &cancelled, interrupter: None };
    read_source(d.as_ref(), s.as_mut(), name, query, DEFAULT_ROW_CAP, &stop).await.unwrap_or_else(|e| panic!("{}: {e}", cfg.driver))
}

#[tokio::test]
#[ignore]
async fn notebook_local_live_postgres_sqlserver_mongodb_redis() {
    // PostgreSQL: sales.
    let mut pg = server_cfg("postgres", &url("DBINE_TEST_POSTGRES_URL", "postgres://postgres:pw@localhost:25010"));
    pg.database = "postgres".into();
    {
        let mut w = dbine_drivers::open_session(&pg, Some("postgres")).await.unwrap();
        rows_of(w.as_mut(), "DROP TABLE IF EXISTS dbine_nbx_ventas").await;
        rows_of(w.as_mut(), "CREATE TABLE dbine_nbx_ventas (id int, cliente int, total numeric(10,2), dia date)").await;
        rows_of(w.as_mut(), "INSERT INTO dbine_nbx_ventas VALUES (1, 10, 12.50, '2026-10-01'), (2, 20, 7.25, '2026-10-02'), (3, 10, 1.00, '2026-10-03'), (4, 30, 5.00, '2026-10-03')").await;
    }
    // SQL Server: customers.
    let ms = server_cfg("sqlserver", &url("DBINE_TEST_SQLSERVER_URL", "mssql://sa:Pw_12345!@localhost:25013"));
    {
        let mut w = dbine_drivers::open_session(&ms, None).await.unwrap();
        rows_of(w.as_mut(), "IF OBJECT_ID('dbo.dbine_nbx_clientes') IS NOT NULL DROP TABLE dbo.dbine_nbx_clientes").await;
        rows_of(w.as_mut(), "CREATE TABLE dbo.dbine_nbx_clientes (id int, nombre nvarchar(50), alta datetime2, guid uniqueidentifier)").await;
        rows_of(w.as_mut(), "INSERT INTO dbo.dbine_nbx_clientes VALUES (10, N'Ana', '2026-01-02 03:04:05', NEWID()), (20, N'Luis', '2026-02-03 04:05:06', NEWID()), (30, N'Eva', NULL, NULL)").await;
    }
    // MongoDB: regions, as documents.
    let mut mg = ConnectionConfig { driver: "mongodb".into(), database: "dbine_nbx_db".into(), ..Default::default() };
    mg.options.insert("connection_string".into(), url("DBINE_TEST_MONGODB_URL", "mongodb://root:secret@localhost:25201/?authSource=admin"));
    {
        let mut w = dbine_drivers::open_session(&mg, Some("dbine_nbx_db")).await.unwrap();
        rows_of(w.as_mut(), "db.regiones.drop()").await;
        rows_of(w.as_mut(), r#"db.regiones.insertMany([{cliente: 10, region: "norte", tags: ["a"]}, {cliente: 20, region: "sur", tags: []}, {cliente: 30, region: "norte"}])"#).await;
    }
    // Redis: a hash per customer's score.
    let rd = server_cfg("redis", &url("DBINE_TEST_REDIS_URL", "redis://localhost:25400"));
    {
        let mut w = dbine_drivers::open_session(&rd, None).await.unwrap();
        rows_of(w.as_mut(), "HSET dbine:nbx:puntos 10 5 20 3 30 9").await;
    }

    let (pg_cols, pg_rows, _) = fetch_live(&pg, "ventas_pg", "SELECT id, cliente, total, dia FROM dbine_nbx_ventas ORDER BY id").await;
    let (ms_cols, ms_rows, _) = fetch_live(&ms, "clientes_mssql", "SELECT id, nombre, alta, guid FROM dbo.dbine_nbx_clientes").await;
    let (mg_cols, mg_rows, _) = fetch_live(&mg, "regiones_mongo", "db.regiones.find({}, {_id: 0})").await;
    let (rd_cols, rd_rows, _) = fetch_live(&rd, "puntos_redis", "HGETALL dbine:nbx:puntos").await;
    eprintln!("pg {pg_cols:?}\nms {ms_cols:?}\nmongo {mg_cols:?} {mg_rows:?}\nredis {rd_cols:?} {rd_rows:?}");

    let sources = vec![
        with_rows(src("ventas_pg", "q1"), 1, pg_cols, pg_rows),
        with_rows(src("clientes_mssql", "q2"), 2, ms_cols, ms_rows),
        with_rows(src("regiones_mongo", "q3"), 3, mg_cols, mg_rows),
        with_rows(src("puntos_redis", "q4"), 4, rd_cols, rd_rows),
    ];
    let mut duck = open_sandboxed().await.expect("DuckDB");
    let never = |_i: usize| async { Err::<Fetched, _>(CommandError::Internal("no".into())) };
    let sql = "SELECT c.nombre, r.region, sum(v.total) AS total, count(*) AS ventas \
               FROM ventas_pg v JOIN clientes_mssql c ON c.id = v.cliente JOIN regiones_mongo r ON r.cliente = v.cliente \
               GROUP BY ALL ORDER BY total DESC";
    let mut out = QueryOutcome::default();
    let loaded = materialize(duck.as_mut(), &mut HashMap::new(), &HashSet::new(), sql, &sources, DEFAULT_ROW_CAP, &mut out, never).await.unwrap();
    eprintln!("{loaded:?}\n{:?}", out.log);
    assert_eq!(loaded.iter().map(|l| l.rows).collect::<Vec<_>>(), vec![4, 3, 3]);
    let got = rows_of(duck.as_mut(), sql).await;
    eprintln!("{got:?}");
    assert_eq!(got, vec![
        vec![json!("Ana"), json!("norte"), json!("13.50"), json!(2)],
        vec![json!("Luis"), json!("sur"), json!("7.25"), json!(1)],
        vec![json!("Eva"), json!("norte"), json!("5.00"), json!(1)],
    ]);
    let types = rows_of(duck.as_mut(), "SELECT table_name, column_name, data_type FROM information_schema.columns ORDER BY table_name, ordinal_position").await;
    eprintln!("{types:?}");
    // Redis too, in its own query.
    let sql = "SELECT * FROM puntos_redis";
    materialize(duck.as_mut(), &mut HashMap::new(), &HashSet::new(), sql, &sources, DEFAULT_ROW_CAP, &mut out, never).await.unwrap();
    let got = rows_of(duck.as_mut(), sql).await;
    eprintln!("{got:?}");
    assert_eq!(got.len(), 3);
}

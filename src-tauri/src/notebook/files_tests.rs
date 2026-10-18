//! Tests for local data files in results cells (`notebook::files`). The pure
//! ones always run; the DuckDB ones load DuckDB as the app does. The
//! workbook test needs DuckDB's `excel` extension (fetched once) and says
//! so when it can't get it.

use super::*;
use crate::notebook::local::{existing_tables, open_sandboxed, prepare, Fetched, LiveRows, LocalSource};
use dbine_driver::ResultColumn;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbine-nbfiles-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

// -- detection --------------------------------------------------------------------------------

#[test]
fn reader_calls_are_found_outside_strings_and_comments() {
    let sql = "SELECT * FROM c2 c JOIN read_csv('/Users/x/Downloads/CustomerTest.csv') t ON t.CustomerID = c.CustomerID";
    let c = reader_calls(sql).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].function.as_str(), c[0].paths.clone(), c[0].list, c[0].rest.as_str()), ("read_csv", vec!["/Users/x/Downloads/CustomerTest.csv".to_string()], false, ""));
    assert_eq!(&sql[c[0].start..c[0].end], "read_csv('/Users/x/Downloads/CustomerTest.csv')");

    // Escaped quotes and named arguments (`=` and `:=`).
    let c = reader_calls("SELECT * FROM read_csv('/d/it''s.csv', header = true, delim := ';', columns = {'a': 'INT'})").unwrap();
    assert_eq!(c[0].paths, ["/d/it's.csv"]);
    assert_eq!(c[0].rest, ", header = true, delim := ';', columns = {'a': 'INT'}");

    // Comments and strings don't count; a list with a glob; a comment inside the call is left out.
    let sql = "-- read_csv('/a.csv')\nSELECT 'read_csv(''/b.csv'')' /* read_parquet('/c') */ FROM read_parquet(['/x/1.parquet', 'exports/*.parquet'] /* ) */, hive_partitioning = true) p";
    let c = reader_calls(sql).unwrap();
    assert_eq!(c.len(), 1);
    assert!(c[0].list);
    assert_eq!(c[0].paths, ["/x/1.parquet", "exports/*.parquet"]);
    assert_eq!(c[0].rest, ", hive_partitioning = true");
    assert!(sql[c[0].end..].starts_with(" p"));

    // Any case, every reader, several calls in order.
    let c = reader_calls("SELECT * FROM READ_CSV_AUTO('a.csv'), read_ndjson('b.ndjson'), parquet_scan('c.parquet'), read_json_auto('d.json'), read_xlsx('e.xlsx')").unwrap();
    assert_eq!(c.iter().map(|c| c.function.as_str()).collect::<Vec<_>>(), ["read_csv_auto", "read_ndjson", "parquet_scan", "read_json_auto", "read_xlsx"]);

    // Not a call, or qualified: not a reader call.
    assert!(reader_calls("SELECT read_csv FROM t; SELECT x.read_csv('a') FROM t; SELECT * FROM read_text('a')").unwrap().is_empty());
}

#[test]
fn reader_calls_refuse_paths_that_are_not_written_out() {
    for sql in [
        "SELECT * FROM read_csv(t.path)",
        "SELECT * FROM read_csv('a' || '.csv')",
        "SELECT * FROM read_csv(E'a.csv')",
        "SELECT * FROM read_csv($$a.csv$$)",
        "SELECT * FROM read_csv([])",
        "SELECT * FROM read_csv(['a.csv', t.b])",
        "SELECT * FROM read_csv(getvariable('p'))",
    ] {
        let e = reader_calls(sql).unwrap_err();
        assert!(e.contains("comillas simples"), "{sql}: {e}");
    }
    // Nothing past the call can hide in it.
    for sql in ["SELECT * FROM read_csv('a.csv'; COPY (SELECT 1) TO 'a.csv')", "SELECT * FROM read_csv('a.csv', x = (1); COPY (SELECT 1) TO 'a.csv' (USE_TMP_FILE false); SELECT (1))"] {
        assert!(reader_calls(sql).is_err(), "{sql}");
    }
    assert!(reader_calls("SELECT * FROM read_csv('a.csv', header = (true").unwrap_err().contains("falta cerrar"));
    assert!(reader_calls("SELECT * FROM read_csv('a.csv', delim = E'\\t')").unwrap_err().contains("comillas simples"));
}

// -- statements -------------------------------------------------------------------------------

#[test]
fn statements_that_write_attach_or_configure_are_refused() {
    for (sql, what) in [
        ("COPY (SELECT 1) TO '/tmp/x.csv'", "COPY"),
        ("SELECT 1; copy t TO 'x.parquet'", "COPY"),
        ("EXPLAIN ANALYZE COPY (SELECT 1) TO 'x.csv'", "COPY"),
        ("EXPLAIN (FORMAT json) COPY (SELECT 1) TO 'x.csv'", "COPY"),
        ("EXPORT DATABASE '/tmp/x'", "EXPORT"),
        ("IMPORT DATABASE '/tmp/x'", "IMPORT"),
        ("ATTACH 'x.duckdb' AS x", "ATTACH"),
        ("DETACH x", "DETACH"),
        ("INSTALL httpfs", "INSTALL"),
        ("FORCE INSTALL httpfs", "FORCE"),
        ("LOAD httpfs", "LOAD"),
        ("UPDATE EXTENSIONS", "UPDATE EXTENSIONS"),
        ("CREATE SECRET s (TYPE s3)", "CREATE SECRET"),
        ("CREATE OR REPLACE PERSISTENT SECRET s (TYPE s3)", "CREATE SECRET"),
        ("DROP SECRET s", "DROP SECRET"),
        ("SET enable_external_access = true", "SET"),
        ("SET GLOBAL threads = 1", "SET"),
        ("RESET lock_configuration", "RESET"),
        ("PRAGMA enable_profiling", "PRAGMA"),
        ("/* hi */ checkpoint", "CHECKPOINT"),
    ] {
        let e = refused(sql).unwrap_or_else(|| panic!("{sql} not refused"));
        assert!(e.starts_with(&format!("«{what}»")), "{sql}: {e}");
    }
    for ok in [
        "SELECT * FROM read_csv('a.csv')",
        "CREATE TABLE t AS SELECT 1 AS x; UPDATE t SET x = 2",
        "SET VARIABLE p = 1",
        "RESET VARIABLE p",
        "EXPLAIN SELECT 1",
        "SELECT 'COPY x TO y' AS s -- ATTACH",
        "WITH copy AS (SELECT 1) SELECT * FROM copy",
        "SELECT * FROM pragma_table_info('t')",
        "CREATE TABLE secret (x INT)",
    ] {
        assert_eq!(refused(ok), None, "{ok}");
    }
}

// -- resolution -------------------------------------------------------------------------------

#[test]
fn paths_resolve_home_relative_globs_and_symlinks() {
    let dir = temp("resolve");
    let home = dir.join("home");
    let nb = dir.join("proyecto");
    write(&home.join("Downloads/h.csv"), "a\n1\n");
    write(&nb.join("a.csv"), "a\n1\n");
    write(&nb.join("exports/ventas_1.csv"), "a\n1\n");
    write(&nb.join("exports/ventas_2.csv"), "a\n2\n");
    write(&nb.join("exports/otro.csv"), "a\n3\n");
    write(&nb.join("exports/2025/ventas_9.csv"), "a\n9\n");
    std::fs::create_dir_all(nb.join("sub")).unwrap();
    let r = |raw: &str, base: Option<&Path>| resolve(raw, base, Some(&home));
    let base = Some(nb.as_path());

    assert_eq!(r("~/Downloads/h.csv", None).unwrap(), [home.join("Downloads/h.csv")]);
    assert_eq!(r("a.csv", base).unwrap(), [nb.join("a.csv")]);
    assert_eq!(r("./sub/../a.csv", base).unwrap(), [nb.join("a.csv")]);
    assert_eq!(r(&nb.join("a.csv").to_string_lossy(), None).unwrap(), [nb.join("a.csv")]);
    // Globs: sorted, only what matches.
    assert_eq!(r("exports/ventas_*.csv", base).unwrap(), [nb.join("exports/ventas_1.csv"), nb.join("exports/ventas_2.csv")]);
    assert_eq!(r("exports/ventas_[2-9].csv", base).unwrap(), [nb.join("exports/ventas_2.csv")]);
    assert_eq!(r("exports/ventas_?.csv", base).unwrap().len(), 2);
    assert_eq!(r("exports/**/ventas_*.csv", base).unwrap(), [nb.join("exports/2025/ventas_9.csv"), nb.join("exports/ventas_1.csv"), nb.join("exports/ventas_2.csv")]);
    assert!(r("exports/nada_*.csv", base).unwrap_err().starts_with("ningún archivo coincide con «"));

    // A symlink is the file it points to.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(nb.join("a.csv"), dir.join("enlace.csv")).unwrap();
        assert_eq!(r(&dir.join("enlace.csv").to_string_lossy(), None).unwrap(), [nb.join("a.csv")]);
    }

    // Clear errors naming the path.
    let missing = r("falta.csv", base).unwrap_err();
    assert_eq!(missing, format!("no existe el archivo «{}»", nb.join("falta.csv").display()));
    let rel = r("a.csv", None).unwrap_err();
    assert!(rel.starts_with("«a.csv» es una ruta relativa: guardá el notebook"), "{rel}");
    assert!(r("sub", base).unwrap_err().contains("es una carpeta"));
    for url in ["https://example.com/x.csv", "http://h/x.csv", "s3://bucket/x.parquet", "gs://b/x.csv", "azure://c/x.csv", "abfss://c@a/x", "hf://datasets/x"] {
        let e = r(url, base).unwrap_err();
        assert!(e.contains("no es un archivo local"), "{url}: {e}");
    }
    assert!(r("  ", base).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plan_rebuilds_the_call_with_resolved_paths() {
    let dir = temp("plan");
    write(&dir.join("it's.csv"), "a\n1\n");
    write(&dir.join("x/1.csv"), "a\n1\n");
    write(&dir.join("x/2.csv"), "a\n2\n");
    let sql = "SELECT * FROM read_csv('it''s.csv', header = true) t JOIN read_csv(['x/*.csv', 'x/1.csv']) u ON true";
    let p = plan(sql, Some(&dir), None, 100).unwrap();
    let q = |p: &Path| lit(&p.to_string_lossy());
    assert_eq!(p[0].text, format!("read_csv({}, header = true)", q(&dir.join("it's.csv"))));
    // A glob or a list: a list of the files, each once.
    assert_eq!(p[1].text, format!("read_csv([{}, {}])", q(&dir.join("x/1.csv")), q(&dir.join("x/2.csv"))));
    assert_eq!(p[1].display, "x/*.csv, x/1.csv");
    assert_eq!(p[0].display, "it's.csv");
    assert_eq!(p[1].files.iter().map(|f| f.display.as_str()).collect::<Vec<_>>(), ["x/1.csv", "x/2.csv"]);
    assert!(p[0].files[0].size > 0 && p[0].files[0].mtime_ms > 0);
    let out = rewrite(sql, &p);
    assert_eq!(out, format!("SELECT * FROM \"{}\" t JOIN \"{}\" u ON true", p[0].table, p[1].table));
    assert!(p[0].table.starts_with("file:") && p[0].table != p[1].table);
    // The key follows the file: another size, another key; the table stays.
    let before = p[0].key.clone();
    write(&dir.join("it's.csv"), "a\n1\n2\n");
    let again = plan(sql, Some(&dir), None, 100).unwrap();
    assert_ne!(again[0].key, before);
    assert_eq!(again[0].table, p[0].table);
    assert_ne!(plan(sql, Some(&dir), None, 200).unwrap()[1].key, p[1].key);
    // A workbook is one file.
    write(&dir.join("x/a.xlsx"), "x");
    write(&dir.join("x/b.xlsx"), "x");
    assert!(plan("SELECT * FROM read_xlsx('x/*.xlsx')", Some(&dir), None, 100).unwrap_err().contains("un solo archivo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_detected_file_is_allowed_for_now() {
    let f = DataFile { path: "/a.csv".into(), display: "a.csv".into(), size: 1, mtime_ms: 1 };
    assert_eq!(allowed_files(std::slice::from_ref(&f)), vec![f]);
}

// -- DuckDB -----------------------------------------------------------------------------------

async fn rows_of(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.into_iter().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default()
}

async fn fails(s: &mut dyn Session, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 10, &mut out).await {
        Err(e) => e.to_string(),
        Ok(()) => out.error.unwrap_or_else(|| panic!("{sql} ran")),
    }
}

/// A DuckDB session that can write files (the test's setup, not a cell).
async fn writer() -> Box<dyn Session> {
    let mut cfg = ConnectionConfig { driver: "duckdb".into(), host: ":memory:".into(), ..Default::default() };
    cfg.options.insert("isolated".into(), "true".into());
    dbine_drivers::open_session(&cfg, None).await.expect("DuckDB")
}

fn cell(name: &str, sql: &str) -> LocalSource {
    LocalSource { name: name.into(), local: false, sql: sql.into(), connection_id: Some("conn".into()), database: None, run: None, rows: None, files_key: String::new() }
}

fn clientes() -> LocalSource {
    let mut c = cell("c2", "SELECT CustomerID, nombre FROM clientes");
    c.run = Some(1);
    c.rows = Some(LiveRows {
        columns: vec![ResultColumn { name: "CustomerID".into(), type_name: "int".into() }, ResultColumn { name: "nombre".into(), type_name: "nvarchar".into() }],
        rows: vec![vec![json!(1), json!("Ana")], vec![json!(2), json!("Luis")], vec![json!(3), json!("Eva")]],
    });
    c
}

fn never(_: usize) -> std::future::Ready<CommandResult<Fetched>> {
    std::future::ready(Err(CommandError::Internal("no".into())))
}

#[tokio::test]
async fn notebook_files_csv_parquet_and_ndjson_join_a_cell_table() {
    let dir = temp("join");
    let nb = dir.join("notebooks");
    write(&nb.join("CustomerTest.csv"), "CustomerID,ciudad\n1,Rosario\n2,Córdoba\n4,Salta\n");
    write(&nb.join("eventos.ndjson"), "{\"CustomerID\": 1, \"tag\": \"web\"}\n{\"CustomerID\": 2, \"tag\": \"app\"}\n");
    let parquet = dir.join("datos/ventas.parquet");
    std::fs::create_dir_all(parquet.parent().unwrap()).unwrap();
    {
        let mut w = writer().await;
        rows_of(w.as_mut(), &format!("COPY (SELECT * FROM (VALUES (1, 12.50::DECIMAL(10,2)), (2, 7.25::DECIMAL(10,2))) v(CustomerID, total)) TO {} (FORMAT parquet)", lit(&parquet.to_string_lossy()))).await;
    }
    let mut s = open_sandboxed().await.expect("DuckDB");
    let sources = vec![clientes()];
    let sql = "SELECT c.nombre, t.ciudad, p.total, j.tag \
               FROM c2 c JOIN read_csv('CustomerTest.csv') t ON t.CustomerID = c.CustomerID \
               JOIN read_parquet('../datos/ventas.parquet') p ON p.CustomerID = c.CustomerID \
               JOIN read_ndjson('eventos.ndjson') j ON j.CustomerID = c.CustomerID ORDER BY 1";
    let mut tables = HashMap::new();
    let mut out = QueryOutcome::default();
    let (run, loaded) = prepare(s.as_mut(), &mut tables, &HashSet::new(), sql, &sources, 1_000, Some(&nb), &mut out, None, never).await.unwrap();
    assert!(!run.contains("read_"), "{run}");
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!("Ana"), json!("Rosario"), json!("12.50"), json!("web")], vec![json!("Luis"), json!("Córdoba"), json!("7.25"), json!("app")]]);
    let summary: Vec<(&str, u64, &str, bool)> = loaded.iter().map(|l| (l.name.as_str(), l.rows, l.origin, l.cached)).collect();
    let pq = parquet.to_string_lossy().to_string();
    assert_eq!(summary, vec![("c2", 3, "run", false), ("CustomerTest.csv", 3, "file", false), (pq.as_str(), 2, "file", false), ("eventos.ndjson", 2, "file", false)]);
    assert!(loaded[1].files[0].size > 0 && loaded[1].files[0].mtime_ms > 0);
    // The notebook's session was granted nothing: it still can't read the file itself.
    assert_eq!(rows_of(s.as_mut(), "SELECT current_setting('allowed_paths')::VARCHAR").await, vec![vec![json!("[]")]]);
    let csv = nb.join("CustomerTest.csv");
    for direct in [format!("SELECT * FROM read_text({})", lit(&csv.to_string_lossy())), format!("SELECT * FROM {}", lit(&csv.to_string_lossy()))] {
        assert!(fails(s.as_mut(), &direct).await.contains("disabled"), "{direct}");
    }

    // Again: the files are already loaded.
    let present = existing_tables(s.as_mut()).await.unwrap();
    let (_, loaded) = prepare(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, Some(&nb), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert!(loaded.iter().all(|l| l.cached), "{loaded:?}");
    // The CSV changed: only it is read again.
    write(&csv, "CustomerID,ciudad\n1,Mendoza\n2,Córdoba\n");
    let (run, loaded) = prepare(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, Some(&nb), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert_eq!(loaded.iter().map(|l| l.cached).collect::<Vec<_>>(), [true, false, true, true]);
    assert_eq!(rows_of(s.as_mut(), &run).await[0][1], json!("Mendoza"));

    // A results cell that reads a file is a table too; its table follows the file.
    let mut c3 = cell("c3", "SELECT count(*) AS n FROM read_csv('CustomerTest.csv')");
    c3.local = true;
    let sources = vec![clientes(), c3];
    let (run, _) = prepare(s.as_mut(), &mut tables, &present, "SELECT n FROM c3", &sources, 1_000, Some(&nb), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!(2)]]);
    write(&csv, "CustomerID,ciudad\n1,Mendoza\n2,Córdoba\n3,Jujuy\n");
    let present = existing_tables(s.as_mut()).await.unwrap();
    let (run, loaded) = prepare(s.as_mut(), &mut tables, &present, "SELECT n FROM c3", &sources, 1_000, Some(&nb), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert!(loaded.iter().all(|l| !l.cached), "{loaded:?}");
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!(3)]]);

    // An unsaved notebook can't read relative paths.
    let e = prepare(s.as_mut(), &mut tables, &present, sql, &sources, 1_000, None, &mut QueryOutcome::default(), None, never).await.unwrap_err();
    assert!(e.to_string().contains("«CustomerTest.csv» es una ruta relativa"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_files_globs_and_lists_and_the_row_cap() {
    let dir = temp("glob");
    for i in 1..=3 {
        write(&dir.join(format!("exports/ventas_{i}.csv")), &format!("n\n{i}\n{}\n", i * 10));
    }
    write(&dir.join("exports/otro.csv"), "n\n999\n");
    let mut s = open_sandboxed().await.expect("DuckDB");
    let mut out = QueryOutcome::default();
    let sql = "SELECT (SELECT sum(n) FROM read_csv('exports/ventas_*.csv')) AS g, (SELECT sum(n) FROM read_csv(['exports/ventas_1.csv', 'exports/otro.csv'])) AS l";
    let (run, loaded) = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), sql, &[], 1_000, Some(&dir), &mut out, None, never).await.unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!(66), json!(1010)]]);
    assert_eq!(loaded.iter().map(|l| (l.name.as_str(), l.files.len(), l.rows)).collect::<Vec<_>>(), [("exports/ventas_*.csv", 3, 6), ("exports/ventas_1.csv, exports/otro.csv", 2, 3)]);
    // Cut at the cap, and said.
    let mut out = QueryOutcome::default();
    let (_, loaded) = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), "SELECT * FROM read_csv('exports/*.csv')", &[], 4, Some(&dir), &mut out, None, never).await.unwrap();
    assert!(loaded[0].capped && loaded[0].rows == 4, "{loaded:?}");
    assert!(out.log.iter().any(|m| m.text.contains("se cargó con las primeras 4 filas")), "{:?}", out.log);
    // A URL never reaches DuckDB.
    let e = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), "SELECT * FROM read_parquet('https://example.com/x.parquet')", &[], 4, Some(&dir), &mut out, None, never).await.unwrap_err();
    assert!(e.to_string().contains("no leen de la red"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_files_an_allowed_file_is_never_written() {
    let dir = temp("copy");
    let csv = dir.join("clientes.csv");
    let original = "CustomerID,ciudad\n1,Rosario\n";
    write(&csv, original);
    let p = lit(&csv.to_string_lossy());
    let mut s = open_sandboxed().await.expect("DuckDB");
    // In the cell: refused before anything runs, even next to a read of that very file.
    for sql in [
        format!("COPY (SELECT 1) TO {p}"),
        format!("SELECT * FROM read_csv({p}); COPY (SELECT 1 AS x) TO {p} (USE_TMP_FILE false)"),
        format!("SELECT * FROM read_csv({p}); EXPORT DATABASE {}", lit(&dir.to_string_lossy())),
        format!("ATTACH {p} AS x"),
    ] {
        let e = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), &sql, &[], 100, Some(&dir), &mut QueryOutcome::default(), None, never).await.unwrap_err();
        assert!(e.to_string().contains("no se puede usar en una celda"), "{sql}: {e}");
    }
    // After the file was read into a table, the notebook's session still has no access to it:
    // DuckDB refuses the write too.
    let (run, _) = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), &format!("SELECT * FROM read_csv({p})"), &[], 100, Some(&dir), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await.len(), 1);
    for sql in [format!("COPY (SELECT 1 AS x) TO {p} (USE_TMP_FILE false)"), format!("COPY (SELECT 1 AS x) TO {p}"), format!("ATTACH {p} AS x")] {
        let e = fails(s.as_mut(), &sql).await;
        assert!(e.contains("disabled") || e.contains("Permission"), "{sql}: {e}");
    }
    assert_eq!(std::fs::read_to_string(&csv).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_files_reader_session_gets_exactly_its_files() {
    let dir = temp("reader");
    write(&dir.join("a.csv"), "x\n1\n");
    write(&dir.join("b.csv"), "x\n2\n");
    let a = plan("SELECT * FROM read_csv('a.csv')", Some(&dir), None, 10).unwrap();
    let mut r = open_reader(&a[0].files, false).await.unwrap();
    assert_eq!(rows_of(r.as_mut(), &format!("SELECT * FROM {}", a[0].text)).await, vec![vec![json!(1)]]);
    let b = lit(&dir.join("b.csv").to_string_lossy());
    assert!(fails(r.as_mut(), &format!("SELECT * FROM read_csv({b})")).await.contains("disabled"));
    assert!(!fails(r.as_mut(), &format!("SET allowed_paths = [{b}]")).await.is_empty());
    assert!(fails(r.as_mut(), "SET enable_external_access = true").await.contains("locked"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_files_workbooks_with_the_excel_extension() {
    let dir = temp("xlsx");
    let book = dir.join("libro.xlsx");
    {
        let mut w = writer().await;
        let mut out = QueryOutcome::default();
        let made = w.execute(&format!("INSTALL excel; LOAD excel; COPY (SELECT 1 AS id, 'Ana' AS nombre) TO {} (FORMAT xlsx, HEADER true)", lit(&book.to_string_lossy())), 1, &mut out).await;
        if made.is_err() || out.error.is_some() || !book.exists() {
            eprintln!("skipped: DuckDB's excel extension isn't available here ({made:?} {:?})", out.error);
            return;
        }
    }
    let mut s = open_sandboxed().await.expect("DuckDB");
    let (run, loaded) = prepare(s.as_mut(), &mut HashMap::new(), &HashSet::new(), "SELECT nombre FROM read_xlsx('libro.xlsx')", &[], 100, Some(&dir), &mut QueryOutcome::default(), None, never).await.unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!("Ana")]]);
    assert_eq!(loaded[0].name, "libro.xlsx");
    let _ = std::fs::remove_dir_all(&dir);
}

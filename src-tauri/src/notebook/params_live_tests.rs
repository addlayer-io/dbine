//! Notebook parameters against real servers (the `dbine-test-*`
//! containers): a parameterized cell runs on each engine, bound where the
//! driver binds and as literals where it doesn't, and a value made to break
//! out of its literal finds its row and drops nothing. Ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored notebook_params_live --test-threads=1 --nocapture
//! ```
//!
//! URLs (defaults in brackets): `DBINE_TEST_POSTGRES_URL`
//! [postgres://postgres:pw@localhost:25010], `DBINE_TEST_SQLSERVER_URL`
//! [mssql://sa:Pw_12345!@localhost:25013], `DBINE_TEST_MYSQL_URL`
//! [mysql://root:pw@localhost:25011], `DBINE_TEST_ORACLE_ADMIN_URL`
//! [oracle://system:Secret123@localhost:25601/FREEPDB1],
//! `DBINE_TEST_MONGODB_URL` [mongodb://root:secret@localhost:25201/?authSource=admin],
//! `DBINE_TEST_REDIS_URL` [redis://localhost:25400].

use super::*;
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::json;

const EVIL: &str = "'; DROP TABLE dbine_nbp; --";

fn url(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

fn server_cfg(driver: &str, u: &str) -> ConnectionConfig {
    let rest = u.split_once("://").map_or(u, |(_, r)| r);
    let (rest, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    let mut cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap_or(0),
        username: (!user.is_empty()).then(|| user.to_string()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    };
    if driver == "oracle" {
        cfg.options.insert("service".into(), path.split('?').next().unwrap_or("").into());
    }
    cfg
}

async fn run(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none() && out.errors.is_empty(), "{sql}: {:?} {:?}", out.error, out.errors);
    out.results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default()
}

fn defs() -> Vec<NotebookParameter> {
    let p = |name: &str, kind| NotebookParameter { name: name.into(), kind, default: None, description: None, required: true, options: vec![] };
    vec![p("nombre", ParamType::Text), p("desde", ParamType::Date), p("minimo", ParamType::Number)]
}

/// The cell `text` with `given`, run as a run without the UI runs it:
/// the rows of its last result and how many units went bound.
async fn cell(s: &mut dyn Session, driver_id: &str, text: &str, given: serde_json::Value) -> (Vec<Vec<Value>>, usize) {
    let d = dbine_drivers::find(driver_id).unwrap();
    let given: Values = serde_json::from_value(given).unwrap();
    let values = resolve(&defs(), &given).unwrap();
    let p = prepare_text(text, &Engine::Driver(d.as_ref()), &defs(), &values, None).unwrap_or_else(|e| panic!("{driver_id}: {e}"));
    let mut out = QueryOutcome::default();
    let bound = run_prepared(s, d.as_ref(), &p, 1000, &mut out).await.unwrap_or_else(|e| panic!("{driver_id}: {}: {e}", p.text));
    assert!(out.error.is_none() && out.errors.is_empty(), "{driver_id}: {:?} {:?}", out.error, out.errors);
    (out.results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default(), bound)
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter().map(|r| r[0].as_i64().or_else(|| r[0].as_str().and_then(|s| s.parse().ok())).or_else(|| r[0].as_f64().map(|f| f as i64)).unwrap_or(-1)).collect()
}

/// One SQL engine: the table, a lookup by each parameter type, the evil
/// value, and the table still there.
async fn sql_engine(driver_id: &str, cfg: ConnectionConfig, database: Option<&str>, setup: &[&str], query: &str, count: &str, cleanup: &str, expect_bound: bool) {
    let mut s = dbine_drivers::open_session(&cfg, database).await.unwrap_or_else(|e| panic!("{driver_id}: {e}"));
    for q in setup {
        run(s.as_mut(), q).await;
    }
    let (rows, bound) = cell(s.as_mut(), driver_id, query, json!({ "nombre": "Ana", "desde": "2026-01-01", "minimo": 1 })).await;
    assert_eq!(ids(&rows), vec![1], "{driver_id}");
    assert_eq!(bound > 0, expect_bound, "{driver_id}: bound units {bound}");
    let (rows, _) = cell(s.as_mut(), driver_id, query, json!({ "nombre": EVIL, "desde": "2026-01-01", "minimo": 1.5 })).await;
    assert_eq!(ids(&rows), vec![3], "{driver_id}: the evil value is data");
    let (rows, _) = cell(s.as_mut(), driver_id, query, json!({ "nombre": "O'Brien \\ x", "desde": "2020-01-01", "minimo": -7 })).await;
    assert_eq!(ids(&rows), vec![2], "{driver_id}: quotes and backslashes");
    assert_eq!(ids(&run(s.as_mut(), count).await), vec![3], "{driver_id}: the table is still there");
    run(s.as_mut(), cleanup).await;
    eprintln!("{driver_id}: ok ({})", if expect_bound { "bound" } else { "literals" });
}

#[tokio::test]
#[ignore]
async fn notebook_params_live_sql_engines() {
    let rows = "(1, 'Ana', DATE '2026-05-01', 10), (2, 'O''Brien \\ x', DATE '2021-05-01', 0), (3, '''; DROP TABLE dbine_nbp; --', DATE '2026-02-01', 2)";
    let query = "SELECT id FROM dbine_nbp WHERE nombre = {{nombre}} AND alta >= {{desde}} AND n > {{minimo}} ORDER BY id";

    let mut pg = server_cfg("postgres", &url("DBINE_TEST_POSTGRES_URL", "postgres://postgres:pw@localhost:25010"));
    pg.database = "postgres".into();
    let pg_rows = rows.replace("'O''Brien \\ x'", "E'O''Brien \\\\ x'");
    sql_engine(
        "postgres",
        pg,
        Some("postgres"),
        &["DROP TABLE IF EXISTS dbine_nbp", "CREATE TABLE dbine_nbp (id int, nombre text, alta date, n numeric(10,2))", &format!("INSERT INTO dbine_nbp VALUES {pg_rows}")],
        query,
        "SELECT count(*) FROM dbine_nbp",
        "DROP TABLE dbine_nbp",
        false,
    )
    .await;

    let ms = server_cfg("sqlserver", &url("DBINE_TEST_SQLSERVER_URL", "mssql://sa:Pw_12345!@localhost:25013"));
    let ms_rows = rows.replace("DATE '", "'");
    sql_engine(
        "sqlserver",
        ms,
        None,
        &[
            "IF OBJECT_ID('dbo.dbine_nbp') IS NOT NULL DROP TABLE dbo.dbine_nbp",
            "CREATE TABLE dbo.dbine_nbp (id int, nombre nvarchar(50), alta date, n decimal(10,2))",
            &format!("INSERT INTO dbo.dbine_nbp VALUES {ms_rows}"),
        ],
        query,
        "SELECT count(*) FROM dbo.dbine_nbp",
        "DROP TABLE dbo.dbine_nbp",
        true,
    )
    .await;

    let my = server_cfg("mysql", &url("DBINE_TEST_MYSQL_URL", "mysql://root:pw@localhost:25011"));
    {
        let mut s = dbine_drivers::open_session(&my, None).await.unwrap();
        run(s.as_mut(), "CREATE DATABASE IF NOT EXISTS dbine_nbp_db").await;
    }
    let my_rows = rows.replace("'O''Brien \\ x'", "'O''Brien \\\\ x'");
    sql_engine(
        "mysql",
        my.clone(),
        Some("dbine_nbp_db"),
        &["DROP TABLE IF EXISTS dbine_nbp", "CREATE TABLE dbine_nbp (id int, nombre varchar(50), alta date, n decimal(10,2))", &format!("INSERT INTO dbine_nbp VALUES {my_rows}")],
        query,
        "SELECT count(*) FROM dbine_nbp",
        "DROP TABLE dbine_nbp",
        true,
    )
    .await;

    let ora = server_cfg("oracle", &url("DBINE_TEST_ORACLE_ADMIN_URL", "oracle://system:Secret123@localhost:25601/FREEPDB1"));
    let ora_rows: Vec<String> = rows
        .trim_matches(['(', ')'])
        .split("), (")
        .map(|r| format!("INSERT INTO dbine_nbp VALUES ({r})"))
        .collect();
    let mut setup = vec![
        "BEGIN EXECUTE IMMEDIATE 'DROP TABLE dbine_nbp'; EXCEPTION WHEN OTHERS THEN NULL; END;".to_string(),
        "CREATE TABLE dbine_nbp (id NUMBER, nombre VARCHAR2(50), alta DATE, n NUMBER(10,2))".to_string(),
    ];
    setup.extend(ora_rows);
    setup.push("COMMIT".into());
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    sql_engine("oracle", ora, None, &setup, query, "SELECT count(*) FROM dbine_nbp", "DROP TABLE dbine_nbp", true).await;

    // SQLite: a file of its own.
    let dir = std::env::temp_dir().join(format!("dbine-nbp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lite = ConnectionConfig { driver: "sqlite".into(), host: dir.join("t.db").to_string_lossy().into_owned(), ..Default::default() };
    let lite_rows = rows.replace("DATE '", "'");
    sql_engine(
        "sqlite",
        lite,
        None,
        &["DROP TABLE IF EXISTS dbine_nbp", "CREATE TABLE dbine_nbp (id int, nombre text, alta text, n real)", &format!("INSERT INTO dbine_nbp VALUES {lite_rows}")],
        query,
        "SELECT count(*) FROM dbine_nbp",
        "DROP TABLE dbine_nbp",
        true,
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore]
async fn notebook_params_live_mongodb_redis() {
    let mut mg = ConnectionConfig { driver: "mongodb".into(), database: "dbine_nbp_db".into(), ..Default::default() };
    mg.options.insert("connection_string".into(), url("DBINE_TEST_MONGODB_URL", "mongodb://root:secret@localhost:25201/?authSource=admin"));
    let mut s = dbine_drivers::open_session(&mg, Some("dbine_nbp_db")).await.unwrap();
    run(s.as_mut(), "db.dbine_nbp.drop()").await;
    run(
        s.as_mut(),
        r#"db.dbine_nbp.insertMany([{_id: 1, nombre: "Ana", alta: ISODate("2026-05-01T00:00:00Z"), n: 10}, {_id: 2, nombre: "\"}); db.dbine_nbp.drop(); ({\"", alta: ISODate("2026-02-01T00:00:00Z"), n: 2}])"#,
    )
    .await;
    let q = "db.dbine_nbp.find({ nombre: {{nombre}}, alta: { $gte: {{desde}} }, n: { $gt: {{minimo}} } }, { _id: 1 })";
    let (rows, bound) = cell(s.as_mut(), "mongodb", q, json!({ "nombre": "Ana", "desde": "2026-01-01", "minimo": 1 })).await;
    assert_eq!(ids(&rows), vec![1]);
    assert_eq!(bound, 0, "mongodb: literals");
    let (rows, _) = cell(s.as_mut(), "mongodb", q, json!({ "nombre": "\"}); db.dbine_nbp.drop(); ({\"", "desde": "2026-01-01", "minimo": 1 })).await;
    assert_eq!(ids(&rows), vec![2], "the evil value is data");
    assert_eq!(run(s.as_mut(), "db.dbine_nbp.countDocuments({})").await.len(), 1, "the collection is still there");
    run(s.as_mut(), "db.dbine_nbp.drop()").await;
    eprintln!("mongodb: ok (literals)");

    let rd = server_cfg("redis", &url("DBINE_TEST_REDIS_URL", "redis://localhost:25400"));
    let mut s = dbine_drivers::open_session(&rd, None).await.unwrap();
    run(s.as_mut(), "SET \"dbine_nbp:a b\" uno").await;
    let (rows, bound) = cell(s.as_mut(), "redis", "GET {{nombre}}", json!({ "nombre": "dbine_nbp:a b", "desde": "2026-01-01", "minimo": 1 })).await;
    assert_eq!(rows, vec![vec![json!("uno")]]);
    assert_eq!(bound, 0);
    let (rows, _) = cell(s.as_mut(), "redis", "GET {{nombre}}", json!({ "nombre": "x\"\nFLUSHALL", "desde": "2026-01-01", "minimo": 1 })).await;
    assert!(rows.iter().all(|r| r[0].is_null()), "{rows:?}");
    assert_eq!(run(s.as_mut(), "GET \"dbine_nbp:a b\"").await, vec![vec![json!("uno")]], "nothing was flushed");
    run(s.as_mut(), "DEL \"dbine_nbp:a b\"").await;
    eprintln!("redis: ok (literals)");
}

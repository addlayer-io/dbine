//! `chart_aggregate` against real engines: the grouped statement DBine
//! builds runs on the server and gives the same numbers as a hand-written
//! `GROUP BY`, and the client path (the rows streamed through the
//! aggregator) gives the same numbers again.
//!
//! SQLite runs always (a file in the temp dir). The rest are ignored by
//! default; the servers are the `dbine-test-*` containers on their usual
//! ports (each URL can be replaced with `DBINE_TEST_<ENGINE>_URL`):
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored chart_live --test-threads=1 --nocapture
//! ```

use super::{aggregate_on, Stop};
use dbine_driver::aggregate::{label, num, AggregateMethod, Aggregator, ChartAggregate};
use dbine_driver::{AggFn, AggregateSpec, ChartKind, ConnectionConfig, Driver, Measure, QueryOutcome, RowSinkRef, Session};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const REGIONS: [&str; 4] = ["norte", "sur", "este", "oeste"];

/// Row `i` of the sample: (id, region or NULL, cat, v or NULL).
fn sample(i: u32) -> (u32, Option<&'static str>, &'static str, Option<u32>) {
    let region = (!i.is_multiple_of(7)).then_some(REGIONS[(i % 4) as usize]);
    let v = (!i.is_multiple_of(11)).then_some((i * 37) % 101);
    (i, region, if i.is_multiple_of(2) { "a" } else { "b" }, v)
}

fn sql_values(i: u32) -> String {
    let (id, region, cat, v) = sample(i);
    let region = region.map_or("NULL".to_string(), |r| format!("'{r}'"));
    let v = v.map_or("NULL".to_string(), |v| v.to_string());
    format!("({id}, {region}, '{cat}', {v})")
}

const ROWS: u32 = 60;

async fn exec(s: &mut dyn Session, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

fn rows(out: &QueryOutcome) -> Vec<Vec<Value>> {
    out.results.iter().find(|r| !r.columns.is_empty()).map(|r| r.rows.clone()).unwrap_or_default()
}

fn close(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0),
        _ => false,
    }
}

fn spec(x: &str, v: &str, split: Option<&str>) -> AggregateSpec {
    let m = |column: Option<&str>, func| Measure { column: column.map(Into::into), func };
    let measures = if split.is_some() {
        vec![m(None, AggFn::Count), m(Some(v), AggFn::Sum)]
    } else {
        vec![m(None, AggFn::Count), m(Some(v), AggFn::Count), m(Some(v), AggFn::Sum), m(Some(v), AggFn::Avg), m(Some(v), AggFn::Min), m(Some(v), AggFn::Max)]
    };
    AggregateSpec { x: x.into(), split: split.map(Into::into), measures, max_groups: 100 }
}

/// `chart` (no split) against the hand-written GROUP BY's rows: x, then one
/// column per measure in order.
fn assert_matches(engine: &str, chart: &ChartAggregate, expected: &[Vec<Value>]) {
    let mut want: Vec<(Option<String>, Vec<Option<f64>>)> = expected.iter().map(|r| (label(&r[0]), r[1..].iter().map(num).collect())).collect();
    let mut got: Vec<(Option<String>, Vec<Option<f64>>)> = chart
        .categories
        .iter()
        .enumerate()
        .map(|(i, c)| (c.clone(), chart.series.iter().map(|s| s.values[i]).collect()))
        .collect();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(got.len(), want.len(), "{engine}: categories {got:?} vs {want:?}");
    for ((gc, gv), (wc, wv)) in got.iter().zip(&want) {
        assert_eq!(gc, wc, "{engine}");
        assert!(gv.len() == wv.len() && gv.iter().zip(wv).all(|(a, b)| close(*a, *b)), "{engine}: {gc:?}: {gv:?} vs {wv:?}");
    }
}

/// The split chart against counts and sums per (x, split) of the rows.
fn assert_split(engine: &str, chart: &ChartAggregate, expected: &[Vec<Value>]) {
    for r in expected {
        let (x, k) = (label(&r[0]), label(&r[1]));
        let i = chart.categories.iter().position(|c| *c == x).unwrap_or_else(|| panic!("{engine}: no category {x:?}"));
        for (m, want) in [(0, num(&r[2])), (1, num(&r[3]))] {
            let s = chart.series.iter().find(|s| s.measure == m && s.split == k).unwrap_or_else(|| panic!("{engine}: no series {k:?}"));
            assert!(close(s.values[i], want), "{engine}: {x:?}/{k:?} m{m}: {:?} vs {want:?}", s.values[i]);
        }
    }
}

async fn run(d: &dyn Driver, s: &mut dyn Session, query: &str, chart: &ChartKind) -> ChartAggregate {
    let (cancel, cancelled) = (Notify::new(), AtomicBool::new(false));
    let stop = Stop { cancel: &cancel, cancelled: &cancelled, interrupter: None };
    aggregate_on(d, s, query, chart, 1_000_000, &stop).await.unwrap_or_else(|e| panic!("{query}: {e}"))
}

/// The client path alone: the query's rows through the aggregator.
async fn client(s: &mut dyn Session, query: &str, chart: &ChartKind) -> ChartAggregate {
    let agg = Arc::new(Mutex::new(Aggregator::new(chart.clone(), 1_000_000).unwrap()));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(agg.clone())), ..Default::default() };
    s.execute(query, 1_000_001, &mut out).await.unwrap_or_else(|e| panic!("{query}: {e}"));
    assert!(out.error.is_none(), "{query}: {:?}", out.error);
    let r = agg.lock().unwrap().finish().unwrap();
    r
}

struct Case<'a> {
    engine: &'a str,
    /// The chart's query and its column names as the engine reports them.
    query: &'a str,
    x: &'a str,
    k: &'a str,
    v: &'a str,
    /// Hand-written: x, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v).
    by_x: &'a str,
    /// Hand-written: x, k, COUNT(*), SUM(v).
    by_xk: &'a str,
    /// Whether the grouped chart runs on the server.
    server: bool,
}

async fn check(d: &dyn Driver, s: &mut dyn Session, setup: &mut dyn Session, c: Case<'_>) {
    let chart = ChartKind::Group(spec(c.x, c.v, None));
    let got = run(d, s, c.query, &chart).await;
    eprintln!("[{}] {:?}: {} categories, {:?} rows", c.engine, got.method, got.categories.len(), got.rows_scanned);
    let want = if c.server { AggregateMethod::Server } else { AggregateMethod::Client };
    assert_eq!(got.method, want, "{}", c.engine);
    assert_eq!(got.rows_scanned, Some(u64::from(ROWS)), "{}", c.engine);
    assert!(!got.groups_truncated && !got.rows_capped, "{}", c.engine);
    let expected = if c.by_x.is_empty() { None } else { Some(rows(&exec(setup, c.by_x).await)) };
    if let Some(e) = &expected {
        assert_matches(c.engine, &got, e);
    }
    // The client path gives the same numbers.
    let by_client = client(s, c.query, &chart).await;
    assert_eq!(by_client.method, AggregateMethod::Client);
    let as_rows: Vec<Vec<Value>> = got
        .categories
        .iter()
        .enumerate()
        .map(|(i, cat)| {
            let mut r = vec![cat.clone().map_or(Value::Null, Value::String)];
            r.extend(got.series.iter().map(|s| s.values[i].map_or(Value::Null, |v| serde_json::json!(v))));
            r
        })
        .collect();
    assert_matches(&format!("{} (client)", c.engine), &by_client, &as_rows);
    // A split.
    let split = ChartKind::Group(spec(c.x, c.v, Some(c.k)));
    let got = run(d, s, c.query, &split).await;
    assert_eq!(got.method, want, "{} split", c.engine);
    if !c.by_xk.is_empty() {
        assert_split(c.engine, &got, &rows(&exec(setup, c.by_xk).await));
    }
    // Groups cut: max_groups 2 of 5 categories.
    let mut few = spec(c.x, c.v, None);
    few.max_groups = 2;
    let got = run(d, s, c.query, &ChartKind::Group(few)).await;
    assert!(got.groups_truncated && got.categories.len() == 2, "{}: {got:?}", c.engine);
    // Histogram and box plot (always in DBine) over every row.
    let h = run(d, s, c.query, &ChartKind::Histogram { column: c.v.into(), bins: 5 }).await;
    let hist = h.histogram.unwrap();
    let nulls = (1..=ROWS).filter(|i| sample(*i).3.is_none()).count() as u64;
    assert_eq!((hist.counts.iter().sum::<u64>() + hist.nulls, hist.nulls), (u64::from(ROWS), nulls), "{}", c.engine);
    let b = run(d, s, c.query, &ChartKind::Box { column: c.v.into(), by: Some(c.x.into()), max_groups: 50 }).await;
    let boxes = b.boxes.unwrap();
    assert_eq!(boxes.len(), 5, "{}", c.engine);
    let mut all: Vec<f64> = (1..=ROWS).filter_map(|i| (sample(i).1 == Some("norte")).then(|| sample(i).3).flatten().map(f64::from)).collect();
    all.sort_by(f64::total_cmp);
    let norte = boxes.iter().find(|b| b.category.as_deref() == Some("norte")).unwrap();
    assert_eq!((norte.count as usize, norte.min, norte.max), (all.len(), all[0], all[all.len() - 1]), "{}", c.engine);
}

fn cfg(engine: &str, env: &str, default_url: &str) -> ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default_url.to_string());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    ConnectionConfig {
        driver: engine.into(),
        host: host.into(),
        port: port.parse().unwrap_or(0),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

/// A writable session for the setup and a read-only one (as the command
/// opens) for the chart.
async fn sessions(cfg: &ConnectionConfig, db: Option<&str>) -> (&'static dyn Driver, Box<dyn Session>, Box<dyn Session>) {
    let d = dbine_drivers::find(&cfg.driver).unwrap_or_else(|| panic!("driver {} not built in", cfg.driver)).as_ref();
    let setup = d.connect(cfg, db).await.unwrap_or_else(|e| panic!("{}: {e}", cfg.driver));
    let ro = ConnectionConfig { read_only: true, ..cfg.clone() };
    let s = dbine_drivers::open_session(&ro, db).await.unwrap_or_else(|e| panic!("{}: {e}", cfg.driver));
    (d, setup, s)
}

async fn fill_sql(s: &mut dyn Session, table: &str, one_by_one: bool) {
    if one_by_one {
        for i in 1..=ROWS {
            exec(s, &format!("INSERT INTO {table} (id, region, cat, v) VALUES {}", sql_values(i))).await;
        }
    } else {
        let values: Vec<String> = (1..=ROWS).map(sql_values).collect();
        exec(s, &format!("INSERT INTO {table} (id, region, cat, v) VALUES {}", values.join(", "))).await;
    }
}

fn standard_case<'a>(engine: &'a str, query: &'a str, by_x: &'a str, by_xk: &'a str) -> Case<'a> {
    Case { engine, query, x: "region", k: "cat", v: "v", by_x, by_xk, server: true }
}

#[tokio::test]
async fn chart_live_sqlite() {
    let path = std::env::temp_dir().join(format!("dbine_chart_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id INTEGER, region TEXT, cat TEXT, v INTEGER)").await;
    fill_sql(setup.as_mut(), "dbine_chart", false).await;
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id;";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("sqlite", q, by_x, by_xk)).await;
    // A CTE is a single read too.
    let cte = "WITH t AS (SELECT * FROM dbine_chart) SELECT region, cat, v FROM t";
    let got = run(d, s.as_mut(), cte, &ChartKind::Group(spec("region", "v", None))).await;
    assert_eq!(got.method, AggregateMethod::Server);
    // Several statements, and writes, are refused.
    let (cancel, cancelled) = (Notify::new(), AtomicBool::new(false));
    let stop = Stop { cancel: &cancel, cancelled: &cancelled, interrupter: None };
    let chart = ChartKind::Group(spec("region", "v", None));
    assert!(aggregate_on(d, s.as_mut(), "SELECT 1; SELECT 2", &chart, 10, &stop).await.is_err());
    let e = aggregate_on(d, s.as_mut(), "DELETE FROM dbine_chart RETURNING region, v", &chart, 10, &stop).await.unwrap_err();
    eprintln!("[sqlite] write refused: {e}");
    assert_eq!(rows(&exec(setup.as_mut(), "SELECT COUNT(*) FROM dbine_chart").await)[0][0], serde_json::json!(ROWS));
    // The row cap.
    let capped = aggregate_on(d, s.as_mut(), q, &ChartKind::Histogram { column: "v".into(), bins: 3 }, 10, &stop).await.unwrap();
    assert!(capped.rows_capped && capped.rows_scanned == Some(10), "{capped:?}");
    drop((s, setup));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
#[ignore]
async fn chart_live_duckdb() {
    let path = std::env::temp_dir().join(format!("dbine_chart_{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = ConnectionConfig { driver: "duckdb".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id INTEGER, region VARCHAR, cat VARCHAR, v INTEGER)").await;
    fill_sql(setup.as_mut(), "dbine_chart", false).await;
    // DuckDB holds the file: the read-only session is the same process' second connection.
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("duckdb", q, by_x, by_xk)).await;
    drop((s, setup));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
#[ignore]
async fn chart_live_postgres() {
    let mut cfg = cfg("postgres", "DBINE_TEST_POSTGRES_URL", "postgres://postgres:pw@localhost:25010");
    cfg.database = "postgres".into();
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    exec(setup.as_mut(), "DROP TABLE IF EXISTS dbine_chart").await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id int, region text, cat text, v numeric(10,2))").await;
    fill_sql(setup.as_mut(), "dbine_chart", false).await;
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("postgres", q, by_x, by_xk)).await;
    // A column type with no equality (json) can't be grouped: the client path.
    let j = "SELECT to_json(region) AS region, cat, v FROM dbine_chart";
    let got = run(d, s.as_mut(), j, &ChartKind::Group(spec("region", "v", None))).await;
    assert_eq!(got.method, AggregateMethod::Client);
    exec(setup.as_mut(), "DROP TABLE dbine_chart").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_mysql() {
    let cfg = cfg("mysql", "DBINE_TEST_MYSQL_URL", "mysql://root:pw@localhost:25011");
    {
        let d = dbine_drivers::find("mysql").unwrap();
        let mut s = d.connect(&cfg, None).await.unwrap();
        exec(s.as_mut(), "CREATE DATABASE IF NOT EXISTS dbine_chart_db").await;
    }
    let (d, mut setup, mut s) = sessions(&cfg, Some("dbine_chart_db")).await;
    exec(setup.as_mut(), "DROP TABLE IF EXISTS dbine_chart").await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id int, region varchar(10), cat varchar(10), v int)").await;
    fill_sql(setup.as_mut(), "dbine_chart", false).await;
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("mysql", q, by_x, by_xk)).await;
    exec(setup.as_mut(), "DROP DATABASE dbine_chart_db").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_sqlserver() {
    let mut cfg = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL", "mssql://sa:Pw_12345!@localhost:25013");
    cfg.trust_server_certificate = true;
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    exec(setup.as_mut(), "IF OBJECT_ID('dbo.dbine_chart') IS NOT NULL DROP TABLE dbo.dbine_chart").await;
    exec(setup.as_mut(), "CREATE TABLE dbo.dbine_chart (id int, region nvarchar(10), cat nvarchar(10), v int)").await;
    fill_sql(setup.as_mut(), "dbo.dbine_chart", false).await;
    // An ORDER BY a derived table refuses: dropped from the grouped statement.
    let q = "SELECT region, cat, v FROM dbo.dbine_chart ORDER BY id";
    // AVG of an int truncates in T-SQL: the chart's is in float.
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v * 1.0E0), MIN(v), MAX(v) FROM dbo.dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbo.dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("sqlserver", q, by_x, by_xk)).await;
    // A CTE can't go in a derived table there: the client path.
    let cte = "WITH t AS (SELECT * FROM dbo.dbine_chart) SELECT region, cat, v FROM t";
    let got = run(d, s.as_mut(), cte, &ChartKind::Group(spec("region", "v", None))).await;
    assert_eq!(got.method, AggregateMethod::Client);
    assert_matches("sqlserver cte", &got, &rows(&exec(setup.as_mut(), by_x).await));
    exec(setup.as_mut(), "DROP TABLE dbo.dbine_chart").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_oracle() {
    let mut cfg = cfg("oracle", "DBINE_TEST_ORACLE_URL", "oracle://system:Secret123@localhost:25601");
    cfg.options.insert("service".into(), "FREEPDB1".into());
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    let mut out = QueryOutcome::default();
    let _ = setup.execute("DROP TABLE dbine_chart", 10, &mut out).await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id NUMBER, region VARCHAR2(10), cat VARCHAR2(10), v NUMBER)").await;
    fill_sql(setup.as_mut(), "dbine_chart", true).await;
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    let case = Case { engine: "oracle", query: q, x: "REGION", k: "CAT", v: "V", by_x, by_xk, server: true };
    check(d, s.as_mut(), setup.as_mut(), case).await;
    exec(setup.as_mut(), "DROP TABLE dbine_chart").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_clickhouse() {
    let cfg = cfg("clickhouse", "DBINE_TEST_CLICKHOUSE_URL", "http://dbine:dbine@localhost:25123");
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    exec(setup.as_mut(), "DROP TABLE IF EXISTS dbine_chart").await;
    exec(setup.as_mut(), "CREATE TABLE dbine_chart (id Int32, region Nullable(String), cat String, v Nullable(Int32)) ENGINE = Memory").await;
    fill_sql(setup.as_mut(), "dbine_chart", false).await;
    let q = "SELECT region, cat, v FROM dbine_chart ORDER BY id";
    let by_x = "SELECT region, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM dbine_chart GROUP BY region";
    let by_xk = "SELECT region, cat, COUNT(*), SUM(v) FROM dbine_chart GROUP BY region, cat";
    check(d, s.as_mut(), setup.as_mut(), standard_case("clickhouse", q, by_x, by_xk)).await;
    exec(setup.as_mut(), "DROP TABLE dbine_chart").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_mongodb() {
    let url = std::env::var("DBINE_TEST_MONGODB_URL").unwrap_or_else(|_| "mongodb://root:secret@localhost:25201/?authSource=admin".into());
    let mut cfg = ConnectionConfig { driver: "mongodb".into(), database: "dbine_chart_db".into(), ..Default::default() };
    cfg.options.insert("connection_string".into(), url);
    let (d, mut setup, mut s) = sessions(&cfg, Some("dbine_chart_db")).await;
    let mut out = QueryOutcome::default();
    let _ = setup.execute("db.getCollection(\"dbine_chart\").drop()", 10, &mut out).await;
    let docs: Vec<String> = (1..=ROWS)
        .map(|i| {
            let (id, region, cat, v) = sample(i);
            let region = region.map_or("null".to_string(), |r| format!("\"{r}\""));
            let v = v.map_or("null".to_string(), |v| v.to_string());
            format!("{{ _id: {id}, region: {region}, cat: \"{cat}\", v: {v} }}")
        })
        .collect();
    exec(setup.as_mut(), &format!("db.getCollection(\"dbine_chart\").insertMany([{}])", docs.join(", "))).await;
    let q = "db.getCollection(\"dbine_chart\").find({})";
    let by_x = r#"db.getCollection("dbine_chart").aggregate([{ $group: { _id: "$region", n: { $sum: 1 }, c: { $sum: { $cond: [{ $eq: [{ $ifNull: ["$v", null] }, null] }, 0, 1] } }, s: { $sum: "$v" }, a: { $avg: "$v" }, mn: { $min: "$v" }, mx: { $max: "$v" } } }])"#;
    let case = Case { engine: "mongodb", query: q, x: "region", k: "cat", v: "v", by_x: "", by_xk: "", server: false };
    check(d, s.as_mut(), setup.as_mut(), case).await;
    // The hand-written $group gives the same numbers.
    let got = run(d, s.as_mut(), q, &ChartKind::Group(spec("region", "v", None))).await;
    let expected: Vec<Vec<Value>> = rows(&exec(setup.as_mut(), by_x).await);
    eprintln!("[mongodb] $group: {expected:?}");
    let expected: Vec<Vec<Value>> = expected.into_iter().map(|r| r.into_iter().map(|v| if v.is_string() && v.as_str() == Some("null") { Value::Null } else { v }).collect()).collect();
    assert_matches("mongodb $group", &got, &expected);
    let _ = setup.drop_database("dbine_chart_db").await;
}

#[tokio::test]
#[ignore]
async fn chart_live_redis() {
    let cfg = cfg("redis", "DBINE_TEST_REDIS_URL", "redis://localhost:25400");
    let (d, mut setup, mut s) = sessions(&cfg, None).await;
    let _ = exec(setup.as_mut(), "DEL dbine_chart").await;
    let values: Vec<String> = (1..=ROWS).map(|i| sample(i).3.unwrap_or(0).to_string()).collect();
    exec(setup.as_mut(), &format!("RPUSH dbine_chart {}", values.join(" "))).await;
    let q = "LRANGE dbine_chart 0 -1";
    let out = exec(setup.as_mut(), q).await;
    let col = out.results.iter().find(|r| !r.columns.is_empty()).map(|r| r.columns.last().unwrap().name.clone()).unwrap();
    eprintln!("[redis] columns: {:?}", out.results[0].columns);
    let chart = ChartKind::Group(AggregateSpec {
        x: col.clone(),
        split: None,
        measures: vec![Measure { column: None, func: AggFn::Count }],
        max_groups: 1000,
    });
    let got = run(d, s.as_mut(), q, &chart).await;
    assert_eq!(got.method, AggregateMethod::Client);
    assert_eq!(got.rows_scanned, Some(u64::from(ROWS)));
    let zero = got.categories.iter().position(|c| c.as_deref() == Some("0")).unwrap();
    let zeros = (1..=ROWS).filter(|i| sample(*i).3.unwrap_or(0) == 0).count() as f64;
    assert_eq!(got.series[0].values[zero], Some(zeros));
    let h = run(d, s.as_mut(), q, &ChartKind::Histogram { column: col, bins: 4 }).await;
    assert_eq!(h.histogram.unwrap().counts.iter().sum::<u64>(), u64::from(ROWS));
    exec(setup.as_mut(), "DEL dbine_chart").await;
}

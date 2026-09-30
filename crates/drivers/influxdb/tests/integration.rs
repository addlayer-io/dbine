//! Against real servers (each test needs its variable):
//! - `influxdb:2` in setup mode (org `dbine`, bucket `test`, admin token
//!   `dbinetoken`): `DBINE_TEST_INFLUXDB_URL=http://localhost:25403`
//!   (`DBINE_TEST_INFLUXDB_ORG` / `DBINE_TEST_INFLUXDB_TOKEN` override those);
//! - `influxdb:1.8`: `DBINE_TEST_INFLUXDB1_URL=http://localhost:25404`;
//! - `influxdb:3-core` (`serve --without-auth`): `DBINE_TEST_INFLUXDB3_URL=http://localhost:25409`.
//!
//! Run with `cargo test -p dbine-driver-influxdb -- --ignored`.

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;

async fn open(driver: &str, cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    d.connect(cfg, db).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, max_rows, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

fn measurement(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::MEASUREMENT.into(), schema: None, name: name.into() }
}

fn col_names(out: &QueryOutcome, i: usize) -> Vec<String> {
    out.results[i].columns.iter().map(|c| c.name.clone()).collect()
}

/// Line protocol: 2 hosts × 3 points of cpu, 1 point of mem (seconds).
const LINES: &str = "cpu,host=a value=1.5,n=1i 1706708700\n\
cpu,host=a value=2.5,n=2i 1706708760\n\
cpu,host=b value=3.5,n=3i 1706708820\n\
cpu,host=b value=4.5 1706708880\n\
cpu,host=a value=5.5 1706708940\n\
cpu,host=b value=6.5 1706709000\n\
mem,host=a used=10i 1706708700";

#[tokio::test]
#[ignore]
async fn influxdb2_flux() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").expect("DBINE_TEST_INFLUXDB_URL");
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{url}/api/v2/write?org={org}&bucket=test&precision=s"))
        .header("Authorization", format!("Token {token}"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org.clone());
    cfg.options.insert("token".into(), token.clone());
    let mut s = open("influxdb", &cfg, None).await;
    println!("{}", s.server_version().await.unwrap());
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"test".into()) && !dbs.iter().any(|b| b.starts_with('_')), "{dbs:?}");

    let mut s = open("influxdb", &cfg, Some("test")).await;
    let objs: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(objs, ["cpu", "mem"]);
    let cols: Vec<String> = s.columns(&measurement("cpu")).await.unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(cols, ["_time", "host", "n", "value"]);
    assert!(s.definition(&measurement("cpu")).await.unwrap().is_none());

    let q = s.browse_query(&measurement("cpu"), 4);
    println!("{q}");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results.len(), 1, "{out:?}");
    let names = col_names(&out, 0);
    println!("{names:?} {:?}", out.results[0].rows);
    assert!(names.contains(&"value".into()) && names.contains(&"host".into()) && names.contains(&"_time".into()));
    assert_eq!(out.results[0].rows.len(), 4);

    // Raw rows: one table per field type; max_rows.
    let out = run(&mut s, "from(bucket: \"test\") |> range(start: 0) |> filter(fn: (r) => r._measurement == \"cpu\")", 3).await;
    assert_eq!(out.results.len(), 2);
    assert_eq!(out.results[1].total_rows, 6);
    assert_eq!(out.results[1].rows.len(), 3);
    assert!(out.results[1].truncated);
    let t = col_names(&out, 1).iter().position(|c| c == "_time").unwrap();
    assert_eq!(out.results[1].rows[0][t], json!("2024-01-31 13:45:00"));

    // Errors: syntax, unknown bucket.
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("from(bucket: ", 10, &mut out).await, Err(Error::Query(_))));
    let e = s.execute("from(bucket: \"nope\") |> range(start: 0)", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("nope")), "{e:?}");

    // Read-only refuses to().
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = open("influxdb", &ro_cfg, Some("test")).await;
    let e = ro
        .execute("from(bucket: \"test\") |> range(start: 0) |> to(bucket: \"test\")", 10, &mut out)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("to()")));
    run(&mut ro, &q, 10).await;

    // Bad token.
    let mut bad = cfg.clone();
    bad.options.insert("token".into(), "nope".into());
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == "influxdb").unwrap();
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))));
}

#[tokio::test]
#[ignore]
async fn influxdb1_influxql() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").expect("DBINE_TEST_INFLUXDB1_URL");
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb1", &cfg, None).await;
    println!("{}", s.server_version().await.unwrap());
    run(&mut s, "DROP DATABASE dbine_it; CREATE DATABASE dbine_it", 10).await;
    let http = reqwest::Client::new();
    let r = http.post(format!("{url}/write?db=dbine_it&precision=s")).body(LINES).send().await.unwrap();
    assert!(r.status().is_success());
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dbine_it".into()) && !dbs.contains(&"_internal".into()));

    let mut s = open("influxdb1", &cfg, Some("dbine_it")).await;
    let objs: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(objs, ["cpu", "mem"]);
    let cols = s.columns(&measurement("cpu")).await.unwrap();
    let names: Vec<_> = cols.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(names, [("time", "time"), ("host", "tag"), ("n", "integer"), ("value", "float")]);

    let q = s.browse_query(&measurement("cpu"), 2);
    let out = run(&mut s, &q, 100).await;
    assert_eq!(col_names(&out, 0), ["time", "host", "n", "value"]);
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].rows[0][0], json!("2024-01-31 13:50:00"));

    // Several statements, GROUP BY series, max_rows, an empty result.
    let out = run(&mut s, "SELECT mean(value) FROM cpu GROUP BY host; SELECT * FROM cpu; SELECT * FROM nothere", 4).await;
    assert_eq!(out.results.len(), 3);
    assert_eq!(col_names(&out, 0), ["name", "host", "time", "mean"]);
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[1].total_rows, 6);
    assert!(out.results[1].truncated);
    assert_eq!(out.results[2].rows_affected, Some(0));

    // A failing statement keeps the ones before.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT * FROM cpu LIMIT 1; SELECT FROM", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");

    // Read-only: refused before the server, and SELECT … INTO too.
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = open("influxdb1", &ro_cfg, Some("dbine_it")).await;
    run(&mut ro, "SHOW MEASUREMENTS; SELECT * FROM cpu LIMIT 1", 10).await;
    for w in ["DROP MEASUREMENT cpu", "SELECT * INTO cpu2 FROM cpu"] {
        let e = ro.execute(w, 10, &mut out).await.unwrap_err();
        assert!(matches!(e, Error::Query(ref m) if m.contains("solo lectura")), "{e:?}");
    }
    run(&mut s, "DROP DATABASE dbine_it", 10).await;
}

#[tokio::test]
#[ignore]
async fn influxdb3_sql() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").expect("DBINE_TEST_INFLUXDB3_URL");
    let http = reqwest::Client::new();
    let r = http.post(format!("{url}/api/v3/write_lp?db=dbine_it&precision=second")).body(LINES).send().await.unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);

    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb3", &cfg, None).await;
    println!("{}", s.server_version().await.unwrap());
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dbine_it".into()) && !dbs.contains(&"_internal".into()), "{dbs:?}");

    let mut s = open("influxdb3", &cfg, Some("dbine_it")).await;
    let objs: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(objs, ["cpu", "mem"]);
    let cols = s.columns(&measurement("cpu")).await.unwrap();
    println!("{cols:?}");
    let host = cols.iter().find(|c| c.name == "host").unwrap();
    assert!(host.primary_key);
    assert!(cols.iter().find(|c| c.name == "time").unwrap().primary_key);
    assert!(!cols.iter().find(|c| c.name == "value").unwrap().primary_key);

    let q = s.browse_query(&measurement("cpu"), 2);
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows.len(), 2);
    let t = col_names(&out, 0).iter().position(|c| c == "time").unwrap();
    assert_eq!(out.results[0].rows[0][t], json!("2024-01-31 13:50:00"));

    let out = run(&mut s, "SELECT host, count(*) AS n FROM cpu GROUP BY host ORDER BY host; SELECT * FROM cpu", 4).await;
    assert_eq!(col_names(&out, 0), ["host", "n"]);
    assert_eq!(out.results[0].rows, vec![vec![json!("a"), json!(3)], vec![json!("b"), json!(3)]]);
    assert_eq!(out.results[1].total_rows, 6);
    assert!(out.results[1].truncated);

    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT nope FROM cpu", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("nope")), "{e:?}");
    assert_eq!(out.results.len(), 1);
}

#[tokio::test]
#[ignore]
async fn plans_influxql() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else { return };
    let http = reqwest::Client::new();
    http.post(format!("{url}/query")).form(&[("q", "CREATE DATABASE dbine_plan")]).send().await.unwrap();
    http.post(format!("{url}/write?db=dbine_plan&precision=s")).body(LINES).send().await.unwrap();
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb1", &cfg, Some("dbine_plan")).await;
    let q = "SELECT mean(value) FROM cpu GROUP BY host";
    let mut out = QueryOutcome::default();
    s.explain(q, false, 10, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans[0].root.detail, "mean(value::float)", "{:#?}", out.plans[0].root);
    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    let p = &out.plans[0];
    assert!(p.actual);
    assert_eq!(p.root.op, "select", "{}", p.raw);
    assert!(p.root.actual_ms.is_some());
    http.post(format!("{url}/query")).form(&[("q", "DROP DATABASE dbine_plan")]).send().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn plans_flux() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else { return };
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let bucket = std::env::var("DBINE_TEST_INFLUXDB_BUCKET").unwrap_or("test".into());
    reqwest::Client::new()
        .post(format!("{url}/api/v2/write?org={org}&bucket={bucket}&precision=s"))
        .header("Authorization", format!("Token {token}"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org);
    cfg.options.insert("token".into(), token);
    let mut s = open("influxdb", &cfg, Some(&bucket)).await;
    let q = format!(
        "from(bucket: \"{bucket}\") |> range(start: 0) |> filter(fn: (r) => r._measurement == \"cpu\" and r._field == \"value\") |> mean()"
    );
    let mut out = QueryOutcome::default();
    assert!(matches!(s.explain(&q, false, 10, &mut out).await, Err(Error::Unsupported(_))));
    let mut out = QueryOutcome::default();
    s.explain(&q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1, "profiler tables are not results");
    let p = &out.plans[0];
    println!("{}\n{:#?}", p.raw, p.root);
    assert!(p.actual);
    assert_eq!(p.root.op, "Consulta Flux");
    assert!(p.root.actual_ms.is_some());
    assert!(!p.root.children.is_empty());
}

#[tokio::test]
#[ignore]
async fn plans_sql() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else { return };
    reqwest::Client::new().post(format!("{url}/api/v3/write_lp?db=dbine_plan&precision=second")).body(LINES).send().await.unwrap();
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb3", &cfg, Some("dbine_plan")).await;
    let q = "SELECT host, avg(value) FROM cpu WHERE host = 'a' GROUP BY host";
    let mut out = QueryOutcome::default();
    s.explain(q, false, 10, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    let p = &out.plans[0];
    assert!(!p.actual && p.raw.contains("logical_plan"));
    assert!(p.root.op.ends_with("Exec"), "{:#?}", p.root);
    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    let p = &out.plans[0];
    assert!(p.actual);
    assert!(p.root.actual_rows.is_some(), "{}", p.raw);
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// What every InfluxDB variant shares: no measurement DDL, no INSERT script,
/// create / drop database on.
fn no_ddl(id: &str) {
    let d = driver(id);
    let c = d.capabilities();
    assert!(c.create_database && c.drop_database && !c.foreign_keys);
    assert!(d.designer().is_none());
    let t = dbine_driver::TableSchema { name: "cpu".into(), ..Default::default() };
    assert!(matches!(d.table_ddl(&t, Default::default()), Err(Error::Unsupported(_))));
    assert!(matches!(d.insert_script(&measurement("cpu"), &["time".into()], &[vec![json!(1)]]), Err(Error::Unsupported(_))));
}

#[tokio::test]
#[ignore]
async fn influxdb1_schema_and_databases() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").expect("DBINE_TEST_INFLUXDB1_URL");
    no_ddl("influxdb1");
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb1", &cfg, None).await;
    let _ = s.drop_database("dbine_ddl").await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
    let r = reqwest::Client::new().post(format!("{url}/write?db=dbine_ddl&precision=s")).body(LINES).send().await.unwrap();
    assert!(r.status().is_success());

    let mut s = open("influxdb1", &cfg, Some("dbine_ddl")).await;
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cpu", "mem"]);
    let cols: Vec<(&str, &str)> = schema[0].columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(cols, [("time", "time"), ("host", "tag"), ("n", "integer"), ("value", "float")]);
    assert_eq!(schema[0].primary_key.as_ref().unwrap().columns, ["time", "host"]);

    // Templates run once `mi_base` is the database.
    for t in driver("influxdb1").create_templates() {
        run(&mut s, &t.template.replace("{name}", "dbine_tpl").replace("mi_base", "dbine_ddl"), 10).await;
    }
    let out = run(&mut s, "SHOW RETENTION POLICIES; SHOW CONTINUOUS QUERIES", 100).await;
    assert!(out.results[0].rows.iter().any(|r| r[0] == json!("dbine_tpl")));

    let e = s.drop_database("dbine_ddl").await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("esta conexión")), "{e:?}");
    let mut s = open("influxdb1", &cfg, None).await;
    s.drop_database("dbine_ddl").await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
}

#[tokio::test]
#[ignore]
async fn influxdb2_schema_and_buckets() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").expect("DBINE_TEST_INFLUXDB_URL");
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    no_ddl("influxdb");
    assert!(driver("influxdb").create_templates().is_empty());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org.clone());
    cfg.options.insert("token".into(), token.clone());
    let mut s = open("influxdb", &cfg, None).await;
    let _ = s.drop_database("dbine_ddl").await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
    let r = reqwest::Client::new()
        .post(format!("{url}/api/v2/write?org={org}&bucket=dbine_ddl&precision=s"))
        .header("Authorization", format!("Token {token}"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let mut s = open("influxdb", &cfg, Some("dbine_ddl")).await;
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cpu", "mem"]);
    let cols: Vec<(&str, &str)> = schema[0].columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(cols, [("_time", "time"), ("host", "tag"), ("n", "field"), ("value", "field")]);
    assert_eq!(schema[0].primary_key.as_ref().unwrap().columns, ["_time", "host"]);
    assert_eq!(schema[1].columns.len(), 3);

    let e = s.drop_database("dbine_ddl").await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("esta conexión")), "{e:?}");
    let mut s = open("influxdb", &cfg, None).await;
    s.drop_database("dbine_ddl").await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
    assert!(matches!(s.drop_database("dbine_ddl").await, Err(Error::Query(_))));
}

#[tokio::test]
#[ignore]
async fn influxdb3_schema_and_databases() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").expect("DBINE_TEST_INFLUXDB3_URL");
    no_ddl("influxdb3");
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb3", &cfg, None).await;
    let _ = s.drop_database("dbine_ddl").await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
    let r = reqwest::Client::new().post(format!("{url}/api/v3/write_lp?db=dbine_ddl&precision=second")).body(LINES).send().await.unwrap();
    assert!(r.status().is_success());

    let mut s = open("influxdb3", &cfg, Some("dbine_ddl")).await;
    let schema = s.database_schema().await.unwrap();
    println!("{schema:#?}");
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cpu", "mem"]);
    let host = schema[0].columns.iter().find(|c| c.name == "host").unwrap();
    assert_eq!(host.options.get("tag").map(String::as_str), Some("true"));
    let pk = &schema[0].primary_key.as_ref().unwrap().columns;
    assert!(pk.contains(&"host".into()) && pk.contains(&"time".into()) && !pk.contains(&"value".into()));

    let e = s.drop_database("dbine_ddl").await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("esta conexión")), "{e:?}");
    let mut s = open("influxdb3", &cfg, None).await;
    s.drop_database("dbine_ddl").await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&"dbine_ddl".into()));
}

fn metric(s: &dbine_driver::MonitorSnapshot, key: &str) -> Option<f64> {
    s.metrics.iter().find(|m| m.key == key).unwrap_or_else(|| panic!("no metric {key}")).value
}

fn print_snapshot(s: &dbine_driver::MonitorSnapshot) {
    for m in &s.metrics {
        println!("{:<18} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &s.tables {
        println!("[{}] {} rows {:?}", t.key, t.rows.len(), t.rows.first());
    }
    println!("info {:?}\nnotes {:?}", s.info, s.notes);
}

#[tokio::test]
#[ignore]
async fn influxdb1_monitor() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").expect("DBINE_TEST_INFLUXDB1_URL");
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb1", &cfg, None).await;
    run(&mut s, "CREATE DATABASE dbine_mon", 10).await;
    let r = reqwest::Client::new().post(format!("{url}/write?db=dbine_mon&precision=s")).body(LINES).send().await.unwrap();
    assert!(r.status().is_success());
    let snap = s.monitor().await.unwrap();
    print_snapshot(&snap);
    assert!(metric(&snap, "mem_used").unwrap() > 0.0);
    assert!(metric(&snap, "queries").unwrap() > 0.0);
    assert!(metric(&snap, "uptime").unwrap() > 0.0);
    assert!(metric(&snap, "series").unwrap() > 0.0);
    let dbs = snap.tables.iter().find(|t| t.key == "databases").unwrap();
    assert!(dbs.rows.iter().any(|r| r[0] == json!("dbine_mon")), "{:?}", dbs.rows);
    assert!(snap.tables.iter().any(|t| t.key == "queries"));
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
    run(&mut s, "DROP DATABASE dbine_mon", 10).await;
}

#[tokio::test]
#[ignore]
async fn influxdb2_monitor() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").expect("DBINE_TEST_INFLUXDB_URL");
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let r = reqwest::Client::new()
        .post(format!("{url}/api/v2/write?org={org}&bucket=test&precision=s"))
        .header("Authorization", format!("Token {token}"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org);
    cfg.options.insert("token".into(), token);
    let mut s = open("influxdb", &cfg, Some("test")).await;
    run(&mut s, "from(bucket: \"test\") |> range(start: 2024-01-01T00:00:00Z) |> limit(n: 1)", 10).await;
    let snap = s.monitor().await.unwrap();
    print_snapshot(&snap);
    assert!(metric(&snap, "mem_used").unwrap() > 0.0);
    assert!(metric(&snap, "requests").unwrap() > 0.0);
    assert!(metric(&snap, "uptime").unwrap() > 0.0);
    assert!(metric(&snap, "rows_written").unwrap() > 0.0);
    assert!(metric(&snap, "queries").unwrap() > 0.0);
    assert_eq!(metric(&snap, "active_sessions"), Some(0.0));
    let t = snap.tables.iter().find(|t| t.key == "databases").unwrap();
    assert!(t.rows.iter().any(|r| r[0] == json!("test")), "{:?}", t.rows);
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
}

#[tokio::test]
#[ignore]
async fn influxdb3_monitor() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").expect("DBINE_TEST_INFLUXDB3_URL");
    let r = reqwest::Client::new()
        .post(format!("{url}/api/v3/write_lp?db=dbine_mon&precision=second"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url.clone(), ..Default::default() };
    let mut s = open("influxdb3", &cfg, Some("dbine_mon")).await;
    run(&mut s, "SELECT count(*) FROM cpu", 10).await;
    let snap = s.monitor().await.unwrap();
    print_snapshot(&snap);
    assert!(metric(&snap, "mem_used").unwrap() > 0.0);
    assert!(metric(&snap, "queries").unwrap() > 0.0);
    assert!(metric(&snap, "rows_written").unwrap() > 0.0);
    assert!(metric(&snap, "uptime").unwrap() > 0.0);
    let recent = snap.tables.iter().find(|t| t.key == "recent_queries").unwrap();
    assert!(!recent.rows.is_empty());
    assert!(snap.tables.iter().any(|t| t.key == "top_objects"));
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
    // Without a database in the connection it reads the first one.
    let mut s = open("influxdb3", &cfg, None).await;
    assert!(s.monitor().await.unwrap().tables.iter().any(|t| t.key == "recent_queries"));
}

// -- profiler -----------------------------------------------------------------------------

/// The profiler sees another session's queries on the database once each
/// (InfluxDB 1.x: only the slow one, sampled; InfluxDB 3: both, from the
/// query log) and leaves out its own.
async fn profile(id: &str, url: &str, db: &str) {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.supports_profiler(), "{id}");
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() };
    let mut p = open(id, &cfg, None).await;
    let mut w = open(id, &cfg, Some(db)).await;
    let opts = dbine_driver::ProfilerOptions { database: db.into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    let marker = format!("dbine_prof_{}", std::process::id());
    let (slow, fast) = if id == "influxdb1" {
        (
            format!("SELECT PERCENTILE(v, 50) AS {marker}_slow, MEDIAN(v), STDDEV(v) FROM m GROUP BY h"),
            format!("SELECT LAST(v) AS {marker}_fast FROM m"),
        )
    } else {
        (format!("SELECT count(*) AS {marker}_slow FROM m"), format!("SELECT 1 AS {marker}_fast"))
    };
    let work = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let t = std::time::Instant::now();
        run(&mut w, &slow, 10).await;
        eprintln!("{id}: slow query took {:?}", t.elapsed());
        run(&mut w, &fast, 10).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(if complete { 8 } else { 20 });
        while std::time::Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            let n = got.iter().filter(|s| s.text.contains(&marker)).count();
            if n >= if complete { 2 } else { 1 } && !complete {
                break;
            }
            if complete {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    assert_eq!(mine.iter().filter(|s| s.text.contains("_slow")).count(), 1, "{id}: the slow query once");
    if complete {
        assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{id}: the fast query once");
    } else {
        let slow = mine.iter().find(|s| s.text.contains("_slow")).unwrap();
        assert!(slow.duration_ms.unwrap_or(0.0) >= 200.0, "{id}: duration {:?}", slow.duration_ms);
        assert_eq!(slow.database.as_deref(), Some(db));
    }
    assert!(got.iter().all(|s| !s.text.contains("SHOW QUERIES") && !s.text.contains("system.queries")), "{id}: its own queries are left out");
}

/// 200 000 points of `m` (50 series, one per second), from the `part`th 200 000 seconds.
fn profiler_points(part: u64) -> String {
    (part * 200_000..(part + 1) * 200_000).map(|i| format!("m,h=h{} v={i} {}\n", i % 50, 1_700_000_000 + i)).collect()
}

#[tokio::test]
#[ignore]
async fn influxdb1_profiler() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").expect("DBINE_TEST_INFLUXDB1_URL");
    let http = reqwest::Client::new();
    http.post(format!("{url}/query")).form(&[("q", "CREATE DATABASE dbine_prof")]).send().await.unwrap();
    // A million points, so that aggregating them takes about a second.
    for part in 0..5 {
        let r = http.post(format!("{url}/write?db=dbine_prof&precision=s")).body(profiler_points(part)).send().await.unwrap();
        assert!(r.status().is_success());
    }
    profile("influxdb1", &url, "dbine_prof").await;
    http.post(format!("{url}/query")).form(&[("q", "DROP DATABASE dbine_prof")]).send().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn influxdb3_profiler() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").expect("DBINE_TEST_INFLUXDB3_URL");
    let http = reqwest::Client::new();
    let r = http.post(format!("{url}/api/v3/write_lp?db=dbine_prof&precision=second")).body("m,h=a v=1 1706708700").send().await.unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
    profile("influxdb3", &url, "dbine_prof").await;
}

#[test]
fn flux_has_no_profiler() {
    assert!(!driver("influxdb").supports_profiler());
}

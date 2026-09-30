//! "Clonar tabla" on time series and wide-column engines, against the test
//! containers: what is cloned exactly and what is refused (with its reason,
//! and nothing left behind). Ignored by default; each test takes its
//! server's URL from the environment (defaults: the `dbine-test-*` ports):
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! cargo test -p dbine-transfer --test clone_table_timeseries_live -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Each test works in its own database / keyspace (`clonetsx`), dropped at
//! the end.

use dbine_driver::transfer::{BatchSink, BatchSinkRef, Cell, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, ReadSpec, Session};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneReport, CloneRequest, ConfigEndpoints};
use dbine_transfer::Endpoints;
use std::sync::{Arc, Mutex};

const DB: &str = "clonetsx";

fn env(k: &str, default: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

fn endpoints(cfg: &ConnectionConfig, database: Option<&str>) -> Arc<ConfigEndpoints> {
    let driver = dbine_drivers::find(&cfg.driver).unwrap_or_else(|| panic!("no driver '{}'", cfg.driver)).clone();
    Arc::new(ConfigEndpoints { driver, config: cfg.clone(), database: database.map(str::to_string) })
}

async fn clone(ep: &Arc<ConfigEndpoints>, source: ObjectRef, new_name: &str, with_data: bool) -> dbine_driver::Result<CloneReport> {
    let req = CloneRequest { source, new_name: new_name.into(), options: CloneOptions { with_data, with_indexes: true } };
    let ep: Arc<dyn Endpoints> = ep.clone();
    clone_table(ep, req, &CloneControl::default(), |e| println!("{e:?}")).await
}

async fn run(s: &mut dyn Session, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 0, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error {
        panic!("{sql}: {e}");
    }
}

async fn first(s: &mut dyn Session, sql: &str) -> serde_json::Value {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out.results.iter().rev().find_map(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or_default()
}

#[derive(Default)]
struct Collect(Vec<Vec<Cell>>);

impl BatchSink for Collect {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0.extend(b.rows);
        Ok(())
    }
}

/// Every row, as text, sorted.
async fn rows(s: &mut dyn Session, table: &ObjectRef) -> Vec<String> {
    let names: Vec<String> = s.columns(table).await.unwrap().into_iter().map(|c| c.name).collect();
    let sink = Arc::new(Mutex::new(Collect::default()));
    let dyn_sink: BatchSinkRef = sink.clone();
    s.read_batches(&ReadSpec { table: table.clone(), columns: Some(names), filter: None }, dyn_sink).await.unwrap();
    let mut v: Vec<String> = sink.lock().unwrap().0.iter().map(|r| format!("{r:?}")).collect();
    v.sort();
    v
}

/// Columns as (name, type, nullable, key).
async fn cols(s: &mut dyn Session, t: &ObjectRef) -> Vec<(String, String, bool, bool)> {
    s.columns(t).await.unwrap().into_iter().map(|c| (c.name, c.data_type, c.nullable, c.primary_key)).collect()
}

async fn names(s: &mut dyn Session) -> Vec<String> {
    s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect()
}

fn refused(r: dbine_driver::Result<CloneReport>, words: &[&str]) {
    let e = r.expect_err("the clone should have been refused").to_string();
    println!("rechazado: {e}");
    for w in words {
        assert!(e.contains(w), "«{w}» missing from: {e}");
    }
}

// -- TDengine -----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-tdengine"]
async fn tdengine() {
    let url = reqwest_free_url(&env("DBINE_TEST_TDENGINE_URL", "http://localhost:25641"));
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.0,
        port: url.1,
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let ep = endpoints(&cfg, Some(DB));
    let mut admin = endpoints(&cfg, None).open_target().await.unwrap();
    run(&mut *admin, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    run(&mut *admin, &format!("CREATE DATABASE {DB}")).await;
    let mut s = ep.open_target().await.unwrap();
    run(
        &mut *s,
        &format!(
            "CREATE TABLE {DB}.pk2 (ts TIMESTAMP, id INT PRIMARY KEY, v DOUBLE COMPRESS 'zstd' LEVEL 'high', s VARCHAR(10)) COMMENT 'c';\n\
             INSERT INTO {DB}.pk2 VALUES ('2024-01-01 00:00:00.000', 1, 1.5, 'a') ('2024-01-01 00:00:00.000', 2, 2.5, 'b') ('2024-01-01 00:00:01.000', 1, 3.5, NULL);\n\
             CREATE STABLE {DB}.st (ts TIMESTAMP, v INT) TAGS (loc VARCHAR(10));\n\
             CREATE TABLE {DB}.sub1 USING {DB}.st TAGS ('a');\n\
             INSERT INTO {DB}.sub1 VALUES ('2024-01-01 00:00:00.000', 1);"
        ),
    )
    .await;
    let t = |kind: &str, name: &str| ObjectRef { kind: kind.into(), schema: Some(DB.into()), name: name.into() };

    // The composite key and the per-column compression come along: rows
    // that share a timestamp stay apart.
    let r = clone(&ep, t("table", "pk2"), "pk2_c", true).await.unwrap();
    assert_eq!(r.rows, 3);
    assert_eq!(first(&mut *s, &format!("SELECT count(*) FROM {DB}.pk2_c")).await.as_i64(), Some(3));
    let def = |d: Option<String>, n: &str| d.unwrap().replacen(&format!("`{n}`"), "`x`", 1);
    let a = def(s.definition(&t("table", "pk2")).await.unwrap(), "pk2");
    let b = def(s.definition(&t("table", "pk2_c")).await.unwrap(), "pk2_c");
    assert_eq!(a, b);
    assert!(b.contains("COMPOSITE KEY") && b.contains("COMPRESS 'zstd' LEVEL 'high'"), "{b}");
    assert_eq!(rows(&mut *s, &t("table", "pk2")).await, rows(&mut *s, &t("table", "pk2_c")).await);

    // Supertables and subtables: refused before anything is written.
    refused(clone(&ep, t("supertable", "st"), "st_c", true).await, &["no se puede clonar", "supertabla"]);
    refused(clone(&ep, t("subtable", "sub1"), "sub1_c", true).await, &["no se puede clonar", "subtabla"]);
    let n = names(&mut *s).await;
    assert!(!n.iter().any(|x| x == "st_c" || x == "sub1_c"), "{n:?}");

    run(&mut *admin, &format!("DROP DATABASE {DB}")).await;
}

/// `http://host:port` as (host, port), without a URL crate.
fn reqwest_free_url(url: &str) -> (String, u16) {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':').expect("host:port");
    (host.to_string(), port.parse().expect("port"))
}

// -- Cassandra / ScyllaDB -----------------------------------------------------------------------------

async fn cql(driver: &str, url: &str) {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() };
    let mut admin = endpoints(&cfg, None).open_target().await.unwrap();
    run(&mut *admin, &format!("DROP KEYSPACE IF EXISTS {DB}")).await;
    // Scylla: without tablets, which don't take counter tables.
    let tablets = if driver == "scylladb" { " AND tablets = {'enabled': false}" } else { "" };
    run(&mut *admin, &format!("CREATE KEYSPACE {DB} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}")).await;
    let ep = endpoints(&cfg, Some(DB));
    let mut s = ep.open_target().await.unwrap();
    run(&mut *s, "CREATE TABLE ev (p int, c int, v text, PRIMARY KEY (p, c)) WITH CLUSTERING ORDER BY (c DESC) AND default_time_to_live = 86400").await;
    run(&mut *s, "INSERT INTO ev (p, c, v) VALUES (1, 1, 'a')").await;
    run(&mut *s, "INSERT INTO ev (p, c, v) VALUES (1, 2, 'b') USING TTL 500").await;
    run(&mut *s, "CREATE TABLE cnt (k int PRIMARY KEY, n counter)").await;
    run(&mut *s, "UPDATE cnt SET n = n + 5 WHERE k = 1").await;
    let t = |name: &str| ObjectRef { kind: "table".into(), schema: Some(DB.into()), name: name.into() };

    // Keys and clustering order as the original's; the report says what
    // each row's TTL and writetime become.
    let r = clone(&ep, t("ev"), "ev_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    assert!(r.notes.iter().any(|n| n.contains("TTL") && n.contains("86400 s") && n.contains("writetime")), "{:?}", r.notes);
    let schema = s.database_schema().await.unwrap();
    let a = schema.iter().find(|x| x.name == "ev").unwrap();
    let b = schema.iter().find(|x| x.name == "ev_c").unwrap();
    assert_eq!(a.columns, b.columns);
    assert_eq!(a.primary_key, b.primary_key);
    assert_eq!(a.options, b.options);
    assert_eq!(rows(&mut *s, &t("ev")).await, rows(&mut *s, &t("ev_c")).await);

    // Counters: refused up front, in Spanish, nothing created.
    refused(clone(&ep, t("cnt"), "cnt_c", true).await, &["no se puede clonar", "counter", "UPDATE"]);
    // Names CQL can't take.
    refused(clone(&ep, t("ev"), "ñandú", true).await, &["guion bajo"]);
    refused(clone(&ep, t("ev"), "src clone", true).await, &["guion bajo"]);
    let n = names(&mut *s).await;
    assert!(!n.iter().any(|x| x == "cnt_c" || x.contains(' ') || !x.is_ascii()), "{n:?}");

    run(&mut *admin, &format!("DROP KEYSPACE {DB}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-cassandra"]
async fn cassandra() {
    cql("cassandra", &env("DBINE_TEST_CASSANDRA_URL", "localhost:25402")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-scylladb"]
async fn scylladb() {
    cql("scylladb", &env("DBINE_TEST_SCYLLADB_URL", "localhost:25413")).await;
}

// -- IoTDB ----------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-iotdb"]
async fn iotdb() {
    let cfg = ConnectionConfig {
        driver: "iotdb".into(),
        host: env("DBINE_TEST_IOTDB_URL", "http://localhost:25405"),
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    };
    let db = format!("root.{DB}");
    let mut admin = endpoints(&cfg, None).open_target().await.unwrap();
    let _ = admin.drop_database(&db).await;
    admin.create_database(&db).await.unwrap();
    let ep = endpoints(&cfg, Some(&db));
    let mut s = ep.open_target().await.unwrap();
    for sql in [
        format!("CREATE TIMESERIES {db}.dn.s1 WITH DATATYPE=INT32, ENCODING=TS_2DIFF, COMPRESSOR=LZ4"),
        format!("CREATE TIMESERIES {db}.dn.s2 WITH DATATYPE=DOUBLE"),
        format!("INSERT INTO {db}.dn(time, s1, s2) VALUES (1000, 1, 1.5), (2000, 2, NULL)"),
        format!("CREATE TIMESERIES {db}.dt.s(al) WITH DATATYPE=INT32 TAGS(owner=x) ATTRIBUTES(descr=d)"),
        format!("INSERT INTO {db}.dt(time, s) VALUES (1000, 1)"),
        format!("CREATE TIMESERIES {db}.px.inner.s WITH DATATYPE=INT32"),
        format!("INSERT INTO {db}.px.inner(time, s) VALUES (1000, 7)"),
    ] {
        run(&mut *s, &sql).await;
    }
    // As the explorer lists devices: no schema.
    let d = |name: &str| ObjectRef { kind: "device".into(), schema: None, name: name.into() };

    let r = clone(&ep, d("dn"), "dn_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    assert_eq!(cols(&mut *s, &d("dn")).await, cols(&mut *s, &d("dn_c")).await);
    assert_eq!(rows(&mut *s, &d("dn")).await, rows(&mut *s, &d("dn_c")).await);

    // Alias, tags and attributes would be lost: refused.
    refused(clone(&ep, d("dt"), "dt_c", true).await, &["no se puede clonar", "alias", "tags", "atributos"]);
    // A path that holds another device, and a nested name: refused, and
    // the other device keeps its data.
    refused(clone(&ep, d("dn"), "px", true).await, &["ya hay series"]);
    refused(clone(&ep, d("dn"), "dn.sub", true).await, &["IoTDB"]);
    assert_eq!(first(&mut *s, &format!("SELECT count(s) FROM {db}.px.inner")).await.as_i64(), Some(1));
    let n = names(&mut *s).await;
    assert!(!n.iter().any(|x| x == "dt_c" || x == "px" || x == "dn.sub"), "{n:?}");

    admin.drop_database(&db).await.unwrap();
}

// -- InfluxDB 1 -------------------------------------------------------------------------------------------

/// A raw HTTP/1.1 POST (line protocol writes), without an HTTP crate.
fn post(url: &str, path: &str, body: &str) {
    use std::io::{Read, Write};
    let (host, port) = reqwest_free_url(url);
    let mut c = std::net::TcpStream::connect((host.as_str(), port)).unwrap();
    write!(c, "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut answer = String::new();
    c.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 204"), "{answer}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-influxdb1"]
async fn influxdb1() {
    let url = env("DBINE_TEST_INFLUXDB1_URL", "http://localhost:25404");
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut admin = endpoints(&cfg, None).open_target().await.unwrap();
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let ep = endpoints(&cfg, Some(DB));
    let mut s = ep.open_target().await.unwrap();
    post(
        &url,
        &format!("/write?db={DB}"),
        "m,host=a,region=x v=1.0,n=2i,s=\"t\",ok=true 1000000000\nm,host=b v=2.5,n=3i 2000000000\nm v=1,s=\"u\" 3000000000\n",
    );
    run(&mut *s, &format!("CREATE RETENTION POLICY other ON {DB} DURATION 1d REPLICATION 1")).await;
    post(&url, &format!("/write?db={DB}&rp=other"), "r,host=a v=1\n");
    let m = |name: &str| ObjectRef { kind: "measurement".into(), schema: None, name: name.into() };

    // Tags stay tags, fields keep their types (a float 1.0 too).
    let r = clone(&ep, m("m"), "m_c", true).await.unwrap();
    assert_eq!(r.rows, 3);
    assert_eq!(cols(&mut *s, &m("m")).await, cols(&mut *s, &m("m_c")).await);
    assert_eq!(rows(&mut *s, &m("m")).await, rows(&mut *s, &m("m_c")).await);

    // Points under a policy the copy doesn't read: refused.
    refused(clone(&ep, m("r"), "r_c", true).await, &["no se puede clonar", "política de retención", "other"]);
    // Without data there is no measurement.
    refused(clone(&ep, m("m"), "m_d", false).await, &["no se puede clonar", "puntos"]);
    // A tag and a field with one name (InfluxDB allows it once the
    // measurement exists, but no point can carry both): refused in Spanish.
    post(&url, &format!("/write?db={DB}"), "dup,host=a v=1i 1000000000\n");
    post(&url, &format!("/write?db={DB}"), "dup host=\"f1\" 2000000000\n");
    refused(clone(&ep, m("dup"), "dup_c", true).await, &["no se puede clonar", "«host»", "tag y field"]);
    let n = names(&mut *s).await;
    assert!(!n.iter().any(|x| x == "r_c" || x == "m_d" || x == "dup_c"), "{n:?}");

    admin.drop_database(DB).await.unwrap();
}

#[test]
fn influxdb2_and_3_are_refused_before_connecting() {
    for id in ["influxdb", "influxdb3"] {
        let d = dbine_drivers::find(id).unwrap();
        let e = dbine_transfer::clone_table::check_cloneable(&**d, "measurement").unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)) && e.to_string().contains("deshacer el clon"), "{e}");
    }
    let td = dbine_drivers::find("tdengine").unwrap();
    assert!(dbine_transfer::clone_table::check_cloneable(&**td, "supertable").is_err());
    assert!(dbine_transfer::clone_table::check_cloneable(&**td, "table").is_ok());
    assert!(dbine_transfer::clone_table::check_cloneable(&**dbine_drivers::find("influxdb1").unwrap(), "measurement").is_ok());
}

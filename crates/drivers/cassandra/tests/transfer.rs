//! Bulk load and typed read against real servers (see `integration.rs` for
//! the containers):
//! `DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!  cargo test -p dbine-driver-cassandra --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (host, port) = url.rsplit_once(':').unwrap();
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() }
}

async fn open(driver: &str, url: &str, ks: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_bulk_load());
    d.connect(&cfg(driver, url), ks).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 10, &mut out).await {
        panic!("{text}: {e}");
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

fn row(i: usize) -> Vec<Cell> {
    let i64_ = i as i64;
    vec![
        Cell::Int(i64_),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Int(i64_ * 1_000_003) },
        Cell::Decimal(format!("{}.{:02}", i64_ - 50_000, i % 100)),
        Cell::Float(i as f64 / 8.0),
        Cell::Text(format!("fila {i} ñ")),
        Cell::Bytes(vec![(i % 256) as u8; i % 40]),
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::Time(format!("{:02}:{:02}:{:02}", i % 24, i % 60, i % 60)),
        Cell::DateTimeTz(format!("2024-01-{:02} 10:00:00.{:03}+00:00", i % 28 + 1, i % 1000)),
        Cell::Uuid(format!("00000000-0000-4000-8000-{i:012x}")),
        Cell::Json(format!("[{i},{}]", i + 1)),
        Cell::Json(format!("{{\"k{}\":{i}}}", i % 3)),
    ]
}

/// What the read gives back for [`row`] (bytes, empty blobs, trimmed text).
fn expected(i: usize) -> Vec<Cell> {
    let mut r = row(i);
    let ts = format!("2024-01-{:02} 10:00:00.{:03}", i % 28 + 1, i % 1000);
    r[8] = Cell::DateTimeTz(format!("{}+00:00", if i.is_multiple_of(1000) { ts.trim_end_matches(".000").to_string() } else { ts }));
    r
}

async fn transfer(driver: &str, url: &str) {
    let mut s = open(driver, url, None).await;
    run(
        &mut s,
        "DROP KEYSPACE IF EXISTS dbine_xfer;
         CREATE KEYSPACE dbine_xfer WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};",
    )
    .await;
    let mut s = open(driver, url, Some("dbine_xfer")).await;
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, n bigint, d decimal, f double, s text, b blob, dt date, tm time,
                         ts timestamp, u uuid, l list<int>, m map<text, int>);",
    )
    .await;
    let names = ["id", "n", "d", "f", "s", "b", "dt", "tm", "ts", "u", "l", "m"];
    let table = ObjectRef { kind: "table".into(), schema: None, name: "t".into() };
    let batches: Vec<RowBatch> =
        (0..ROWS).collect::<Vec<_>>().chunks(1000).map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 }).collect();
    let spec = LoadSpec {
        table: table.clone(),
        columns: names.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("{driver}: loaded {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);
    let reports = reports.into_inner().unwrap();
    assert_eq!(reports.len(), 10, "{reports:?}");
    assert_eq!(reports.last(), Some(&(ROWS as u64)));

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("{driver}: read {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, ROWS as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    // Table order: partition key first, then the rest by name.
    let order: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(order[0], "id");
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    for (i, r) in rows.iter().enumerate() {
        let want = expected(i);
        for (c, name) in order.iter().enumerate() {
            let w = &want[names.iter().position(|n| n == name).unwrap()];
            // An empty blob reads back as an empty blob.
            assert_eq!(&r[c], w, "row {i}, column {name}");
        }
    }

    // Filtered read.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["id".into(), "s".into()]), filter: Some("id IN (1, 2, 3)".into()) };
    assert_eq!(s.read_batches(&spec, sink).await.unwrap(), 3);
}

/// A fresh keyspace `ks`, and a session in it.
async fn fresh(driver: &str, url: &str, ks: &str) -> Box<dyn Session> {
    let mut s = open(driver, url, None).await;
    run(
        &mut s,
        &format!(
            "DROP KEYSPACE IF EXISTS {ks};
             CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}};"
        ),
    )
    .await;
    open(driver, url, Some(ks)).await
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: name.into() }
}

fn load_spec(name: &str, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

async fn read_all(s: &mut Box<dyn Session>, name: &str) -> Vec<Vec<Cell>> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table(name), columns: None, filter: None }, sink.clone()).await.unwrap();
    let mut rows = std::mem::take(&mut sink.lock().unwrap().rows);
    rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    rows
}

/// A load that fails leaves nothing landing after it returns: the requests
/// already sent are awaited first.
async fn failed_load_settles(driver: &str, url: &str) {
    let mut s = fresh(driver, url, "dbine_xfer_fail").await;
    run(&mut s, "CREATE TABLE t (id int PRIMARY KEY, b blob);").await;
    let blob = vec![7u8; 40 * 1024];
    let mut rows: Vec<Vec<Cell>> = (0..1500).map(|i| vec![Cell::Int(i), Cell::Bytes(blob.clone())]).collect();
    rows.push(vec![Cell::Text("no es un int".into()), Cell::Null]);
    let batches: Vec<RowBatch> = rows.chunks(40).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    let err = s.bulk_load(&load_spec("t", &["id", "b"]), &[], &mut Batches(batches.into_iter()), &|_| {}).await.unwrap_err();
    println!("{driver}: failed as expected: {err}");
    async fn count(s: &mut Box<dyn Session>) -> u64 {
        let sink = Arc::new(Mutex::new(Collect::default()));
        let spec = ReadSpec { table: table("t"), columns: Some(vec!["id".into()]), filter: None };
        s.read_batches(&spec, sink).await.unwrap()
    }
    let first = count(&mut s).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let later = count(&mut s).await;
    assert_eq!(first, later, "rows landed after bulk_load returned");
    assert_eq!(first, 1500);
}

/// Legal CQL values past the usual ranges copy between tables unchanged.
async fn edge_values(driver: &str, url: &str) {
    let mut s = fresh(driver, url, "dbine_xfer_edge").await;
    let digits: String = (0..10_000).map(|i| char::from(b'1' + (i % 9) as u8)).collect();
    run(&mut s, "CREATE TABLE src (id int PRIMARY KEY, d date, ts timestamp, v varint, i int, n decimal);").await;
    run(&mut s, "CREATE TABLE dst (id int PRIMARY KEY, d date, ts timestamp, v varint, i int, n decimal);").await;
    // A decimal literal with a fraction goes through a double in Cassandra 5
    // (over ~308 digits it's `Infinity`, a NumberFormatException): the long
    // one goes as JSON text, which is parsed exactly.
    run(
        &mut s,
        &format!(
            "INSERT INTO src (id, d, ts, v, i, n) VALUES (1, '-0001-01-01', 9223372036854775807, {digits}, blobAsInt(0x), fromJson('\"-{digits}.5\"'));
             INSERT INTO src (id, d, ts, v, i, n) VALUES (2, 2150416545, -9223372036854775808, -{digits}, 7, 0.001);
             INSERT INTO src (id, d, ts) VALUES (3, 0, 0);
             INSERT INTO src (id, i) VALUES (4, null);"
        ),
    )
    .await;
    let src = read_all(&mut s, "src").await;
    let cols = ["id", "d", "i", "n", "ts", "v"];
    // Table order: key, then the rest by name.
    assert_eq!(src[0][1], Cell::Date("-0001-01-01".into()));
    assert_eq!(src[0][2], Cell::Text(String::new()), "an empty int is not NULL");
    assert_eq!(src[0][3], Cell::Decimal(format!("-{digits}.5")));
    assert_eq!(src[0][4], Cell::DateTimeTz("+292278994-08-17 07:12:55.807+00:00".into()));
    assert_eq!(src[0][5], Cell::Decimal(digits.clone()));
    assert_eq!(src[1][1], Cell::Date("+10000-01-01".into()));
    assert_eq!(src[2][1], Cell::Date("-5877641-06-23".into()));
    assert_eq!(src[3][2], Cell::Null);
    let batches = vec![RowBatch { rows: src.clone(), bytes: 0 }];
    let n = s.bulk_load(&load_spec("dst", &cols), &[], &mut Batches(batches.into_iter()), &|_| {}).await.unwrap();
    assert_eq!(n, 4);
    assert_eq!(read_all(&mut s, "dst").await, src);

    // Finer than a millisecond: refused, not truncated.
    let rows = vec![vec![Cell::Int(9), Cell::DateTimeTz("2024-01-01 00:00:00.123456+00:00".into())]];
    let err = s.bulk_load(&load_spec("dst", &["id", "ts"]), &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|_| {}).await.unwrap_err();
    assert!(matches!(err, dbine_driver::Error::Unsupported(_)), "{err:?}");

    // An unknown column is an error, not an empty one.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table("src"), columns: Some(vec!["id".into(), "nope".into()]), filter: None };
    assert!(s.read_batches(&spec, sink).await.is_err());
}

/// Values the target can't hold whole are refused, never trimmed: a UDT
/// without one of the source's fields, a longer tuple, a repeated map key,
/// a date-time into a date.
async fn nothing_dropped(driver: &str, url: &str) {
    let mut s = fresh(driver, url, "dbine_xfer_drop").await;
    run(
        &mut s,
        "CREATE TYPE addr_src (city text, zip int);
         CREATE TYPE addr_dst (city text);
         CREATE TABLE src (id int PRIMARY KEY, a frozen<addr_src>, tp frozen<tuple<int, text>>);
         CREATE TABLE dst (id int PRIMARY KEY, a frozen<addr_dst>, tp frozen<tuple<int>>, m map<text, int>, d date, t time);
         INSERT INTO src (id, a, tp) VALUES (1, {city: 'Rosario', zip: 2000}, (1, 'lost'));",
    )
    .await;
    let src = read_all(&mut s, "src").await;
    let load = |cols: &[&str], rows: Vec<Vec<Cell>>| (load_spec("dst", cols), vec![RowBatch { rows, bytes: 0 }]);
    let cases = [
        load(&["id", "a"], vec![vec![src[0][0].clone(), src[0][1].clone()]]),
        load(&["id", "tp"], vec![vec![src[0][0].clone(), src[0][2].clone()]]),
        load(&["id", "m"], vec![vec![Cell::Int(1), Cell::Json(r#"{"a":1,"a":2}"#.into())]]),
        load(&["id", "d"], vec![vec![Cell::Int(1), Cell::DateTime("2024-01-01 12:34:56".into())]]),
        load(&["id", "t"], vec![vec![Cell::Int(1), Cell::DateTimeTz("2024-01-01 12:34:56+00:00".into())]]),
        load(&["id", "t"], vec![vec![Cell::Int(1), Cell::DateTime("1970-01-01 00:00:00".into())]]),
    ];
    for (spec, batches) in cases {
        let err = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|_| {}).await.unwrap_err();
        assert!(matches!(err, dbine_driver::Error::Unsupported(_)), "{:?}: {err:?}", spec.columns);
    }
    assert!(read_all(&mut s, "dst").await.is_empty());
    // A bare time loads.
    let (spec, batches) = load(&["id", "t"], vec![vec![Cell::Int(2), Cell::Text("12:34:56.789".into())]]);
    assert_eq!(s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|_| {}).await.unwrap(), 1);
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table("dst"), columns: Some(vec!["id".into(), "t".into()]), filter: None }, sink.clone()).await.unwrap();
    assert_eq!(sink.lock().unwrap().rows, vec![vec![Cell::Int(2), Cell::Time("12:34:56.789000000".into())]]);
}

/// Wide rows read in pages that stay small (Apache Cassandra pages by rows
/// only).
async fn wide_rows(driver: &str, url: &str) {
    let mut s = fresh(driver, url, "dbine_xfer_wide").await;
    run(&mut s, "CREATE TABLE w (id int PRIMARY KEY, b blob);").await;
    let blob = vec![3u8; 100 * 1024];
    let rows: Vec<Vec<Cell>> = (0..300).map(|i| vec![Cell::Int(i), Cell::Bytes(blob.clone())]).collect();
    let batches: Vec<RowBatch> = rows.chunks(20).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    assert_eq!(s.bulk_load(&load_spec("w", &["id", "b"]), &[], &mut Batches(batches.into_iter()), &|_| {}).await.unwrap(), 300);
    let got = read_all(&mut s, "w").await;
    assert_eq!(got.len(), 300);
    assert!(got.iter().all(|r| r[1] == Cell::Bytes(blob.clone())));
}

#[tokio::test]
#[ignore]
async fn transfer_cassandra() {
    let url = std::env::var("DBINE_TEST_CASSANDRA_URL").unwrap_or_else(|_| "localhost:25402".into());
    transfer("cassandra", &url).await;
    failed_load_settles("cassandra", &url).await;
    nothing_dropped("cassandra", &url).await;
    edge_values("cassandra", &url).await;
    wide_rows("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn transfer_scylladb() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").unwrap_or_else(|_| "localhost:25413".into());
    transfer("scylladb", &url).await;
    failed_load_settles("scylladb", &url).await;
    nothing_dropped("scylladb", &url).await;
    edge_values("scylladb", &url).await;
    wide_rows("scylladb", &url).await;
}

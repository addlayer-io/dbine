//! Bulk transfer against a real IoTDB (ignored by default; containers as in
//! `integration.rs`). IoTDB 2 (STRING, BLOB, DATE and TIMESTAMP series)
//! too when `DBINE_TEST_IOTDB2_URL` is set:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const DB: &str = "root.dbine_transfer";
/// 2024-01-01 00:00:00 UTC, in ms.
const T0: i64 = 1_704_067_200_000;

struct Batches(std::vec::IntoIter<RowBatch>);

#[async_trait]
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
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    let v: Vec<RowBatch> = rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Batches(v.into_iter())
}

async fn open(url: &str) -> Box<dyn Session> {
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.supports_bulk_load());
    let cfg = ConnectionConfig { driver: "iotdb".into(), host: url.into(), username: Some("root".into()), password: Some("root".into()), ..Default::default() };
    d.connect(&cfg, Some(DB)).await.unwrap()
}

async fn run(s: &mut dyn Session, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn read(s: &mut dyn Session, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.expect("read");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

fn obj(name: &str) -> ObjectRef {
    ObjectRef { kind: "device".into(), schema: None, name: name.into() }
}

/// `YYYY-MM-DD HH:MM:SS.fff` of `T0 + ms` (within January 2024).
fn stamp(ms: i64) -> String {
    let t = T0 + ms;
    let (secs, frac) = (t.div_euclid(1000), t.rem_euclid(1000));
    let day = (secs - T0 / 1000).div_euclid(86_400) + 1;
    let rem = secs.rem_euclid(86_400);
    format!("2024-01-{day:02} {:02}:{:02}:{:02}.{frac:03}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn spec(device: &str, names: &[&str]) -> LoadSpec {
    LoadSpec {
        table: obj(device),
        columns: names.iter().map(|s| s.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: 1 << 29,
    }
}

async fn round_trip(url: &str, v2: bool) {
    let mut s = open(url).await;
    let mut out = QueryOutcome::default();
    let _ = s.execute(&format!("DELETE DATABASE {DB}"), 10, &mut out).await;
    run(s.as_mut(), &format!("CREATE DATABASE {DB}")).await;
    run(s.as_mut(), &format!("CREATE ALIGNED TIMESERIES {DB}.d1(v DOUBLE, n INT64, s TEXT, b BOOLEAN, i INT32)")).await;

    let names = ["Time", "v", "n", "s", "b", "i"];
    let rows: Vec<Vec<Cell>> = (0..ROWS)
        .map(|i| {
            vec![
                Cell::DateTime(stamp(i as i64 * 7)),
                Cell::Float(if i == 1 { 1e300 } else { i as f64 / 3.0 }),
                Cell::Int(9_007_199_254_740_993 + i as i64),
                if i % 7 == 0 { Cell::Null } else { Cell::Text(format!("fila {i} ñ'")) },
                Cell::Bool(i % 2 == 0),
                Cell::Int(-(i as i64)),
            ]
        })
        .collect();
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let n = s.bulk_load(&spec("d1", &names), &[], &mut batches(rows.clone()), &|n| reports.lock().unwrap().push(n)).await.expect("load");
    let secs = t.elapsed().as_secs_f64();
    println!("iotdb load: {n} rows in {secs:.2}s = {:.0} rows/s", n as f64 / secs);
    assert_eq!(n as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 9 && reports.windows(2).all(|w| w[0] < w[1]) && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    let t = Instant::now();
    let got = read(s.as_mut(), ReadSpec { table: obj("d1"), columns: Some(names.iter().map(|s| s.to_string()).collect()), filter: None }).await;
    let secs = t.elapsed().as_secs_f64();
    println!("iotdb read: {} rows in {secs:.2}s = {:.0} rows/s", got.rows.len(), got.rows.len() as f64 / secs);
    assert_eq!(got.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(), ["TIMESTAMP", "DOUBLE", "INT64", "TEXT", "BOOLEAN", "INT32"]);
    assert_eq!(got.rows.len(), ROWS);
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r, &rows[i], "row {i}");
    }

    // Filtered, all columns.
    let f = read(s.as_mut(), ReadSpec { table: obj("d1"), columns: None, filter: Some("b = true".into()) }).await;
    assert_eq!(f.rows.len(), ROWS / 2);
    assert_eq!(f.columns[0].name, "Time");

    // Into a device that doesn't exist yet: series created with the source's types.
    let n = s.bulk_load(&spec("d2", &names), &got.columns, &mut batches(got.rows), &|_| {}).await.expect("copy");
    assert_eq!(n as usize, ROWS);
    let back = read(s.as_mut(), ReadSpec { table: obj("d2"), columns: Some(names.iter().map(|s| s.to_string()).collect()), filter: None }).await;
    assert_eq!(back.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(), ["TIMESTAMP", "DOUBLE", "INT64", "TEXT", "BOOLEAN", "INT32"]);
    assert_eq!(back.rows, rows);

    if v2 {
        run(s.as_mut(), &format!("CREATE ALIGNED TIMESERIES {DB}.d3(g STRING, h BLOB, dt DATE, j TIMESTAMP)")).await;
        let names = ["Time", "g", "h", "dt", "j"];
        let rows: Vec<Vec<Cell>> = (0..1_000)
            .map(|i| {
                vec![
                    Cell::DateTime(stamp(i)),
                    Cell::Text(format!("g{i}")),
                    // Every tenth BLOB isn't UTF-8: that row goes by SQL.
                    if i % 10 == 0 { Cell::Bytes(vec![0xCA, 0xFE, i as u8]) } else { Cell::Bytes(format!("b{i}").into_bytes()) },
                    Cell::Date(format!("2024-02-{:02}", i % 28 + 1)),
                    Cell::DateTime(stamp(i * 1000)),
                ]
            })
            .collect();
        s.bulk_load(&spec("d3", &names), &[], &mut batches(rows.clone()), &|_| {}).await.expect("v2 types");
        let got = read(s.as_mut(), ReadSpec { table: obj("d3"), columns: Some(names.iter().map(|s| s.to_string()).collect()), filter: None }).await;
        assert_eq!(got.rows.len(), rows.len());
        for (i, (a, b)) in got.rows.iter().zip(&rows).enumerate() {
            if i % 10 == 0 {
                // REST reads a BLOB as UTF-8 text: only the rest compares.
                assert_eq!((&a[..2], &a[3..]), (&b[..2], &b[3..]), "row {i}");
            } else {
                assert_eq!(a, b, "row {i}");
            }
        }
        // The non-UTF-8 BLOBs were written whole (checked in SQL).
        let mut out = QueryOutcome::default();
        s.execute(&format!("SELECT count(h) FROM {DB}.d3 WHERE h = X'CAFE00'"), 10, &mut out).await.unwrap();
        assert_eq!(out.results[0].rows[0].last(), Some(&serde_json::json!(1)));
    }

    run(s.as_mut(), &format!("DELETE DATABASE {DB}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb() {
    let Ok(url) = std::env::var("DBINE_TEST_IOTDB_URL") else { return };
    round_trip(&url, false).await;
    if let Ok(url) = std::env::var("DBINE_TEST_IOTDB2_URL") {
        round_trip(&url, true).await;
    }
}

// Regression tests (ignored; `DBINE_TEST_IOTDB_URL`), each in its own database.

/// A fresh database `root.<name>` and a session on it.
async fn fresh(url: &str, name: &str) -> Box<dyn Session> {
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    let cfg = ConnectionConfig { driver: "iotdb".into(), host: url.into(), username: Some("root".into()), password: Some("root".into()), ..Default::default() };
    let db = format!("root.{name}");
    let mut s = d.connect(&cfg, Some(&db)).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute(&format!("DELETE DATABASE {db}"), 10, &mut out).await;
    run(s.as_mut(), &format!("CREATE DATABASE {db}")).await;
    s
}

async fn drop_db(s: &mut dyn Session, name: &str) {
    run(s, &format!("DELETE DATABASE root.{name}")).await;
}

/// Rows stored in `path` (a series), counted by the server.
async fn count(s: &mut dyn Session, device: &str, series: &str) -> u64 {
    let mut out = QueryOutcome::default();
    s.execute(&format!("SELECT count({series}) FROM {device}"), 10, &mut out).await.unwrap();
    out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.last()).and_then(|v| v.as_u64()).unwrap_or(0)
}

async fn series_type(s: &mut dyn Session, path: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    s.execute(&format!("SHOW TIMESERIES {path}"), 10, &mut out).await.unwrap();
    let r = &out.results[0];
    let i = r.columns.iter().position(|c| c.name == "DataType")?;
    r.rows.first()?.get(i)?.as_str().map(str::to_string)
}

fn col(name: &str, ty: &str) -> TransferColumn {
    TransferColumn { name: name.into(), type_name: ty.into(), nullable: true }
}

fn url() -> Option<String> {
    std::env::var("DBINE_TEST_IOTDB_URL").ok()
}

/// An unknown column is an error, and the columns are matched by name
/// (not by position in the answer).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_columns_by_name() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_cols").await;
    run(s.as_mut(), "INSERT INTO root.rv_cols.c(timestamp, a, b) VALUES (1, 1.5, 'uno')").await;
    let spec = |c: &[&str]| ReadSpec { table: obj("c"), columns: Some(c.iter().map(|s| s.to_string()).collect()), filter: None };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let err = s.read_batches(&spec(&["Time", "b", "nosuch", "a"]), sink).await.unwrap_err();
    assert!(err.to_string().contains("nosuch"), "{err}");
    let got = read(s.as_mut(), spec(&["Time", "b", "a"])).await;
    assert_eq!(got.rows, vec![vec![Cell::DateTime("1970-01-01 00:00:00.001".into()), Cell::Text("uno".into()), Cell::Float(1.5)]]);
    let got = read(s.as_mut(), spec(&["a", "Time"])).await;
    assert_eq!(got.rows, vec![vec![Cell::Float(1.5), Cell::DateTime("1970-01-01 00:00:00.001".into())]]);
    drop_db(s.as_mut(), "rv_cols").await;
}

/// NaN and ±Infinity are stored, not turned into NULL.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_non_finite() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_nan").await;
    let rows = vec![
        vec![Cell::DateTime(stamp(0)), Cell::Float(f64::NAN)],
        vec![Cell::DateTime(stamp(1)), Cell::Float(f64::INFINITY)],
        vec![Cell::DateTime(stamp(2)), Cell::Float(f64::NEG_INFINITY)],
        vec![Cell::DateTime(stamp(3)), Cell::Float(0.1)],
    ];
    let n = s.bulk_load(&spec("f", &["Time", "v"]), &[col("Time", "TIMESTAMP"), col("v", "DOUBLE")], &mut batches(rows), &|_| {}).await.unwrap();
    assert_eq!(n, 4);
    let got = read(s.as_mut(), ReadSpec { table: obj("f"), columns: Some(vec!["v".into()]), filter: None }).await;
    let v: Vec<f64> = got.rows.iter().map(|r| if let Cell::Float(f) = r[0] { f } else { panic!("{r:?}") }).collect();
    assert!(v[0].is_nan());
    assert_eq!(&v[1..], &[f64::INFINITY, f64::NEG_INFINITY, 0.1]);
    drop_db(s.as_mut(), "rv_nan").await;
}

/// A new series takes the source column's type even when its first values
/// are NULL (or all of them are).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_types_from_source() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_nf").await;
    let rows: Vec<Vec<Cell>> =
        (0..12_000).map(|i| vec![Cell::DateTime(stamp(i)), if i < 10_000 { Cell::Null } else { Cell::Float(i as f64) }, Cell::Null]).collect();
    let cols = [col("Time", "TIMESTAMP"), col("v", "DOUBLE"), col("n", "INT64")];
    let n = s.bulk_load(&spec("nf", &["Time", "v", "n"]), &cols, &mut batches(rows), &|_| {}).await.expect("load");
    assert_eq!(n, 12_000);
    assert_eq!(count(s.as_mut(), "root.rv_nf.nf", "v").await, 2_000);
    assert_eq!(series_type(s.as_mut(), "root.rv_nf.nf.v").await.as_deref(), Some("DOUBLE"));
    assert_eq!(series_type(s.as_mut(), "root.rv_nf.nf.n").await.as_deref(), Some("INT64"));
    // Without a source type: the first value decides, earlier NULLs don't.
    let rows: Vec<Vec<Cell>> =
        (0..12_000).map(|i| vec![Cell::DateTime(stamp(i)), if i < 10_000 { Cell::Null } else { Cell::Float(i as f64) }]).collect();
    s.bulk_load(&spec("nf2", &["Time", "v"]), &[], &mut batches(rows), &|_| {}).await.expect("load without types");
    assert_eq!(series_type(s.as_mut(), "root.rv_nf.nf2.v").await.as_deref(), Some("DOUBLE"));
    drop_db(s.as_mut(), "rv_nf").await;
}

/// A failed or cancelled load leaves nothing to commit after it returns,
/// and on an error the progress is what the server holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_no_late_commits() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_late").await;
    let text = "x".repeat(2_000);
    let rows = |bad: bool| -> Vec<Vec<Cell>> {
        let mut r: Vec<Vec<Cell>> = (0..40_000).map(|i| vec![Cell::Int(i + 1), Cell::Text(text.clone())]).collect();
        if bad {
            r.push(vec![Cell::Text("no es una fecha".into()), Cell::Null]);
        }
        r
    };
    let last = Mutex::new(0u64);
    let err = s
        .bulk_load(&spec("e", &["Time", "v"]), &[col("Time", "INT64"), col("v", "TEXT")], &mut batches(rows(true)), &|n| *last.lock().unwrap() = n)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no es una fecha"), "{err}");
    let now = count(s.as_mut(), "root.rv_late.e", "v").await;
    println!("failed load: {now} rows stored, progress {}", last.lock().unwrap());
    assert_eq!(now, *last.lock().unwrap(), "progress = committed rows");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(count(s.as_mut(), "root.rv_late.e", "v").await, now, "no commits after returning");

    // Cancelled: the load's future dropped mid-way.
    let (lspec, cols, mut src) = (spec("c", &["Time", "v"]), [col("Time", "INT64"), col("v", "TEXT")], batches(rows(false)));
    let cut_at = Mutex::new(0u64);
    let cut = tokio::time::timeout(std::time::Duration::from_millis(300), s.bulk_load(&lspec, &cols, &mut src, &|n| *cut_at.lock().unwrap() = n)).await;
    assert!(cut.is_err(), "the load should still be running at 300 ms");
    let now = count(s.as_mut(), "root.rv_late.c", "v").await;
    println!("cancelled load: {now} rows stored, progress {}", cut_at.lock().unwrap());
    assert_eq!(now, *cut_at.lock().unwrap(), "progress after a cancel = committed rows");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(count(s.as_mut(), "root.rv_late.c", "v").await, now, "no commits after the cancel");
    drop_db(s.as_mut(), "rv_late").await;
}

/// Wide rows: pages and tablets bounded by bytes, values whole.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_wide_rows() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_wide").await;
    let rows: Vec<Vec<Cell>> = (0..400).map(|i| vec![Cell::Int(i + 1), Cell::Text(format!("{i:05}{}", "y".repeat(64 * 1024)))]).collect();
    let reports = Mutex::new(Vec::new());
    let n = s
        .bulk_load(&spec("w", &["Time", "v"]), &[col("Time", "INT64"), col("v", "TEXT")], &mut batches(rows.clone()), &|n| reports.lock().unwrap().push(n))
        .await
        .unwrap();
    assert_eq!(n, 400);
    assert_eq!(*reports.lock().unwrap().last().unwrap(), 400);
    let got = read(s.as_mut(), ReadSpec { table: obj("w"), columns: Some(vec!["v".into()]), filter: None }).await;
    assert_eq!(got.rows.len(), 400);
    for (a, b) in got.rows.iter().zip(&rows) {
        assert_eq!(a[0], b[1]);
    }
    drop_db(s.as_mut(), "rv_wide").await;
}

/// Device names are nodes, never SQL or wildcards.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_device_names() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_names").await;
    let cols = [col("Time", "INT64"), col("a", "INT64")];
    for (name, v) in [("order-items", 1), ("d*", 2), ("d1", 3), ("d2", 4)] {
        let n = s.bulk_load(&spec(name, &["Time", "a"]), &cols, &mut batches(vec![vec![Cell::Int(1), Cell::Int(v)]]), &|_| {}).await.expect(name);
        assert_eq!(n, 1);
    }
    for (name, v) in [("order-items", 1), ("d*", 2)] {
        let got = read(s.as_mut(), ReadSpec { table: obj(name), columns: Some(vec!["a".into()]), filter: None }).await;
        assert_eq!(got.rows, vec![vec![Cell::Int(v)]], "{name}");
    }
    drop_db(s.as_mut(), "rv_names").await;
}

/// A timestamp finer than the server's precision is an error, not two
/// rows collapsed into one.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_finer_timestamps() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_prec").await;
    let rows = vec![
        vec![Cell::DateTime("2024-01-01 00:00:00.000001".into()), Cell::Int(1)],
        vec![Cell::DateTime("2024-01-01 00:00:00.000002".into()), Cell::Int(2)],
    ];
    let err = s.bulk_load(&spec("p", &["Time", "a"]), &[], &mut batches(rows), &|_| {}).await.unwrap_err();
    assert!(err.to_string().contains("precisión"), "{err}");
    drop_db(s.as_mut(), "rv_prec").await;
}

/// Series names that aren't plain (`order-id`, `first name`, `a.b`) load,
/// read back, and copy from IoTDB to IoTDB.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_measurement_names() {
    let Some(url) = url() else { return };
    let mut s = fresh(&url, "rv_meas").await;
    let names = ["Time", "order-id", "first name", "a.b", "plain"];
    let cols = [col("Time", "TIMESTAMP"), col("order-id", "bigint"), col("first name", "text"), col("a.b", "double"), col("plain", "int")];
    let rows: Vec<Vec<Cell>> =
        (0..300).map(|i| vec![Cell::DateTime(stamp(i)), Cell::Int(i), Cell::Text(format!("n{i}")), Cell::Float(i as f64 / 4.0), Cell::Int(-i)]).collect();
    let n = s.bulk_load(&spec("m", &names), &cols, &mut batches(rows.clone()), &|_| {}).await.expect("load");
    assert_eq!(n, 300);
    let spec_read = |d: &str| ReadSpec { table: obj(d), columns: Some(names.iter().map(|s| s.to_string()).collect()), filter: None };
    let got = read(s.as_mut(), spec_read("m")).await;
    assert_eq!(got.rows, rows);
    // IoTDB to IoTDB: the names read back load again.
    let n = s.bulk_load(&spec("m2", &names), &got.columns, &mut batches(got.rows), &|_| {}).await.expect("copy");
    assert_eq!(n, 300);
    assert_eq!(read(s.as_mut(), spec_read("m2")).await.rows, rows);
    drop_db(s.as_mut(), "rv_meas").await;
}

/// Source columns typed timestamp, date and bytea: on a server without
/// those types (before 1.3.3) they load as TEXT; on a newer one with the
/// type itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_new_types_by_version() {
    let Some(url) = url() else { return };
    for url in std::iter::once(url).chain(std::env::var("DBINE_TEST_IOTDB2_URL").ok()) {
        let mut s = fresh(&url, "rv_ver").await;
        let mut out = QueryOutcome::default();
        s.execute("SHOW VERSION", 10, &mut out).await.unwrap();
        let version = out.results[0].rows[0][0].as_str().unwrap_or_default().to_string();
        let modern = !version.starts_with("1.3.2") && !version.starts_with("1.2") && !version.starts_with("1.1") && !version.starts_with("1.0");
        let names = ["Time", "ts", "d", "b"];
        let cols = [col("Time", "timestamp"), col("ts", "timestamp with time zone"), col("d", "date"), col("b", "bytea")];
        let rows: Vec<Vec<Cell>> = (0..50)
            .map(|i| vec![Cell::DateTime(stamp(i)), Cell::DateTime(stamp(i * 1000)), Cell::Date(format!("2024-03-{:02}", i % 28 + 1)), Cell::Bytes(format!("b{i}").into_bytes())])
            .collect();
        let n = s.bulk_load(&spec("t", &names), &cols, &mut batches(rows.clone()), &|_| {}).await.unwrap_or_else(|e| panic!("{version}: {e}"));
        assert_eq!(n, 50);
        for (series, ty) in [("ts", "TIMESTAMP"), ("d", "DATE"), ("b", "BLOB")] {
            let got = series_type(s.as_mut(), &format!("root.rv_ver.t.{series}")).await;
            assert_eq!(got.as_deref(), Some(if modern { ty } else { "TEXT" }), "{version} {series}");
        }
        let got = read(s.as_mut(), ReadSpec { table: obj("t"), columns: Some(names.iter().map(|s| s.to_string()).collect()), filter: None }).await;
        if modern {
            assert_eq!(got.rows, rows, "{version}");
        } else {
            let r = &got.rows[3];
            assert_eq!(r[1], Cell::Text(stamp(3000)));
            assert_eq!(r[2], Cell::Text("2024-03-04".into()));
            assert_eq!(r[3], Cell::Text("0x6233".into()));
        }
        println!("{version}: modern types = {modern}");
        drop_db(s.as_mut(), "rv_ver").await;
    }
}

/// Round 21: a source name wrapped in backquotes (or carrying SQL) is a
/// literal series name, in the tablet, in the SQL `INSERT` (a non-UTF-8
/// BLOB) and in the read; an Oracle-style DATE holding a time of day loads
/// as TIMESTAMP, and a time into an existing DATE series is an error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_iotdb_review_round21() {
    let Some(url) = url() else { return };
    for url in std::iter::once(url).chain(std::env::var("DBINE_TEST_IOTDB2_URL").ok()) {
        let mut s = fresh(&url, "rv21").await;
        let mut out = QueryOutcome::default();
        s.execute("SHOW VERSION", 10, &mut out).await.unwrap();
        let version = out.results[0].rows[0][0].as_str().unwrap_or_default().to_string();
        let modern = !version.starts_with("1.3.2") && !version.starts_with("1.2") && !version.starts_with("1.1") && !version.starts_with("1.0");

        // Names.
        let evil = "`a`) VALUES(1,2) --";
        let names = ["Time", "`x`", evil, "b"];
        let cols = [col("Time", "timestamp"), col("`x`", "int"), col(evil, "text"), col("b", "bytea")];
        let rows: Vec<Vec<Cell>> = (0..5)
            .map(|i| vec![Cell::DateTime(stamp(i)), Cell::Int(i), Cell::Text(format!("t{i}")), Cell::Bytes(if i == 2 { vec![0xff, 0xfe] } else { b"ok".to_vec() })])
            .collect();
        let r = s.bulk_load(&spec("n", &names), &cols, &mut batches(rows.clone()), &|_| {}).await;
        let mut out = QueryOutcome::default();
        s.execute("SHOW TIMESERIES root.rv21.n.*", 100, &mut out).await.unwrap();
        let series: Vec<String> = out.results[0].rows.iter().map(|r| r[0].as_str().unwrap_or_default().to_string()).collect();
        println!("{version}: load {r:?}; series {series:?}");
        assert!(!series.iter().any(|s| s == "root.rv21.n.x" || s == "root.rv21.n.a"), "{version}: {series:?}");
        assert_eq!(r.expect("load names"), 5, "{version}");
        let got = read(s.as_mut(), ReadSpec { table: obj("n"), columns: Some(names[..3].iter().map(|s| s.to_string()).collect()), filter: None }).await;
        let want: Vec<Vec<Cell>> = rows.iter().map(|r| r[..3].to_vec()).collect();
        assert_eq!(got.rows, want, "{version}");

        // Oracle's DATE: a date-time.
        let cols = [col("Time", "timestamp"), col("d", "DATE")];
        let rows = vec![vec![Cell::DateTime(stamp(0)), Cell::DateTime("2024-01-31 13:45:07".into())]];
        s.bulk_load(&spec("o", &["Time", "d"]), &cols, &mut batches(rows), &|_| {}).await.unwrap_or_else(|e| panic!("{version}: {e}"));
        let got = read(s.as_mut(), ReadSpec { table: obj("o"), columns: Some(vec!["Time".into(), "d".into()]), filter: None }).await;
        let d = &got.rows[0][1];
        assert!(matches!(d, Cell::DateTime(t) | Cell::Text(t) if t.starts_with("2024-01-31 13:45:07")), "{version}: {d:?}");
        if modern {
            assert_eq!(series_type(s.as_mut(), "root.rv21.o.d").await.as_deref(), Some("TIMESTAMP"));
            // Into an existing DATE series, a time of day is an error.
            run(s.as_mut(), "CREATE TIMESERIES root.rv21.e.d WITH DATATYPE=DATE").await;
            let rows = vec![vec![Cell::DateTime(stamp(0)), Cell::DateTime("2024-01-31 13:45:07".into())]];
            let e = s.bulk_load(&spec("e", &["Time", "d"]), &cols, &mut batches(rows), &|_| {}).await.unwrap_err();
            assert!(e.to_string().contains("hora"), "{e}");
            assert_eq!(count(s.as_mut(), "root.rv21.e", "d").await, 0);
        }
        drop_db(s.as_mut(), "rv21").await;
    }
}

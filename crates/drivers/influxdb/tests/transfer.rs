//! Bulk transfer against real servers (ignored by default; containers and
//! variables as in `integration.rs`, with those ports as defaults):
//!
//! ```sh
//! cargo test -p dbine-driver-influxdb -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, Session};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const DB: &str = "dbine_transfer";
const BASE_NS: i64 = 1_706_708_700_000_000_000;

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

fn time_text(ns: i64) -> String {
    chrono::DateTime::from_timestamp_nanos(ns).format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string()
}

/// time, host (tag), region (tag), v, n, s, ok.
fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::DateTimeTz(time_text(BASE_NS + i as i64 * 1_001)),
        Cell::Text(format!("host {}", i % 10)),
        Cell::Text(if i.is_multiple_of(2) { "eu".into() } else { "us,west".into() }),
        Cell::Float(i as f64 * 0.5 + 0.25),
        Cell::Int(i as i64 - 50_000),
        Cell::Text(format!("say \"{i}\"")),
        Cell::Bool(i.is_multiple_of(3)),
    ]
}

const COLS: [(&str, &str); 7] = [("time", "time"), ("host", "tag"), ("region", "tag"), ("v", ""), ("n", ""), ("s", ""), ("ok", "")];

async fn exercise(id: &str, s: &mut Box<dyn Session>, time_col: &str) {
    let m = ObjectRef { kind: kinds::MEASUREMENT.into(), schema: None, name: "xfer".into() };
    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let columns: Vec<TransferColumn> =
        COLS.iter().map(|(n, t)| TransferColumn { name: n.to_string(), type_name: t.to_string(), nullable: true }).collect();
    let spec = LoadSpec {
        table: m.clone(),
        columns: COLS.iter().map(|(n, _)| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 20_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &columns, &mut Batches(batches.into_iter()), &progress).await.expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    println!("{id}: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table: m.clone(), columns: None, filter: None }, sink.clone()).await.expect("read");
    let secs = start.elapsed().as_secs_f64();
    println!("{id}: read_batches {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read as usize, ROWS);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    let pos: Vec<usize> = COLS
        .iter()
        .map(|(n, _)| {
            let n = if *n == "time" { time_col } else { n };
            names.iter().position(|c| *c == n).unwrap_or_else(|| panic!("{n} not in {names:?}"))
        })
        .collect();
    let by_time: HashMap<String, Vec<Cell>> = got
        .rows
        .into_iter()
        .map(|r| {
            let r: Vec<Cell> = pos.iter().map(|p| r[*p].clone()).collect();
            let Cell::DateTimeTz(t) = &r[0] else { panic!("time {:?}", r[0]) };
            (t.clone(), r)
        })
        .collect();
    assert_eq!(by_time.len(), ROWS);
    for i in 0..ROWS {
        let want = row(i);
        let Cell::DateTimeTz(t) = &want[0] else { unreachable!() };
        assert_eq!(by_time[t], want, "row {i}");
    }
}

async fn open(id: &str, cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.supports_bulk_load());
    d.connect(cfg, db).await.expect("connect")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_influxdb1() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").unwrap_or_else(|_| "http://localhost:25404".into());
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url, ..Default::default() };
    let mut admin = open("influxdb1", &cfg, None).await;
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let mut s = open("influxdb1", &cfg, Some(DB)).await;
    exercise("influxdb1", &mut s, "time").await;
    admin.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_influxdb2() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").unwrap_or_else(|_| "http://localhost:25403".into());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url, ..Default::default() };
    cfg.options.insert("org".into(), std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into()));
    cfg.options.insert("token".into(), std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into()));
    let mut admin = open("influxdb", &cfg, None).await;
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let mut s = open("influxdb", &cfg, Some(DB)).await;
    exercise("influxdb", &mut s, "_time").await;
    admin.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_influxdb3() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").unwrap_or_else(|_| "http://localhost:25409".into());
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() };
    let mut admin = open("influxdb3", &cfg, None).await;
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let mut s = open("influxdb3", &cfg, Some(DB)).await;
    exercise("influxdb3", &mut s, "time").await;
    let _ = admin.drop_database(DB).await;
}

// ------------------------------------------------------------ edge cases

const EDGES: &str = "dbine_transfer_edges";

/// Batches of `rows`, then (with a flag to raise) nothing ever again.
struct Feed(Vec<RowBatch>, Option<Arc<AtomicBool>>);

#[async_trait]
impl BatchSource for Feed {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.0.is_empty() {
            if let Some(hung) = &self.1 {
                hung.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            return None;
        }
        Some(self.0.remove(0))
    }
}

fn mref(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::MEASUREMENT.into(), schema: None, name: name.into() }
}

fn load_spec(m: &str, cols: &[(&str, &str)], commit_rows: u64) -> (LoadSpec, Vec<TransferColumn>) {
    let spec = LoadSpec {
        table: mref(m),
        columns: cols.iter().map(|(n, _)| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let columns = cols.iter().map(|(n, t)| TransferColumn { name: n.to_string(), type_name: t.to_string(), nullable: true }).collect();
    (spec, columns)
}

async fn put(s: &mut Box<dyn Session>, m: &str, cols: &[(&str, &str)], batches: Vec<RowBatch>) -> dbine_driver::Result<u64> {
    let (spec, columns) = load_spec(m, cols, 100_000);
    s.bulk_load(&spec, &columns, &mut Feed(batches, None), &|_| {}).await
}

fn batch(rows: Vec<Vec<Cell>>) -> RowBatch {
    RowBatch { rows, bytes: 0 }
}

/// A measurement's rows by column name, sorted by time.
async fn get(s: &mut Box<dyn Session>, m: &str, columns: Option<Vec<String>>) -> dbine_driver::Result<Vec<HashMap<String, Cell>>> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: mref(m), columns, filter: None }, sink.clone()).await?;
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let mut rows: Vec<HashMap<String, Cell>> =
        got.rows.into_iter().map(|r| got.columns.iter().map(|c| c.name.clone()).zip(r).collect()).collect();
    rows.sort_by_key(|r| format!("{:?}", r.get("time").or(r.get("_time"))));
    Ok(rows)
}

async fn count(s: &mut Box<dyn Session>, m: &str) -> usize {
    get(s, m, None).await.map(|r| r.len()).unwrap_or(0)
}

fn ns(i: i64) -> Cell {
    Cell::Int(BASE_NS + i)
}

async fn edges(id: &str, s: &mut Box<dyn Session>, time_col: &str) {
    let tv = [(time_col, "time"), ("v", "")];
    // 1. A failed load: nothing commits after it returns.
    let mut rows: Vec<Vec<Cell>> = (0..2_000).map(|i| vec![ns(i), Cell::Float(i as f64)]).collect();
    let mut bad = rows.split_off(1_000);
    bad[500][0] = Cell::Text("not a time".into());
    let (spec, columns) = load_spec("late", &tv, 1_000);
    let r = s.bulk_load(&spec, &columns, &mut Feed(vec![batch(rows), batch(bad)], None), &|_| {}).await;
    assert!(r.is_err(), "{id}: {r:?}");
    let at_return = count(s, "late").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(count(s, "late").await, at_return, "{id}: late commit after a failed load");
    //    Cancelled (dropped) with a request out: dropped once it asks for
    //    more rows, after sending its one window (2,000 points).
    let rows: Vec<Vec<Cell>> = (0..2_000).map(|i| vec![ns(i), Cell::Float(i as f64)]).collect();
    let (spec, columns) = load_spec("late_cancel", &tv, 2_000);
    let hung = Arc::new(AtomicBool::new(false));
    let mut feed = Feed(vec![batch(rows)], Some(hung.clone()));
    let fut = s.bulk_load(&spec, &columns, &mut feed, &|_| {});
    let asked = async {
        while !hung.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    tokio::select! {
        r = fut => panic!("{id}: the load ended: {r:?}"),
        r = tokio::time::timeout(std::time::Duration::from_secs(30), asked) => r.expect("the load never asked for more rows"),
    }
    let at_return = count(s, "late_cancel").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(count(s, "late_cancel").await, at_return, "{id}: late commit after a cancelled load");
    assert_eq!(at_return, 2_000, "{id}");

    // 2. Big rows: requests split by bytes (3.x: 10 MiB, 1.x: 25 MB); and
    //    read back after small ones (1.x: one ~12 MB chunk, streamed).
    let wide = "w".repeat(6_000);
    let mut rows: Vec<Vec<Cell>> = (0..20).map(|i| vec![ns(i), Cell::Text(format!("s{i}"))]).collect();
    rows.extend((20..2_020).map(|i| vec![ns(i), Cell::Text(format!("{wide}{i}"))]));
    let batches = rows.chunks(500).map(|c| batch(c.to_vec())).collect();
    assert_eq!(put(s, "wide", &tv, batches).await.expect("wide"), 2_020, "{id}");
    let got = get(s, "wide", None).await.expect("read wide");
    assert_eq!(got.len(), 2_020, "{id}");
    for (g, r) in got.iter().zip(&rows) {
        assert_eq!(g["v"], r[1], "{id}");
    }

    // 3. Doubles exactly.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let floats: Vec<f64> = std::iter::from_fn(|| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        Some(f64::from_bits(x))
    })
    .filter(|f| f.is_finite())
    .take(3_000)
    .chain([-0.0, 0.0, 1.0, -1.0])
    .collect();
    let rows = floats.iter().enumerate().map(|(i, f)| vec![ns(i as i64), Cell::Float(*f)]).collect();
    assert_eq!(put(s, "floats", &tv, vec![batch(rows)]).await.expect("floats"), 3_004);
    let got = get(s, "floats", None).await.expect("read floats");
    assert_eq!(got.len(), 3_004, "{id}");
    // By bits: -0.0 == 0.0.
    let bad: Vec<_> = got.iter().zip(&floats).filter(|(r, f)| !matches!(r["v"], Cell::Float(g) if g.to_bits() == f.to_bits())).collect();
    assert!(bad.is_empty(), "{id}: {} of 3004 floats differ, e.g. {:?}", bad.len(), bad.first());

    // 4. Before 1970 and in the future.
    let rows = vec![vec![Cell::Int(-1_000_000_000), Cell::Int(1)], vec![Cell::Int(1_000), Cell::Int(2)], vec![Cell::Text("2100-01-01T00:00:00Z".into()), Cell::Int(3)]];
    put(s, "times", &tv, vec![batch(rows)]).await.expect("times");
    assert_eq!(count(s, "times").await, 3, "{id}");

    // 5. Backslashes: kept as they are; what can't be written fails.
    let cols = [(time_col, "time"), ("t", "tag"), ("f\\k", "")];
    put(s, "esc", &cols, vec![batch(vec![vec![ns(0), Cell::Text("a\\b".into()), Cell::Int(1)]])]).await.expect("esc");
    let got = get(s, "esc", None).await.unwrap();
    assert_eq!((&got[0]["t"], &got[0]["f\\k"]), (&Cell::Text("a\\b".into()), &Cell::Int(1)), "{id}");
    for v in ["end\\", "l1\nl2"] {
        let r = put(s, "esc", &cols, vec![batch(vec![vec![ns(1), Cell::Text(v.into()), Cell::Int(1)]])]).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{id}: {v:?} {r:?}");
    }

    // 6. Fields named like Flux's columns are data.
    let cols = [(time_col, "time"), ("result", ""), ("table", ""), ("_value", ""), ("v", "")];
    let row = vec![ns(0), Cell::Int(1), Cell::Int(2), Cell::Int(3), Cell::Int(4)];
    assert_eq!(put(s, "names", &cols, vec![batch(vec![row])]).await.expect("names"), 1);
    let got = get(s, "names", None).await.unwrap();
    for (n, v) in [("result", 1), ("table", 2), ("_value", 3), ("v", 4)] {
        assert_eq!(got[0].get(n), Some(&Cell::Int(v)), "{id}: {n} in {:?}", got[0]);
    }

    // 7. An unknown column is an error.
    let r = get(s, "names", Some(vec![time_col.into(), "nope".into()])).await;
    assert!(r.is_err(), "{id}: {r:?}");

    // 8 and 11. Strings as they are: line breaks, `\r`, empty; exact decimals.
    let cols = [(time_col, "time"), ("s", ""), ("d", "")];
    let texts = ["x\ny", "p\rq", "", "a\r\nb"];
    let rows = texts.iter().enumerate().map(|(i, t)| vec![ns(i as i64), Cell::Text(t.to_string()), Cell::Decimal("12345678901234567890.123456789".into())]).collect();
    put(s, "strings", &cols, vec![batch(rows)]).await.expect("strings");
    let got = get(s, "strings", None).await.unwrap();
    for (r, t) in got.iter().zip(texts) {
        assert_eq!(r["s"], Cell::Text(t.into()), "{id}");
        assert_eq!(r["d"], Cell::Text("12345678901234567890.123456789".into()), "{id}");
    }

    // 9. A row with no field values fails, and isn't counted.
    let mut rows: Vec<Vec<Cell>> = (0..10).map(|i| vec![ns(i), Cell::Null]).collect();
    rows.extend((10..20).map(|i| vec![ns(i), Cell::Int(i)]));
    let (spec, columns) = load_spec("fieldless", &tv, 10);
    let r = s.bulk_load(&spec, &columns, &mut Feed(vec![batch(rows)], None), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{id}: {r:?}");

    // 10. Tags named error and reference.
    let cols = [(time_col, "time"), ("error", "tag"), ("reference", "tag"), ("v", "")];
    put(s, "et", &cols, vec![batch(vec![vec![ns(0), Cell::Text("none".into()), Cell::Text("abc".into()), Cell::Int(1)]])]).await.expect("et");
    let got = get(s, "et", None).await.expect("read et");
    assert_eq!(got[0]["error"], Cell::Text("none".into()), "{id}");
    println!("{id}: edge cases ok");
}

async fn edges_db(id: &str, cfg: &ConnectionConfig, time_col: &str) {
    let mut admin = open(id, cfg, None).await;
    let _ = admin.drop_database(EDGES).await;
    admin.create_database(EDGES).await.unwrap();
    let mut s = open(id, cfg, Some(EDGES)).await;
    edges(id, &mut s, time_col).await;
    let _ = admin.drop_database(EDGES).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edges_influxdb1() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").unwrap_or_else(|_| "http://localhost:25404".into());
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url, ..Default::default() };
    edges_db("influxdb1", &cfg, "time").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edges_influxdb2() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").unwrap_or_else(|_| "http://localhost:25403".into());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url, ..Default::default() };
    cfg.options.insert("org".into(), std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into()));
    cfg.options.insert("token".into(), std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into()));
    edges_db("influxdb", &cfg, "_time").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edges_influxdb3() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").unwrap_or_else(|_| "http://localhost:25409".into());
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() };
    edges_db("influxdb3", &cfg, "time").await;
}

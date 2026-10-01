//! Bulk load and typed read against a real Flight SQL server (GizmoSQL, see
//! `integration.rs` for the container):
//! `DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!  cargo test -p dbine-driver-flightsql --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: usize = 200_000;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?;
    let rest = url.split("://").nth(1)?;
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into())),
        password: Some(std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into())),
        ..Default::default()
    })
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
    let n = i as i64;
    vec![
        Cell::Int(n),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Int(n * 1_000_003) },
        Cell::Decimal(format!("{}.{:02}", n - 50_000, i % 100)),
        Cell::Float(i as f64 / 8.0),
        Cell::Text(format!("fila {i} ñ 'x'")),
        Cell::Bytes(vec![(i % 256) as u8; i % 40]),
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::Time(format!("{:02}:{:02}:{:02}.{:06}", i % 24, i % 60, i % 60, i % 1_000_000)),
        Cell::DateTime(format!("2024-01-{:02} 10:00:00.{:06}", i % 28 + 1, i % 1_000_000)),
        Cell::Bool(i.is_multiple_of(2)),
    ]
}

/// The read gives trimmed fractions (`.5` for `.500000`, none for zero).
fn norm(c: &Cell) -> Cell {
    let trim = |s: &str| -> String {
        match s.split_once('.') {
            Some((a, f)) => {
                let f = f.trim_end_matches('0');
                if f.is_empty() {
                    a.to_string()
                } else {
                    format!("{a}.{f}")
                }
            }
            None => s.to_string(),
        }
    };
    match c {
        Cell::Time(s) => Cell::Time(trim(s)),
        Cell::DateTime(s) => Cell::DateTime(trim(s)),
        // Empty binaries come back as empty binaries.
        c => c.clone(),
    }
}

async fn load_and_read(s: &mut Box<dyn Session>, table: &str, columns: &[&str], label: &str, total: usize) {
    let obj = ObjectRef { kind: "table".into(), schema: Some("dbine_xfer".into()), name: table.into() };
    let batches: Vec<RowBatch> = (0..total)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)[..columns.len()].to_vec()).collect(), bytes: 0 })
        .collect();
    let spec = LoadSpec {
        table: obj.clone(),
        columns: columns.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: (total / 4) as u64,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("flightsql {label}: loaded {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded, total as u64);
    let reports = reports.into_inner().unwrap();
    assert_eq!(reports, (1..=4).map(|k| (k * total / 4) as u64).collect::<Vec<_>>());

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: obj.clone(), columns: Some(columns.iter().map(|c| c.to_string()).collect()), filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("flightsql {label}: read {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, total as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), columns);
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    for (i, r) in rows.iter().enumerate() {
        let want: Vec<Cell> = row(i)[..columns.len()].iter().map(norm).collect();
        let r: Vec<Cell> = r.iter().map(norm).collect();
        assert_eq!(r, want, "row {i}");
    }

    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: obj, columns: Some(vec!["id".into(), "s".into()]), filter: Some("id IN (1, 2, 3)".into()) };
    assert_eq!(s.read_batches(&spec, sink).await.unwrap(), 3);
}

#[tokio::test]
#[ignore]
async fn flightsql_transfer() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_flightsql::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, Some("memory")).await.unwrap();
    run(
        &mut s,
        "DROP SCHEMA IF EXISTS dbine_xfer CASCADE; CREATE SCHEMA dbine_xfer;
         CREATE TABLE dbine_xfer.t (id BIGINT NOT NULL, n BIGINT, d DECIMAL(18,2), f DOUBLE, s VARCHAR, b BLOB, dt DATE,
                                    tm TIME, ts TIMESTAMP, ok BOOLEAN);
         CREATE TABLE dbine_xfer.p (id BIGINT NOT NULL, n BIGINT, d DECIMAL(18,2), f DOUBLE, s VARCHAR, extra VARCHAR DEFAULT 'x');",
    )
    .await;
    // Every column: bulk ingest (or the prepared fallback if the server has none).
    let all = ["id", "n", "d", "f", "s", "b", "dt", "tm", "ts", "ok"];
    load_and_read(&mut s, "t", &all, "ingest", ROWS).await;
    // Some of the columns: prepared INSERT with batches as parameter sets
    // (GizmoSQL runs them one by one: fewer rows).
    load_and_read(&mut s, "p", &all[..5], "prepared", 8_000).await;

    // A value that doesn't fit is an error, and nothing of its window stays.
    let obj = ObjectRef { kind: "table".into(), schema: Some("dbine_xfer".into()), name: "t".into() };
    let spec = LoadSpec {
        table: obj,
        columns: vec!["id".into(), "n".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let bad = RowBatch { rows: vec![vec![Cell::Int(1), Cell::Text("no es número".into())]], bytes: 0 };
    assert!(s.bulk_load(&spec, &[], &mut Batches(vec![bad].into_iter()), &|_| {}).await.is_err());
    run(&mut s, "DROP SCHEMA dbine_xfer CASCADE").await;
}

/// Batches handed out `every` apart (a slow source).
struct Slow {
    batches: std::vec::IntoIter<RowBatch>,
    every: Duration,
}

#[dbine_driver::async_trait]
impl BatchSource for Slow {
    async fn next(&mut self) -> Option<RowBatch> {
        tokio::time::sleep(self.every).await;
        self.batches.next()
    }
}

fn table(schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some(schema.into()), name: name.into() }
}

fn spec(t: ObjectRef, columns: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: t,
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// All the rows of a table (in `id` order when it has one).
async fn rows(s: &mut Box<dyn Session>, t: &ObjectRef, columns: Option<Vec<String>>) -> Vec<Vec<Cell>> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: t.clone(), columns, filter: None }, sink.clone()).await.unwrap();
    let mut r = std::mem::take(&mut sink.lock().unwrap().rows);
    r.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    r
}

async fn count(s: &mut Box<dyn Session>, t: &ObjectRef) -> usize {
    rows(s, t, Some(vec!["id".into()])).await.len()
}

fn ints(from: usize, n: usize, cols: usize) -> RowBatch {
    RowBatch { rows: (from..from + n).map(|i| (0..cols).map(|c| if c == 0 { Cell::Int(i as i64) } else { Cell::Text(format!("v{i}")) }).collect()).collect(), bytes: 0 }
}

async fn setup(s: &mut Box<dyn Session>, sql: &str) {
    run(s, &format!("DROP SCHEMA IF EXISTS rv CASCADE; CREATE SCHEMA rv; {sql}")).await;
}

/// The tests share the server's catalog (and its `rv` schema): run in
/// parallel they fail on DuckDB's catalog write conflicts, and time out
/// under each other's load. One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn connect() -> Option<(Box<dyn Session>, tokio::sync::MutexGuard<'static, ()>)> {
    let c = cfg()?;
    let serial = SERIAL.lock().await;
    Some((dbine_driver_flightsql::drivers().remove(0).connect(&c, Some("memory")).await.unwrap(), serial))
}

/// A window that fails on the client is rolled back whole (it used to end
/// its stream as a normal finish and the server kept the first batches);
/// windows already committed stay, and progress says so.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_failed_window_rolls_back() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.t (id BIGINT, s VARCHAR);").await;
    let t = table("rv", "t");
    let mut bad = ints(2000, 1000, 2);
    bad.rows[500][0] = Cell::Text("no es numero".into());
    let batches = || vec![ints(0, 1000, 2), ints(1000, 1000, 2), bad.clone()];
    for (commit, want_rows, want_progress) in [(100_000u64, 0usize, vec![]), (1000, 2000, vec![1000u64, 2000])] {
        run(&mut s, "DELETE FROM rv.t").await;
        let reports = Mutex::new(Vec::new());
        let r = s.bulk_load(&spec(t.clone(), &["id", "s"], commit), &[], &mut Batches(batches().into_iter()), &|n| reports.lock().unwrap().push(n)).await;
        assert!(matches!(&r, Err(e) if e.is_query() && e.to_string().contains("no es un entero")), "{r:?}");
        assert_eq!(count(&mut s, &t).await, want_rows, "commit {commit}");
        assert_eq!(reports.into_inner().unwrap(), want_progress);
    }
}

/// Cancelling in the middle of an ingest window rolls it back.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_cancel_ingest_window() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.t (id BIGINT, s VARCHAR);").await;
    let t = table("rv", "t");
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop();
    });
    let mut src = Slow { batches: (0..10).map(|k| ints(k * 1000, 1000, 2)).collect::<Vec<_>>().into_iter(), every: Duration::from_millis(300) };
    let reports = Mutex::new(Vec::new());
    let r = s.bulk_load(&spec(t.clone(), &["id", "s"], 100_000), &[], &mut src, &|n| reports.lock().unwrap().push(n)).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(reports.into_inner().unwrap().is_empty());
    assert_eq!(count(&mut s, &t).await, 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(count(&mut s, &t).await, 0);
}

/// Batches handed out one by one; the cancel fires when the `at`-th is
/// asked for (no timing involved), and the count of batches taken says how
/// far the load went after it.
struct CancelAt {
    batches: std::vec::IntoIter<RowBatch>,
    at: usize,
    taken: Arc<Mutex<usize>>,
    stop: Arc<dyn Fn() + Send + Sync>,
}

#[dbine_driver::async_trait]
impl BatchSource for CancelAt {
    async fn next(&mut self) -> Option<RowBatch> {
        let mut taken = self.taken.lock().unwrap();
        if *taken == self.at {
            (self.stop)();
        }
        *taken += 1;
        self.batches.next()
    }
}

/// Prepared path (a subset of the columns): a cancel waits for the call in
/// flight and rolls the window back; nothing lands afterwards. The cancel
/// comes from the source, so the check doesn't depend on the machine's load.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_cancel_prepared_window() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.p (id BIGINT, s VARCHAR, extra VARCHAR DEFAULT 'x');").await;
    let t = table("rv", "p");
    let reports = Mutex::new(Vec::new());
    let taken = Arc::new(Mutex::new(0));
    // Batches of 300 rows: two prepared calls (256 + 44) each.
    let mut src = CancelAt { batches: (0..30).map(|k| ints(k * 300, 300, 2)).collect::<Vec<_>>().into_iter(), at: 3, taken: taken.clone(), stop: s.interrupter().unwrap() };
    let started = Instant::now();
    let r = s.bulk_load(&spec(t.clone(), &["id", "s"], 100_000), &[], &mut src, &|n| reports.lock().unwrap().push(n)).await;
    let took = started.elapsed();
    let taken = *taken.lock().unwrap();
    println!("flightsql prepared cancel: returned after {took:?}, {taken} batches taken: {r:?}");
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    // It stopped at the cancel: at most the batch asked for when it came.
    assert_eq!(taken, 4, "the load went on after the cancel");
    assert_eq!(count(&mut s, &t).await, 0);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(count(&mut s, &t).await, 0, "rows landed after bulk_load returned");
    assert!(reports.into_inner().unwrap().is_empty());
}

/// Prepared path: a server error in the middle of a window leaves none of
/// its rows (GizmoSQL runs each parameter set as its own statement).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_prepared_error_rolls_back() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.p (id BIGINT PRIMARY KEY, s VARCHAR, extra VARCHAR DEFAULT 'x'); INSERT INTO rv.p (id, s) VALUES (1000, 'antes');").await;
    let t = table("rv", "p");
    let mut b = ints(0, 60, 2);
    b.rows[40][0] = Cell::Int(1000);
    let reports = Mutex::new(Vec::new());
    let r = s.bulk_load(&spec(t.clone(), &["id", "s"], 1000), &[], &mut Batches(vec![b].into_iter()), &|n| reports.lock().unwrap().push(n)).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(count(&mut s, &t).await, 1);
    assert!(reports.into_inner().unwrap().is_empty());
    // Columns in another order than the table's (also the prepared path),
    // windows of exact size.
    let reports = Mutex::new(Vec::new());
    let rows_in = RowBatch { rows: (0..600).map(|i| vec![Cell::Text(format!("v{i}")), Cell::Int(i)]).collect(), bytes: 0 };
    let n = s.bulk_load(&spec(t.clone(), &["s", "id"], 250), &[], &mut Batches(vec![rows_in].into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    assert_eq!(n, 600);
    assert_eq!(reports.into_inner().unwrap(), vec![250, 500, 600]);
    assert_eq!(count(&mut s, &t).await, 601);
}

/// Values chrono and Arrow's text can't hold: read exactly, and loaded
/// back into a copy of the table they compare equal in SQL.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_types_round_trip() {
    let Some((mut s, _serial)) = connect().await else { return };
    let ddl = "(id INT, tz TIMESTAMPTZ, h HUGEINT, t TIME, d DATE, ts TIMESTAMP, st STRUCT(a INT, b VARCHAR), l INT[], m MAP(VARCHAR, INT), dec DECIMAL(10,2))";
    setup(
        &mut s,
        &format!(
            "CREATE TABLE rv.a {ddl}; CREATE TABLE rv.b {ddl};
             INSERT INTO rv.a VALUES
               (1, '0001-01-01 00:00:00+00', -170141183460469231731687303715884105728, '24:00:00', 'infinity', 'infinity', {{'a': 1, 'b': 'x'}}, [1, NULL, 3], MAP {{'k': 1}}, -1.5),
               (2, '2024-06-01 12:00:00.123456-03', 170141183460469231731687303715884105727, '00:00:00', '-infinity', '-infinity', NULL, [], MAP {{}}, 0),
               (3, NULL, NULL, '13:45:00.5', '0044-03-15 (BC)', '0044-03-15 (BC) 12:00:00', {{'a': NULL, 'b': NULL}}, NULL, NULL, NULL),
               (4, NULL, 0, NULL, '12000-01-01', '12000-01-01 00:00:00', NULL, NULL, NULL, NULL);"
        ),
    )
    .await;
    let a = rows(&mut s, &table("rv", "a"), None).await;
    let tz: Vec<Cell> = a.iter().map(|r| r[1].clone()).collect();
    assert_eq!(tz[..2], [Cell::DateTimeTz("0001-01-01 00:00:00+00:00".into()), Cell::DateTimeTz("2024-06-01 15:00:00.123456+00:00".into())]);
    assert_eq!(a[0][2], Cell::Decimal("-170141183460469231731687303715884105728".into()));
    assert_eq!(a[1][2], Cell::Decimal("170141183460469231731687303715884105727".into()));
    assert_eq!(a[0][3], Cell::Time("24:00:00".into()));
    assert_eq!((a[0][4].clone(), a[1][5].clone()), (Cell::Text("infinity".into()), Cell::Text("-infinity".into())));
    assert_eq!((a[2][5].clone(), a[3][5].clone()), (Cell::DateTime("-0043-03-15 12:00:00".into()), Cell::DateTime("+12000-01-01 00:00:00".into())));
    assert_eq!(a[0][6], Cell::Json(r#"{"a":1,"b":"x"}"#.into()));
    let cols = ["id", "tz", "h", "t", "d", "ts", "st", "l", "m", "dec"];
    let n = s.bulk_load(&spec(table("rv", "b"), &cols, 1000), &[], &mut Batches(vec![RowBatch { rows: a.clone(), bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(n, 4);
    assert_eq!(rows(&mut s, &table("rv", "b"), None).await, a);
    // Same values in SQL, both ways (NULLs compared as equal).
    let diff = rows(
        &mut s,
        &table("rv", "a"),
        None,
    )
    .await;
    assert_eq!(diff.len(), 4);
    run(&mut s, "CREATE TABLE rv.diff AS SELECT count(*) AS id FROM (SELECT * FROM rv.a EXCEPT SELECT * FROM rv.b UNION ALL SELECT * FROM (SELECT * FROM rv.b EXCEPT SELECT * FROM rv.a))").await;
    assert_eq!(rows(&mut s, &table("rv", "diff"), None).await, vec![vec![Cell::Int(0)]]);
    // Every column in another order: still ingest, cells reordered.
    run(&mut s, "DELETE FROM rv.b").await;
    let rev: Vec<&str> = cols.iter().rev().copied().collect();
    let reversed: Vec<Vec<Cell>> = a.iter().map(|r| r.iter().rev().cloned().collect()).collect();
    s.bulk_load(&spec(table("rv", "b"), &rev, 1000), &[], &mut Batches(vec![RowBatch { rows: reversed, bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(rows(&mut s, &table("rv", "b"), None).await, a);
    // A subset goes through the prepared INSERT, same conversions.
    run(&mut s, "DELETE FROM rv.b").await;
    let pick = [5usize, 1, 0, 3, 4, 9];
    let names: Vec<&str> = pick.iter().map(|&i| cols[i]).collect();
    let sub: Vec<Vec<Cell>> = a.iter().map(|r| pick.iter().map(|&i| r[i].clone()).collect()).collect();
    s.bulk_load(&spec(table("rv", "b"), &names, 1000), &[], &mut Batches(vec![RowBatch { rows: sub.clone(), bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    let mut b = rows(&mut s, &table("rv", "b"), Some(names.iter().map(|n| n.to_string()).collect())).await;
    b.sort_by_key(|r| format!("{:?}", r[2]));
    assert_eq!(b, sub);
    // What GizmoSQL can't bind as a parameter is said clearly, and leaves nothing.
    run(&mut s, "DELETE FROM rv.b").await;
    let only = |i: usize| vec![RowBatch { rows: a.iter().map(|r| vec![r[0].clone(), r[i].clone()]).collect(), bytes: 0 }].into_iter();
    let r = s.bulk_load(&spec(table("rv", "b"), &["id", "st"], 1000), &[], &mut Batches(only(6)), &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("anidados")), "{r:?}");
    let r = s.bulk_load(&spec(table("rv", "b"), &["id", "h"], 1000), &[], &mut Batches(only(2)), &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("HUGEINT")), "{r:?}");
    assert_eq!(count(&mut s, &table("rv", "b")).await, 0);
}

/// Rows of `rv.a` and `rv.b` that differ in SQL, both ways (NULLs equal).
async fn differing(s: &mut Box<dyn Session>) -> i64 {
    run(s, "DROP TABLE IF EXISTS rv.diff; CREATE TABLE rv.diff AS SELECT count(*) AS id FROM (SELECT * FROM rv.a EXCEPT SELECT * FROM rv.b UNION ALL SELECT * FROM (SELECT * FROM rv.b EXCEPT SELECT * FROM rv.a))").await;
    match rows(s, &table("rv", "diff"), None).await[..] {
        [ref r] => match r[..] {
            [Cell::Int(n)] => n,
            _ => panic!("{r:?}"),
        },
        ref r => panic!("{r:?}"),
    }
}

/// DuckDB types whose Arrow form isn't faithful (`UHUGEINT` as raw bits,
/// `BIT` and `BIGNUM` as internal bytes, `TIMETZ` without its offset) and
/// unions: read exactly, copied back equal in SQL, by ingest and by the
/// prepared INSERT; inside nested types they are refused, not corrupted.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_duckdb_types_as_text() {
    let Some((mut s, _serial)) = connect().await else { return };
    let ddl = "(id INT, u UHUGEINT, b BIT, tt TIMETZ, un UNION(n INT, s VARCHAR), v BIGNUM, extra INT DEFAULT 1)";
    setup(
        &mut s,
        &format!(
            "CREATE TABLE rv.a {ddl}; CREATE TABLE rv.b {ddl};
             INSERT INTO rv.a VALUES
               (1, 340282366920938463463374607431768211455, '10110', '12:34:56.789+05:30', union_value(s := 'hola'), '-123456789012345678901234567890123456789012', 1),
               (2, 170141183460469231731687303715884105728, '1', '00:00:00-08', union_value(n := 7), 0, 1),
               (3, 0, '0000000011', '23:59:59.999999+15:59', union_value(s := ''), '99999999999999999999999999999999999999999', 1),
               (4, NULL, NULL, NULL, NULL, NULL, 1);
             CREATE TABLE rv.n (id INT, l UHUGEINT[]); INSERT INTO rv.n VALUES (1, [1]);"
        ),
    )
    .await;
    let a = rows(&mut s, &table("rv", "a"), None).await;
    assert_eq!(a[0][1..6], [
        Cell::Decimal("340282366920938463463374607431768211455".into()),
        Cell::Text("10110".into()),
        Cell::Text("12:34:56.789+05:30".into()),
        Cell::Json(r#"{"s":"hola"}"#.into()),
        Cell::Decimal("-123456789012345678901234567890123456789012".into()),
    ]);
    assert_eq!(a[1][1..5], [Cell::Decimal("170141183460469231731687303715884105728".into()), Cell::Text("1".into()), Cell::Text("00:00:00-08".into()), Cell::Json(r#"{"n":7}"#.into())]);
    assert!(a[3][1..6].iter().all(|c| *c == Cell::Null), "{:?}", a[3]);
    // The columns say their DuckDB type.
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table("rv", "a"), columns: Some(vec!["tt".into(), "id".into(), "u".into()]), filter: Some("id = 1".into()) }, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>()[..1], ["TIME WITH TIME ZONE"]);
    assert_eq!(got.rows, vec![vec![Cell::Text("12:34:56.789+05:30".into()), Cell::Int(1), Cell::Decimal("340282366920938463463374607431768211455".into())]]);
    let cols = ["id", "u", "b", "tt", "un", "v", "extra"];
    // Every column: ingest.
    let n = s.bulk_load(&spec(table("rv", "b"), &cols, 1000), &[], &mut Batches(vec![RowBatch { rows: a.clone(), bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(n, 4);
    assert_eq!(differing(&mut s).await, 0);
    // Some of them (no union: GizmoSQL doesn't bind nested parameters): the prepared INSERT.
    run(&mut s, "DELETE FROM rv.b").await;
    let pick = [0usize, 1, 2, 3, 5];
    let names: Vec<&str> = pick.iter().map(|&i| cols[i]).collect();
    let sub: Vec<Vec<Cell>> = a.iter().map(|r| pick.iter().map(|&i| r[i].clone()).collect()).collect();
    s.bulk_load(&spec(table("rv", "b"), &names, 1000), &[], &mut Batches(vec![RowBatch { rows: sub, bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    run(&mut s, "UPDATE rv.b SET un = (SELECT un FROM rv.a WHERE rv.a.id = rv.b.id)").await;
    assert_eq!(differing(&mut s).await, 0);
    // Nested: refused both ways, nothing loaded.
    let r = s.read_batches(&ReadSpec { table: table("rv", "n"), columns: None, filter: None }, Arc::new(Mutex::new(Collect::default()))).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("anidado")), "{r:?}");
    let r = s.bulk_load(&spec(table("rv", "n"), &["id", "l"], 1000), &[], &mut Batches(vec![RowBatch { rows: vec![vec![Cell::Int(2), Cell::Json("[2]".into())]], bytes: 0 }].into_iter()), &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("anidado")), "{r:?}");
    assert_eq!(rows(&mut s, &table("rv", "n"), Some(vec!["id".into()])).await, vec![vec![Cell::Int(1)]]);
}

/// DuckDB `INTERVAL` travels as its text: GizmoSQL's Arrow form (the
/// microseconds times 1000 in an i64) wraps a time part past about 292
/// years into another value. Read exactly and copied back equal in SQL;
/// inside a nested type it's refused, not corrupted.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_duckdb_interval() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(
        &mut s,
        "CREATE TABLE rv.a (id INT, iv INTERVAL); CREATE TABLE rv.b (id INT, iv INTERVAL);
         INSERT INTO rv.a VALUES (1, to_hours(3000000)), (2, to_microseconds(9223372036854775807)),
           (3, INTERVAL '1 year 2 months 3 days 04:05:06.789'), (4, INTERVAL '-26 hours'), (5, NULL);
         CREATE TABLE rv.n (id INT, l INTERVAL[]); INSERT INTO rv.n VALUES (1, [INTERVAL '1 day']);",
    )
    .await;
    let a = rows(&mut s, &table("rv", "a"), None).await;
    assert_eq!(a[0][1], Cell::Text("3000000:00:00".into()));
    assert_eq!(a[1][1], Cell::Text("2562047788:00:54.775807".into()));
    assert_eq!(a[4][1], Cell::Null);
    // DuckDB can't parse back its own text of the largest interval: the
    // load fails whole instead of storing another value.
    let load = |rows: Vec<Vec<Cell>>| Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());
    let r = s.bulk_load(&spec(table("rv", "b"), &["id", "iv"], 1000), &[], &mut load(a.clone()), &|_| {}).await;
    assert!(matches!(&r, Err(e) if e.is_query() && e.to_string().contains("INTERVAL")), "{r:?}");
    assert_eq!(count(&mut s, &table("rv", "b")).await, 0);
    run(&mut s, "DELETE FROM rv.a WHERE id = 2").await;
    let a: Vec<Vec<Cell>> = a.into_iter().filter(|r| r[0] != Cell::Int(2)).collect();
    let n = s.bulk_load(&spec(table("rv", "b"), &["id", "iv"], 1000), &[], &mut load(a), &|_| {}).await.unwrap();
    assert_eq!(n, 4);
    assert_eq!(differing(&mut s).await, 0);
    let r = s.read_batches(&ReadSpec { table: table("rv", "n"), columns: None, filter: None }, Arc::new(Mutex::new(Collect::default()))).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("anidado")), "{r:?}");
}

/// Wide rows: a server batch past tonic's 4 MiB default, and one 6 MB
/// value, read back whole.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_wide_rows() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.w AS SELECT range AS id, repeat('x', 4096) AS s FROM range(4096); CREATE TABLE rv.blob (id BIGINT, b BLOB);").await;
    assert_eq!(count(&mut s, &table("rv", "w")).await, 4096);
    let wide = rows(&mut s, &table("rv", "w"), None).await;
    assert!(wide.iter().all(|r| r[1] == Cell::Text("x".repeat(4096))));
    let big: Vec<u8> = (0..6_000_000u32).map(|i| (i % 251) as u8).collect();
    let b = RowBatch { rows: vec![vec![Cell::Int(1), Cell::Bytes(big.clone())]], bytes: 0 };
    s.bulk_load(&spec(table("rv", "blob"), &["id", "b"], 1000), &[], &mut Batches(vec![b].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(rows(&mut s, &table("rv", "blob"), None).await, vec![vec![Cell::Int(1), Cell::Bytes(big)]]);
}

/// A window also closes at `commit_bytes`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn flightsql_commit_bytes() {
    let Some((mut s, _serial)) = connect().await else { return };
    setup(&mut s, "CREATE TABLE rv.blob (id BIGINT, b BLOB); CREATE TABLE rv.sub (id BIGINT, b BLOB, extra INT DEFAULT 1);").await;
    let batches = || (0..6).map(|k| RowBatch { rows: (0..2).map(|i| vec![Cell::Int(k * 2 + i), Cell::Bytes(vec![7; 512 * 1024])]).collect(), bytes: 0 }).collect::<Vec<_>>();
    for name in ["blob", "sub"] {
        let mut sp = spec(table("rv", name), &["id", "b"], 100_000);
        sp.commit_bytes = 2 * 1024 * 1024;
        let reports = Mutex::new(Vec::new());
        let n = s.bulk_load(&sp, &[], &mut Batches(batches().into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
        assert_eq!(n, 12);
        assert_eq!(reports.into_inner().unwrap(), vec![4, 8, 12], "{name}");
    }
}

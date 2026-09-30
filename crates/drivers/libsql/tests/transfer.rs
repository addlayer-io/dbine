//! Bulk transfer against a real sqld (`dbine-test-libsql`, port 25880):
//! `cargo test -p dbine-driver-libsql --test transfer -- --ignored`
//! (`--release … --nocapture` prints the 1M-row rates). `DBINE_LIBSQL_URL`
//! points elsewhere.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Counts the bytes allocated, and their peak (for the read's memory bound).
struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = CUR.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = CUR.fetch_add(new - l.size(), Ordering::Relaxed) + new - l.size();
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CUR.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The tests share one server and the allocator's counters: one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn url() -> String {
    std::env::var("DBINE_LIBSQL_URL").unwrap_or_else(|_| "http://localhost:25880".into())
}

async fn open(read_only: bool) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "libsql".into(), host: url(), read_only, ..Default::default() };
    dbine_driver_libsql::drivers().pop().unwrap().connect(&cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
}

fn all(name: &str) -> ReadSpec {
    ReadSpec { table: table(name), columns: None, filter: None }
}

#[derive(Default)]
struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.cols = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

async fn read_all(s: &mut Box<dyn Session>, spec: &ReadSpec) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    (c.cols, c.rows)
}

struct VecSource(std::vec::IntoIter<RowBatch>);

impl VecSource {
    fn new(rows: Vec<Vec<Cell>>, per: usize) -> Self {
        VecSource(rows.chunks(per).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect::<Vec<_>>().into_iter())
    }
}

#[dbine_driver::async_trait]
impl BatchSource for VecSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn load_spec(name: &str, cols: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

fn unique(tag: &str) -> String {
    format!("xfer_{tag}_{}", std::process::id())
}

#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn round_trip_every_type_with_windows() {
    let _serial = SERIAL.lock().await;
    let t = unique("rt");
    let mut s = open(false).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, i INTEGER, r REAL, s TEXT NOT NULL, b BLOB, n NUMERIC)")).await;
    // Just under 5 MB: sqld caps a value, and a whole row, at 5 000 000
    // bytes (SQLITE_LIMIT_LENGTH); a 5 MiB blob is refused by the server
    // itself ("string or blob too big").
    let big: Vec<u8> = (0..4_990_000).map(|i| (i % 251) as u8).collect();
    let rows = vec![
        vec![Cell::Int(1), Cell::Int(i64::MIN), Cell::Float(1.5), Cell::Text("ñandú 'q'".into()), Cell::Bytes(vec![0, 255]), Cell::Decimal("12.25".into())],
        vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Text(String::new()), Cell::Null, Cell::Null],
        vec![Cell::Int(3), Cell::Int(i64::MAX), Cell::Float(-1e300), Cell::Text("big".into()), Cell::Bytes(big.clone()), Cell::Int(7)],
        vec![Cell::Int(4), Cell::Bool(true), Cell::Float(0.1), Cell::Uuid("123e4567-e89b-12d3-a456-426614174000".into()), Cell::Bytes(vec![]), Cell::UInt(5)],
    ];
    let committed = Arc::new(Mutex::new(Vec::new()));
    let c2 = committed.clone();
    let cols = ["id", "i", "r", "s", "b", "n"];
    // Windows of 3 rows over batches of 2: a window spans two requests.
    let mut src = VecSource::new(rows.clone(), 2);
    let n = s.bulk_load(&load_spec(&t, &cols, 3), &[], &mut src, &move |n| c2.lock().unwrap().push(n)).await.unwrap();
    assert_eq!(n, 4);
    assert_eq!(*committed.lock().unwrap(), vec![3, 4]);

    let (tcols, back) = read_all(&mut s, &all(&t)).await;
    assert_eq!(tcols.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(), ["INTEGER", "INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC"]);
    assert!(!tcols[0].nullable && !tcols[3].nullable && tcols[1].nullable);
    assert_eq!(back[0][..5], rows[0][..5]);
    assert_eq!(back[0][5], Cell::Float(12.25));
    assert_eq!(back[1], rows[1]);
    assert_eq!(back[2], rows[2]);
    assert_eq!(back[2][4], Cell::Bytes(big));
    assert_eq!(back[3][1], Cell::Int(1));
    assert_eq!(back[3][4], Cell::Bytes(vec![]));

    // Subset and filter.
    let spec = ReadSpec { table: table(&t), columns: Some(vec!["s".into(), "id".into()]), filter: Some("id IN (2, 4)".into()) };
    let (_, back) = read_all(&mut s, &spec).await;
    assert_eq!(back.len(), 2);
    assert_eq!(back[0], vec![Cell::Text(String::new()), Cell::Int(2)]);

    // A view has no rowid: it's read through a streaming cursor.
    run(&mut s, &format!("DROP VIEW IF EXISTS {t}_v; CREATE VIEW {t}_v AS SELECT id, b FROM {t}")).await;
    let (vcols, vrows) = read_all(&mut s, &all(&format!("{t}_v"))).await;
    assert_eq!(vcols.len(), 2);
    assert_eq!(vrows.len(), 4);
    assert_eq!(vrows[2][1], rows[2][4]);
    run(&mut s, &format!("DROP VIEW {t}_v")).await;

    // A failing row rolls back its window; the committed one stays.
    run(&mut s, &format!("DELETE FROM {t}")).await;
    let mut src = VecSource::new(vec![vec![Cell::Int(1)], vec![Cell::Int(2)], vec![Cell::Null]], 1);
    let e = s.bulk_load(&load_spec(&t, &["s"], 2), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("NOT NULL")), "{e:?}");
    let (_, back) = read_all(&mut s, &ReadSpec { table: table(&t), columns: Some(vec!["s".into()]), filter: None }).await;
    // Window 1 (rows 1 and 2) committed; window 2 (row 3) rolled back.
    assert_eq!(back, vec![vec![Cell::Text("1".into())], vec![Cell::Text("2".into())]]);
    run(&mut s, &format!("INSERT INTO {t} (s) VALUES ('after')")).await; // no transaction left open

    // A read-only session refuses to load.
    let mut ro = open(true).await;
    let mut src = VecSource::new(vec![vec![Cell::Text("x".into())]], 1);
    assert!(ro.bulk_load(&load_spec(&t, &["s"], 10), &[], &mut src, &|_| {}).await.is_err());
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

/// 1M rows in release (50k in debug), over HTTP to the local sqld.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn load_benchmark() {
    let _serial = SERIAL.lock().await;
    let n: i64 = if cfg!(debug_assertions) { 50_000 } else { 1_000_000 };
    let t = unique("bench");
    let mut s = open(false).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, a INTEGER, f REAL, s TEXT, b BLOB)")).await;
    let rows: Vec<Vec<Cell>> = (0..n)
        .map(|i| vec![Cell::Int(i), Cell::Int(i * 7), Cell::Float(i as f64 / 3.0), Cell::Text(format!("row {i:08}")), Cell::Bytes(vec![(i % 256) as u8; 16])])
        .collect();
    let mut src = VecSource::new(rows, 1_000);
    let start = Instant::now();
    let loaded = s.bulk_load(&load_spec(&t, &["id", "a", "f", "s", "b"], LoadSpec::DEFAULT_COMMIT_ROWS), &[], &mut src, &|_| {}).await.unwrap();
    let load = start.elapsed();
    assert_eq!(loaded, n as u64);
    let start = Instant::now();
    let (_, back) = read_all(&mut s, &all(&t)).await;
    let read = start.elapsed();
    assert_eq!(back.len(), n as usize);
    assert_eq!(back[12_345][3], Cell::Text("row 00012345".into()));
    let rate = |d: std::time::Duration| (n as f64 / d.as_secs_f64()) as u64;
    println!("libsql {n} rows: bulk_load {load:?} ({} rows/s), read_batches {read:?} ({} rows/s)", rate(load), rate(read));
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> usize {
    read_all(s, &ReadSpec { table: table(t), columns: Some(vec!["id".into()]), filter: None }).await.1.len()
}

/// Gives its batches, then never ends (a source still reading when the
/// load is cancelled).
struct Stalled(Vec<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Stalled {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.0.pop() {
            Some(b) => Some(b),
            None => std::future::pending().await,
        }
    }
}

/// The orchestrator cancels by dropping the load's future. Whatever the
/// moment, nothing may be committed after the drop that survives the
/// cleanup it does next (emptying the table), and the write lock must not
/// stay held until the server's stream expires.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn cancelled_load_commits_nothing_late() {
    let _serial = SERIAL.lock().await;
    let t = unique("cancel");
    let mut other = open(false).await;
    run(&mut other, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, s TEXT)")).await;
    let pad = "x".repeat(200);
    let rows: Vec<Vec<Cell>> = (0..6000).map(|i| vec![Cell::Int(i), Cell::Text(format!("{i} {pad}"))]).collect();
    let mut cancelled = 0;
    for ms in [0u64, 10, 30, 60, 100, 150, 250, 400, 700] {
        let mut s = open(false).await;
        // One request with every row, and its window's end.
        let mut src = VecSource::new(rows.clone(), 6000);
        let spec = load_spec(&t, &["id", "s"], 6000);
        let r = tokio::time::timeout(Duration::from_millis(ms), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
        if let Ok(done) = r {
            assert_eq!(done.unwrap(), 6000);
        } else {
            cancelled += 1;
        }
        // The orchestrator's cleanup, right away: it must not wait for the
        // abandoned stream to expire.
        let start = Instant::now();
        run(&mut other, &format!("DELETE FROM {t}")).await;
        assert!(start.elapsed() < Duration::from_secs(3), "{ms} ms: the cleanup waited {:?}", start.elapsed());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(count(&mut other, &t).await, 0, "{ms} ms: rows committed after the cancel's cleanup");
        drop(s);
    }
    assert!(cancelled > 0, "no load was cancelled: the rows went too fast to test");

    // Cancelled between requests, with the window open (the source is
    // still reading): the window is rolled back and the lock freed.
    let mut s = open(false).await;
    let mut src = Stalled(vec![RowBatch { rows: rows[..100].to_vec(), bytes: 0 }]);
    let spec = load_spec(&t, &["id", "s"], 100_000);
    assert!(tokio::time::timeout(Duration::from_millis(800), s.bulk_load(&spec, &[], &mut src, &|_| {})).await.is_err());
    let start = Instant::now();
    run(&mut other, &format!("INSERT INTO {t} (id, s) VALUES (-1, 'other')")).await;
    assert!(start.elapsed() < Duration::from_secs(3), "the insert waited {:?}", start.elapsed());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count(&mut other, &t).await, 1);
    // The session keeps working (on a new stream).
    let mut src = VecSource::new(rows[..10].to_vec(), 10);
    assert_eq!(s.bulk_load(&spec, &[], &mut src, &|_| {}).await.unwrap(), 10);
    assert_eq!(count(&mut other, &t).await, 11);
    run(&mut other, &format!("DROP TABLE {t}")).await;
}

/// Random REALs (every bit pattern that isn't NaN or ±∞, subnormals
/// included) keep their bits through a load and a read, paged and
/// through a cursor; ±∞ is stored as a REAL and read back with its sign.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn reals_keep_their_bits() {
    let _serial = SERIAL.lock().await;
    let t = unique("reals");
    let mut s = open(false).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, r REAL NOT NULL)")).await;
    let mut x = 0x0123_4567_89AB_CDEFu64;
    let mut vals: Vec<f64> = Vec::new();
    while vals.len() < 3000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let f = f64::from_bits(x);
        if f.is_finite() && f != 0.0 {
            vals.push(f);
        }
    }
    vals.extend([1.0715660391465826e-75, 5e-324, -5e-324, f64::MAX, f64::MIN_POSITIVE, 0.1, f64::INFINITY, f64::NEG_INFINITY]);
    let rows: Vec<Vec<Cell>> = vals.iter().enumerate().map(|(i, f)| vec![Cell::Int(i as i64), Cell::Float(*f)]).collect();
    let mut src = VecSource::new(rows.clone(), 1000);
    assert_eq!(s.bulk_load(&load_spec(&t, &["id", "r"], 2000), &[], &mut src, &|_| {}).await.unwrap(), vals.len() as u64);

    // Stored as REAL, the infinities too (not the text 'Inf').
    let bad = ReadSpec { table: table(&t), columns: Some(vec!["id".into()]), filter: Some("typeof(r) <> 'real'".into()) };
    assert_eq!(read_all(&mut s, &bad).await.1, Vec::<Vec<Cell>>::new());
    let inf = ReadSpec { table: table(&t), columns: Some(vec!["r".into()]), filter: Some("r IN (9e999, -9e999)".into()) };
    assert_eq!(read_all(&mut s, &inf).await.1, vec![vec![Cell::Float(f64::INFINITY)], vec![Cell::Float(f64::NEG_INFINITY)]]);
    let check = |back: &[Vec<Cell>], vals: &[f64]| {
        assert_eq!(back.len(), vals.len());
        for (row, v) in back.iter().zip(vals) {
            match row[1] {
                Cell::Float(f) => assert_eq!(f.to_bits(), v.to_bits(), "{v:e} came back as {f:e}"),
                ref c => panic!("{v:e} came back as {c:?}"),
            }
        }
    };
    let (_, back) = read_all(&mut s, &all(&t)).await;
    check(&back, &vals);

    // Through a cursor (a view has no rowid): exact too; an infinity there
    // can't be asked again, so it's refused, not turned into NULL.
    run(&mut s, &format!("DROP VIEW IF EXISTS {t}_v; CREATE VIEW {t}_v AS SELECT id, r FROM {t} WHERE abs(r) < 9e999")).await;
    let (_, back) = read_all(&mut s, &all(&format!("{t}_v"))).await;
    check(&back, &vals[..vals.len() - 2]);
    run(&mut s, &format!("DROP VIEW {t}_v; CREATE VIEW {t}_v AS SELECT id, r FROM {t}")).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let e = s.read_batches(&all(&format!("{t}_v")), sink).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    run(&mut s, &format!("DROP VIEW {t}_v")).await;

    // NaN can't be stored: refused, not turned into NULL.
    let mut src = VecSource::new(vec![vec![Cell::Int(-1), Cell::Float(f64::NAN)]], 1);
    let e = s.bulk_load(&load_spec(&t, &["id", "r"], 10), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

struct Discard;

impl BatchSink for Discard {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, _: RowBatch) -> std::io::Result<()> {
        Ok(())
    }
}

/// A 20 000 × 60 INTEGER table (~18 MiB of cells) read into a sink that
/// throws the rows away stays well under the ~32 MiB a table may hold in
/// flight (a `serde_json::Value` tree per page took 360 MiB).
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn read_memory_is_bounded() {
    let _serial = SERIAL.lock().await;
    let t = unique("mem");
    let mut s = open(false).await;
    let cols: Vec<String> = (0..60).map(|i| format!("c{i}")).collect();
    let defs = cols.iter().map(|c| format!("{c} INTEGER")).collect::<Vec<_>>().join(", ");
    let exprs = (0..60).map(|i| format!("i * {} + 1000000000", i + 1)).collect::<Vec<_>>().join(", ");
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({defs})")).await;
    run(
        &mut s,
        &format!("INSERT INTO {t} WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000) SELECT {exprs} FROM n"),
    )
    .await;
    let base = CUR.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let n = s.read_batches(&all(&t), Arc::new(Mutex::new(Discard))).await.unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    println!("libsql read 20000 x 60: peak {} KiB above baseline", peak / 1024);
    assert_eq!(n, 20_000);
    assert!(peak < 32 * 1024 * 1024, "peak {} MiB", peak / (1024 * 1024));
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

/// Every power of two a double holds, both signs, and -0 in a column
/// without type (a REAL column turns -0.0 into 0.0 itself); a float column
/// that also carries text keeps both.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn reals_every_exponent_and_mixed_columns() {
    let _serial = SERIAL.lock().await;
    let t = unique("pow2");
    let mut s = open(false).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, r REAL, a)")).await;
    let pow2 = |e: i32| if e >= -1022 { f64::from_bits(((e + 1023) as u64) << 52) } else { f64::from_bits(1u64 << (e + 1074)) };
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    for e in -1074..=1023 {
        let f = pow2(e) * if e % 2 == 0 { 1.0 } else { -1.0 };
        // `a` alternates types: a float, then text, then an integer.
        let a = match e.rem_euclid(3) {
            0 => Cell::Float(-f * 3.0),
            1 => Cell::Text(format!("{e}")),
            _ => Cell::Int(e.into()),
        };
        rows.push(vec![Cell::Int(rows.len() as i64), Cell::Float(f), a]);
    }
    rows.push(vec![Cell::Int(rows.len() as i64), Cell::Float(-0.0), Cell::Float(-0.0)]);
    rows.push(vec![Cell::Int(rows.len() as i64), Cell::Float(f64::NEG_INFINITY), Cell::Float(f64::INFINITY)]);
    let mut src = VecSource::new(rows.clone(), 700);
    assert_eq!(s.bulk_load(&load_spec(&t, &["id", "r", "a"], 1500), &[], &mut src, &|_| {}).await.unwrap(), rows.len() as u64);
    let (_, back) = read_all(&mut s, &all(&t)).await;
    assert_eq!(back.len(), rows.len());
    // SQLite's REAL affinity stores an integral real as an integer: -0.0
    // comes back as 0.0 there (a literal -0.0 does the same).
    let n = rows.len();
    rows[n - 2][1] = Cell::Float(0.0);
    for (got, want) in back.iter().zip(&rows) {
        for (g, w) in got.iter().zip(want) {
            match (g, w) {
                (Cell::Float(g), Cell::Float(w)) => assert_eq!(g.to_bits(), w.to_bits(), "{w:e} came back as {g:e}"),
                _ => assert_eq!(g, w),
            }
        }
    }
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

/// An orchestrator-sized batch of REALs (1000 rows × 60 columns, one
/// window) costs about what integers do, whatever the exponents: small
/// values near 1e-300 once took 45 s (a chain of powers per value), random
/// bit patterns 12 s.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn reals_load_as_fast_as_integers() {
    let _serial = SERIAL.lock().await;
    let t = unique("realspeed");
    let mut s = open(false).await;
    let cols: Vec<String> = (0..60).map(|i| format!("c{i}")).collect();
    let names: Vec<&str> = cols.iter().map(String::as_str).collect();
    let ddl = cols.iter().map(|c| format!("{c} REAL")).collect::<Vec<_>>().join(", ");
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for kind in ["near 1e-300", "random bits", "uniform [0,1)"] {
        let mut gen = || match kind {
            "near 1e-300" => 1e-300 * (1.0 + (next() >> 11) as f64 / (1u64 << 53) as f64),
            "random bits" => loop {
                let f = f64::from_bits(next());
                if f.is_finite() {
                    break f;
                }
            },
            _ => (next() >> 11) as f64 / (1u64 << 53) as f64,
        };
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} ({ddl})")).await;
        let rows: Vec<Vec<Cell>> = (0..1000).map(|_| (0..60).map(|_| Cell::Float(gen())).collect()).collect();
        let mut src = VecSource::new(rows.clone(), 1000);
        let start = Instant::now();
        assert_eq!(s.bulk_load(&load_spec(&t, &names, 10_000), &[], &mut src, &|_| {}).await.unwrap(), 1000);
        let took = start.elapsed();
        println!("libsql 1000 × 60 REAL ({kind}): {took:?}");
        assert!(took < Duration::from_secs(5), "{kind}: {took:?}");
        let (_, back) = read_all(&mut s, &all(&t)).await;
        let mut bad = 0;
        for (g, w) in back.iter().flatten().zip(rows.iter().flatten()) {
            match (g, w) {
                (Cell::Float(g), Cell::Float(w)) if g.to_bits() == w.to_bits() || (*w == 0.0 && *g == 0.0) => {}
                _ => bad += 1,
            }
        }
        assert_eq!(bad, 0, "{kind}");
    }
    run(&mut s, &format!("DROP TABLE {t}")).await;
}

/// A proxy in front of sqld that forwards every request but, for the
/// `cut`-th one carrying an INSERT, drops the connection instead of
/// answering: the request ran, its reply never arrives.
async fn cutting_proxy(cut: usize) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let inserts = Arc::new(AtomicUsize::new(0));
    let http = reqwest::Client::new();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (inserts, http) = (inserts.clone(), http.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    // One request: head, then a body of content-length.
                    let head_end = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                        let mut chunk = [0u8; 65536];
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap_or(0));
                    while buf.len() < head_end + len {
                        let mut chunk = [0u8; 65536];
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = buf[head_end..head_end + len].to_vec();
                    buf.drain(..head_end + len);
                    let is_insert = String::from_utf8_lossy(&body).contains("INSERT INTO");
                    let resp = http.post(format!("{}{path}", url())).header("content-type", "application/json").body(body).send().await.unwrap();
                    let status = resp.status().as_u16();
                    let bytes = resp.bytes().await.unwrap();
                    if is_insert && inserts.fetch_add(1, Ordering::SeqCst) + 1 == cut {
                        return; // the reply is lost
                    }
                    let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", bytes.len());
                    if sock.write_all(head.as_bytes()).await.is_err() || sock.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

/// A load request that ran but whose reply was lost leaves the client's
/// baton stale. The failed load must not leave its window's transaction
/// open on the orphaned stream (holding the write lock until sqld expires
/// it): once it returns, another writer goes through at once, and nothing
/// of the window was committed.
#[tokio::test]
#[ignore = "needs dbine-test-libsql (sqld) on port 25880"]
async fn lost_reply_leaves_no_lock() {
    let _serial = SERIAL.lock().await;
    let t = unique("lost");
    let mut other = open(false).await;
    run(&mut other, &format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INTEGER PRIMARY KEY, s TEXT)")).await;
    let proxy = cutting_proxy(2).await;
    let cfg = ConnectionConfig { driver: "libsql".into(), host: proxy, ..Default::default() };
    let mut s = dbine_driver_libsql::drivers().pop().unwrap().connect(&cfg, None).await.unwrap();
    let rows: Vec<Vec<Cell>> = (0..300).map(|i| vec![Cell::Int(i), Cell::Text(format!("r{i}"))]).collect();
    // Three requests in one window; the second one's reply is lost.
    let mut src = VecSource::new(rows, 100);
    let e = s.bulk_load(&load_spec(&t, &["id", "s"], 10_000), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(matches!(e, Error::Connect(_)), "{e:?}");
    let start = Instant::now();
    run(&mut other, &format!("INSERT INTO {t} (id, s) VALUES (-1, 'other')")).await;
    assert!(start.elapsed() < Duration::from_secs(2), "the writer waited {:?} for the orphaned window", start.elapsed());
    assert_eq!(count(&mut other, &t).await, 1);
    run(&mut other, &format!("DROP TABLE {t}")).await;
}

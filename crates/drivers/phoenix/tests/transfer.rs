//! Bulk load and typed read against a real Phoenix Query Server (see
//! `integration.rs` for the container):
//! `DBINE_TEST_PHOENIX_URL=http://localhost:25165 \
//!  cargo test -p dbine-driver-phoenix --release -- --ignored transfer --test-threads 1 --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: usize = 50_000;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig { driver: "phoenix".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() };
    if let Ok(s) = std::env::var("DBINE_TEST_PHOENIX_SERIALIZATION") {
        c.options.insert("serialization".into(), s);
    }
    Some(c)
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
        Cell::Decimal(format!("{}.{:02}", n - 25_000, i % 100 + 1)),
        Cell::Float(i as f64 / 8.0),
        Cell::Text(format!("fila {i} ñ 'x'")),
        if i.is_multiple_of(5) { Cell::Null } else { Cell::Bytes(vec![(i % 256) as u8; i % 40 + 1]) },
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::Time(format!("{:02}:{:02}:{:02}.{:03}", i % 24, i % 60, i % 60, i % 999 + 1)),
        Cell::DateTime(format!("2024-01-{:02} 10:00:00.{:03}", i % 28 + 1, i % 999 + 1)),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Int((i % 1000) as i64),
    ]
}

/// Decimals by value (Phoenix may drop trailing zeros).
fn norm(c: &Cell) -> Cell {
    match c {
        Cell::Decimal(s) if s.contains('.') => Cell::Decimal(s.trim_end_matches('0').trim_end_matches('.').to_string()),
        c => c.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, None).await.unwrap();
    run(
        &mut s,
        "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XFER;
         CREATE TABLE DBINE.XFER (ID BIGINT NOT NULL PRIMARY KEY, N BIGINT, D DECIMAL(18,2), F DOUBLE, S VARCHAR, B VARBINARY,
                                  DT DATE, TM TIME, TS TIMESTAMP, OK BOOLEAN, I INTEGER)",
    )
    .await;
    let names = ["ID", "N", "D", "F", "S", "B", "DT", "TM", "TS", "OK", "I"];
    let obj = ObjectRef { kind: "table".into(), schema: Some("DBINE".into()), name: "XFER".into() };
    let batches: Vec<RowBatch> =
        (0..ROWS).collect::<Vec<_>>().chunks(1000).map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 }).collect();
    let spec = LoadSpec {
        table: obj.clone(),
        columns: names.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("phoenix: loaded {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);
    // The orchestrator's windows (100,000 rows) are cut to what Phoenix's
    // mutation buffer takes: several commits, each one reported.
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() > 1 && reports.windows(2).all(|w| w[0] < w[1]) && reports.last() == Some(&(ROWS as u64)), "{reports:?}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("phoenix: read {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, ROWS as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), names);
    assert_eq!(got.columns[2].type_name, "DECIMAL(18,2)");
    assert!(!got.columns[0].nullable && got.columns[1].nullable);
    for (i, r) in got.rows.iter().enumerate() {
        let want: Vec<Cell> = row(i).iter().map(norm).collect();
        let r: Vec<Cell> = r.iter().map(norm).collect();
        assert_eq!(r, want, "row {i}");
    }

    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec_r = ReadSpec { table: obj.clone(), columns: Some(vec!["ID".into(), "S".into()]), filter: Some("ID IN (1, 2, 3)".into()) };
    assert_eq!(s.read_batches(&spec_r, sink).await.unwrap(), 3);

    // A value that doesn't convert fails, and its uncommitted window is rolled back.
    run(&mut s, "DELETE FROM DBINE.XFER").await;
    let mut bad: Vec<RowBatch> = vec![RowBatch { rows: (0..500).map(row).collect(), bytes: 0 }];
    let mut r = row(500);
    r[1] = Cell::Text("no es número".into());
    bad.push(RowBatch { rows: vec![r], bytes: 0 });
    assert!(s.bulk_load(&spec, &[], &mut Batches(bad.into_iter()), &|_| {}).await.is_err());
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM DBINE.XFER", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(0));
    // The session still works with auto-commit back on.
    run(&mut s, "UPSERT INTO DBINE.XFER (ID) VALUES (1)").await;
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM DBINE.XFER", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(1));
    run(&mut s, "DROP TABLE DBINE.XFER").await;
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("DBINE".into()), name: name.into() }
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

fn batches(rows: Vec<Vec<Cell>>, per: usize) -> Batches {
    let mut out = Vec::new();
    let mut rows = rows.into_iter().peekable();
    while rows.peek().is_some() {
        out.push(RowBatch { rows: rows.by_ref().take(per).collect(), bytes: 0 });
    }
    Batches(out.into_iter())
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> serde_json::Value {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out.results[0].rows[0][0].clone()
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> u64 {
    let v = scalar(s, &format!("SELECT COUNT(*) FROM DBINE.{t}")).await;
    v.as_u64().or_else(|| v.as_str().and_then(|x| x.parse().ok())).unwrap()
}

async fn read_all(s: &mut Box<dyn Session>, t: &str) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table(t), columns: None, filter: None }, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    got
}

const TYPES: &str = "(ID BIGINT NOT NULL PRIMARY KEY, F FLOAT, UF UNSIGNED_FLOAT, UL UNSIGNED_LONG, UI UNSIGNED_INT, \
                     A INTEGER ARRAY, VA VARCHAR ARRAY, DT DATE, TM TIME, TS TIMESTAMP)";
const TYPE_COLS: [&str; 10] = ["ID", "F", "UF", "UL", "UI", "A", "VA", "DT", "TM", "TS"];

/// The same column name in two families (`A.V`, `B.V`): the widened DATE
/// read and the row bound name each column with its family, and every
/// value stays in its own column.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_families() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(
        &mut s,
        "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XFAM; \
         CREATE TABLE DBINE.XFAM (ID BIGINT NOT NULL PRIMARY KEY, A.V DATE, B.V DATE, A.S VARCHAR, B.S VARCHAR)",
    )
    .await;
    run(
        &mut s,
        "UPSERT INTO DBINE.XFAM VALUES (1, TO_DATE('2024-01-31 10:30:15.250'), TO_DATE('2023-05-06 00:00:00.000'), 'a', 'bb'); \
         UPSERT INTO DBINE.XFAM VALUES (2, NULL, TO_DATE('2020-02-29 23:59:59.999'), 'ccc', NULL)",
    )
    .await;
    let got = read_all(&mut s, "XFAM").await;
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["ID", "V", "V", "S", "S"]);
    assert_eq!(
        got.rows,
        vec![
            vec![
                Cell::Int(1),
                Cell::DateTime("2024-01-31 10:30:15.250".into()),
                Cell::Date("2023-05-06".into()),
                Cell::Text("a".into()),
                Cell::Text("bb".into()),
            ],
            vec![Cell::Int(2), Cell::Null, Cell::DateTime("2020-02-29 23:59:59.999".into()), Cell::Text("ccc".into()), Cell::Null],
        ]
    );
    run(&mut s, "DROP TABLE DBINE.XFAM").await;
}

/// FLOAT, Phoenix's UNSIGNED_* codes, arrays, and DATE / TIME with their full
/// value: loaded, read back, and copied Phoenix to Phoenix without loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_types() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, &format!("CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XTYPES; CREATE TABLE DBINE.XTYPES {TYPES}")).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS DBINE.XTYPES2; CREATE TABLE DBINE.XTYPES2 {TYPES}")).await;
    let rows = vec![
        vec![
            Cell::Int(1),
            Cell::Float(1.5),
            Cell::Float(2.25),
            Cell::UInt(5),
            Cell::UInt(7),
            Cell::Json("[1,2,3]".into()),
            Cell::Json(r#"["a","b c",null]"#.into()),
            Cell::DateTime("2024-01-31 10:30:15.250".into()),
            Cell::DateTime("2024-01-31 13:45:00.500".into()),
            Cell::DateTime("2024-01-31 13:45:00.123".into()),
        ],
        vec![
            Cell::Int(2),
            Cell::Float(1.1),
            Cell::Null,
            Cell::UInt(9_223_372_036_854_775_807),
            Cell::Null,
            Cell::Json("[]".into()),
            Cell::Null,
            Cell::Date("2024-02-01".into()),
            Cell::Time("08:00:00.250".into()),
            Cell::Null,
        ],
        vec![Cell::Int(3), Cell::Int(3), Cell::Null, Cell::Int(0), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null],
    ];
    let spec = load_spec("XTYPES", &TYPE_COLS);
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(rows.clone(), 10), &|_| {}).await.unwrap(), 3);
    // The server sees the values (a FLOAT used to arrive as 0).
    assert_eq!(scalar(&mut s, "SELECT TO_CHAR(F) FROM DBINE.XTYPES WHERE ID = 1").await, serde_json::json!("1.5"));
    assert_eq!(scalar(&mut s, "SELECT TO_CHAR(F) FROM DBINE.XTYPES WHERE ID = 3").await, serde_json::json!("3"));
    assert_eq!(
        scalar(&mut s, "SELECT TO_CHAR(DT, 'yyyy-MM-dd HH:mm:ss.SSS') FROM DBINE.XTYPES WHERE ID = 1").await,
        serde_json::json!("2024-01-31 10:30:15.250")
    );

    let got = read_all(&mut s, "XTYPES").await;
    assert_eq!(got.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>()[7..], ["DATE", "TIME", "TIMESTAMP"]);
    let want: Vec<Vec<Cell>> = vec![
        rows[0].clone(),
        rows[1].clone(),
        vec![Cell::Int(3), Cell::Float(3.0), Cell::Null, Cell::Int(0), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null],
    ];
    let norm = |r: &Vec<Cell>| -> Vec<Cell> {
        r.iter()
            .map(|c| match c {
                Cell::UInt(u) => Cell::Int(*u as i64),
                // An empty array reads back as no array.
                Cell::Json(j) if j == "[]" => Cell::Null,
                // "b c" and nulls inside a VARCHAR ARRAY stay; compare the JSON by value.
                Cell::Json(j) => Cell::Json(serde_json::from_str::<serde_json::Value>(j).unwrap().to_string()),
                c => c.clone(),
            })
            .collect()
    };
    for (i, (g, w)) in got.rows.iter().zip(&want).enumerate() {
        assert_eq!(norm(g), norm(w), "row {i}");
    }

    // Phoenix to Phoenix: what was read loads into the same structure.
    let n = got.rows.len();
    assert_eq!(s.bulk_load(&load_spec("XTYPES2", &TYPE_COLS), &[], &mut batches(got.rows.clone(), 10), &|_| {}).await.unwrap(), n as u64);
    let copy = read_all(&mut s, "XTYPES2").await;
    assert_eq!(copy.rows, got.rows);

    // What can't go faithfully fails instead of being cut.
    run(&mut s, "DELETE FROM DBINE.XTYPES2").await;
    let mut r = rows[2].clone();
    r[1] = Cell::Float(f64::MAX);
    assert!(s.bulk_load(&load_spec("XTYPES2", &TYPE_COLS), &[], &mut batches(vec![r], 10), &|_| {}).await.is_err());
    let mut r = rows[2].clone();
    r[9] = Cell::DateTime("2024-01-01 10:00:00.123456789".into());
    let e = s.bulk_load(&load_spec("XTYPES2", &TYPE_COLS), &[], &mut batches(vec![r], 10), &|_| {}).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
    assert_eq!(count(&mut s, "XTYPES2").await, 0);

    // A DATE written by SQL with a time keeps it on read.
    run(&mut s, "UPSERT INTO DBINE.XTYPES (ID, DT) VALUES (10, TO_DATE('2024-01-31 10:30:15.250'))").await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec_r = ReadSpec { table: table("XTYPES"), columns: Some(vec!["DT".into(), "ID".into()]), filter: Some("ID = 10".into()) };
    assert_eq!(s.read_batches(&spec_r, sink.clone()).await.unwrap(), 1);
    assert_eq!(sink.lock().unwrap().rows, vec![vec![Cell::DateTime("2024-01-31 10:30:15.250".into()), Cell::Int(10)]]);
    run(&mut s, "DROP TABLE DBINE.XTYPES; DROP TABLE DBINE.XTYPES2").await;
}

/// The orchestrator's default windows (100,000 rows, 512 MiB) on tables
/// Phoenix's mutation buffer can't hold in one commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_default_windows() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    for (cols, rows) in [(11usize, 30_000usize), (30, 10_000)] {
        let defs: Vec<String> = (1..cols).map(|i| format!("C{i} BIGINT")).collect();
        run(&mut s, &format!("CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XWIDE; CREATE TABLE DBINE.XWIDE (ID BIGINT NOT NULL PRIMARY KEY, {})", defs.join(", "))).await;
        let names: Vec<String> = std::iter::once("ID".to_string()).chain((1..cols).map(|i| format!("C{i}"))).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let data: Vec<Vec<Cell>> = (0..rows).map(|i| (0..cols).map(|c| Cell::Int((i * 100 + c) as i64)).collect()).collect();
        let reports = Mutex::new(Vec::new());
        let loaded = s.bulk_load(&load_spec("XWIDE", &names), &[], &mut batches(data, 1000), &|n| reports.lock().unwrap().push(n)).await.unwrap();
        assert_eq!(loaded, rows as u64);
        assert_eq!(reports.into_inner().unwrap().last(), Some(&(rows as u64)));
        assert_eq!(count(&mut s, "XWIDE").await, rows as u64);
    }
    run(&mut s, "DROP TABLE DBINE.XWIDE").await;
}

/// A cancelled load (its future dropped) leaves exactly the rows it
/// reported committed: nothing lands after it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_cancel() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XCANCEL; CREATE TABLE DBINE.XCANCEL (ID BIGINT NOT NULL PRIMARY KEY, S VARCHAR)").await;
    // Timeouts that fall at different points: before the first commit, and
    // (on a slow server too) while windows are being sent and committed.
    let mut committed_cases = 0;
    for ms in [60u64, 500, 1_500, 3_000, 5_000] {
        run(&mut s, "DELETE FROM DBINE.XCANCEL").await;
        let data: Vec<Vec<Cell>> = (0..20_000).map(|i| vec![Cell::Int(i), Cell::Text(format!("fila {i}"))]).collect();
        let mut spec = load_spec("XCANCEL", &["ID", "S"]);
        spec.commit_rows = 2_000;
        let reported = AtomicU64::new(0);
        let r = tokio::time::timeout(Duration::from_millis(ms), s.bulk_load(&spec, &[], &mut batches(data, 1000), &|n| reported.store(n, Ordering::SeqCst))).await;
        if r.is_ok() {
            continue;
        }
        let reported = reported.load(Ordering::SeqCst);
        let at_return = count(&mut s, "XCANCEL").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let later = count(&mut s, "XCANCEL").await;
        println!("cancel@{ms}ms: reported {reported}, {at_return} at return, {later} later");
        assert_eq!((at_return, later), (reported, reported), "cancel@{ms}ms");
        committed_cases += (reported > 0) as usize;
    }
    assert!(committed_cases > 0, "no cancel fell after a commit");
    // The session still works, with auto-commit back on.
    run(&mut s, "UPSERT INTO DBINE.XCANCEL (ID) VALUES (-1)").await;
    let mut s2 = d.connect(&c, None).await.unwrap();
    assert_eq!(scalar(&mut s2, "SELECT COUNT(*) FROM DBINE.XCANCEL WHERE ID = -1").await, serde_json::json!(1));
    run(&mut s, "DROP TABLE DBINE.XCANCEL").await;
}

/// Keys are never overwritten: a key repeated in the source, or already in
/// the table, fails the load, and the existing row keeps its values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_no_overwrite() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XDUP; CREATE TABLE DBINE.XDUP (ID BIGINT NOT NULL PRIMARY KEY, S VARCHAR)").await;
    let spec = load_spec("XDUP", &["ID", "S"]);
    let dup = vec![vec![Cell::Int(1), Cell::Text("a".into())], vec![Cell::Int(1), Cell::Text("b".into())]];
    let e = s.bulk_load(&spec, &[], &mut batches(dup, 10), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("clave"), "{e}");
    assert_eq!(count(&mut s, "XDUP").await, 1);

    // Into a table with rows: the existing row stays as it was.
    run(&mut s, "DELETE FROM DBINE.XDUP; UPSERT INTO DBINE.XDUP VALUES (1, 'original')").await;
    let rows = vec![vec![Cell::Int(1), Cell::Text("pisada".into())], vec![Cell::Int(2), Cell::Text("nueva".into())]];
    let e = s.bulk_load(&spec, &[], &mut batches(rows, 10), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("clave"), "{e}");
    assert_eq!(scalar(&mut s, "SELECT S FROM DBINE.XDUP WHERE ID = 1").await, serde_json::json!("original"));
    // New keys into a table with rows load.
    let rows = vec![vec![Cell::Int(3), Cell::Text("otra".into())]];
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(rows, 10), &|_| {}).await.unwrap(), 1);
    run(&mut s, "DROP TABLE DBINE.XDUP").await;
}

/// The system allocator, counting the bytes allocated now and at the peak
/// (what a read keeps in memory, whatever the allocator holds on to).
struct Counting;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(LIVE.fetch_add(l.size() as u64, Ordering::Relaxed) + l.size() as u64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) };
        LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, size: usize) -> *mut u8 {
        let q = unsafe { std::alloc::System.realloc(p, l, size) };
        if !q.is_null() {
            let now = LIVE.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
            PEAK.fetch_max(now, Ordering::Relaxed);
            LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

struct Discard;

impl BatchSink for Discard {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, _: RowBatch) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reading wide binary rows keeps memory bounded by bytes, not by frames of
/// thousands of rows: uniform rows, and small rows first then big ones
/// (a frame sized after the small ones used to take every big one at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_read_memory() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XBLOB; CREATE TABLE DBINE.XBLOB (ID BIGINT NOT NULL PRIMARY KEY, B VARBINARY)").await;
    let data: Vec<Vec<Cell>> = (0..400).map(|i| vec![Cell::Int(i), Cell::Bytes(vec![(i % 251) as u8; 100_000])]).collect();
    assert_eq!(s.bulk_load(&load_spec("XBLOB", &["ID", "B"]), &[], &mut batches(data, 20), &|_| {}).await.unwrap(), 400);
    let grew = read_peak(&mut s, "XBLOB").await;
    println!("phoenix: read 400 x 100 KB, peak +{grew} MiB");
    assert!(grew < 32, "peak +{grew} MiB");

    run(&mut s, "DELETE FROM DBINE.XBLOB").await;
    let data: Vec<Vec<Cell>> = (0..308).map(|i| vec![Cell::Int(i), Cell::Bytes(vec![(i % 251) as u8; if i < 8 { 1 } else { 100_000 }])]).collect();
    assert_eq!(s.bulk_load(&load_spec("XBLOB", &["ID", "B"]), &[], &mut batches(data, 20), &|_| {}).await.unwrap(), 308);
    let grew = read_peak(&mut s, "XBLOB").await;
    println!("phoenix: read 8 x 1 B then 300 x 100 KB, peak +{grew} MiB");
    assert!(grew < 32, "peak +{grew} MiB");
    run(&mut s, "DROP TABLE DBINE.XBLOB").await;
}

/// The most memory (MiB) a whole-table read had allocated at once, beyond
/// what was allocated when it started (run alone: `--test-threads 1`).
async fn read_peak(s: &mut Box<dyn Session>, t: &str) -> u64 {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let read = s.read_batches(&ReadSpec { table: table(t), columns: None, filter: None }, Arc::new(Mutex::new(Discard))).await.unwrap();
    let grew = PEAK.load(Ordering::SeqCst).saturating_sub(base) / (1024 * 1024);
    assert_eq!(read, count(s, t).await);
    grew
}

/// The elements of DATE / TIME arrays keep their time and date when read
/// (Avatica sends a DATE element as days) and load back as they were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn phoenix_transfer_date_arrays() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let def = "(ID BIGINT NOT NULL PRIMARY KEY, DA DATE ARRAY, TA TIME ARRAY)";
    run(&mut s, &format!("CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XDARR; CREATE TABLE DBINE.XDARR {def}")).await;
    run(&mut s, &format!("DROP TABLE IF EXISTS DBINE.XDARR2; CREATE TABLE DBINE.XDARR2 {def}")).await;
    run(
        &mut s,
        "UPSERT INTO DBINE.XDARR VALUES (1, ARRAY[TO_DATE('2024-01-31 10:30:15.250'), TO_DATE('2024-02-01 00:00:00.000')], \
         ARRAY[TO_TIME('1970-01-01 13:45:00.500'), TO_TIME('2024-01-31 08:00:00.000')])",
    )
    .await;
    let got = read_all(&mut s, "XDARR").await;
    let want = vec![vec![
        Cell::Int(1),
        Cell::Json(r#"["2024-01-31 10:30:15.250","2024-02-01"]"#.into()),
        Cell::Json(r#"["13:45:00.500","2024-01-31 08:00:00"]"#.into()),
    ]];
    assert_eq!(got.rows, want);
    assert_eq!(s.bulk_load(&load_spec("XDARR2", &["ID", "DA", "TA"]), &[], &mut batches(got.rows.clone(), 10), &|_| {}).await.unwrap(), 1);
    assert_eq!(read_all(&mut s, "XDARR2").await.rows, want);
    run(&mut s, "DROP TABLE DBINE.XDARR; DROP TABLE DBINE.XDARR2").await;
}

/// A load on a single-threaded runtime refuses to start (a cancel there
/// couldn't wait for a commit the server already has) and writes nothing.
#[tokio::test]
#[ignore]
async fn phoenix_transfer_single_thread_runtime() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE SCHEMA IF NOT EXISTS DBINE; DROP TABLE IF EXISTS DBINE.XST; CREATE TABLE DBINE.XST (ID BIGINT NOT NULL PRIMARY KEY)").await;
    let e = s.bulk_load(&load_spec("XST", &["ID"]), &[], &mut batches(vec![vec![Cell::Int(1)]], 10), &|_| {}).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(ref m) if m.contains("multi-hilo")), "{e}");
    assert_eq!(count(&mut s, "XST").await, 0);
    run(&mut s, "DROP TABLE DBINE.XST").await;
}

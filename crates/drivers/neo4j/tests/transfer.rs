//! Bulk load and typed read against real servers (see `integration.rs` for
//! the containers):
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!  cargo test -p dbine-driver-neo4j --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Counts the live heap and its peak, for the memory rule (2.6: about
/// 32 MiB in flight per table).
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The live-server tests, one at a time (the heap count is per process).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ROWS: usize = 100_000;

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

async fn open(id: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.supports_bulk_load());
    d.connect(&cfg(id, url), None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 10, &mut out).await {
        panic!("{text}: {e}");
    }
    if let Some(e) = &out.error {
        panic!("{text}: {e}");
    }
    out
}

async fn count(s: &mut Box<dyn Session>, label: &str) -> i64 {
    let out = run(s, &format!("MATCH (n:{label}) RETURN count(n) AS c")).await;
    out.results[0].rows[0][0].as_i64().unwrap()
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

const NAMES: [&str; 12] = ["id", "n", "d", "f", "s", "b", "dt", "tm", "ldt", "tz", "u", "l"];

fn row(i: usize, bytes: bool) -> Vec<Cell> {
    let i64_ = i as i64;
    vec![
        Cell::Int(i64_),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Int(i64_ * 1_000_003) },
        Cell::Decimal(format!("{}.{:02}", i64_ - 50_000, i % 100)),
        Cell::Float(i as f64 / 8.0),
        Cell::Text(format!("fila {i} ñ")),
        if bytes { Cell::Bytes(vec![(i % 256) as u8; i % 40 + 1]) } else { Cell::Null },
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::Time(format!("{:02}:{:02}:{:02}.5", i % 24, i % 60, i % 60)),
        Cell::DateTime(format!("2024-01-{:02} 10:00:{:02}", i % 28 + 1, i % 60)),
        Cell::DateTimeTz(format!("2024-01-{:02} 10:00:00-03:00", i % 28 + 1)),
        Cell::Uuid(format!("00000000-0000-4000-8000-{i:012x}")),
        Cell::Json(format!("[{i},{}]", i + 1)),
    ]
}

/// What the read gives back: no decimal or UUID type in Cypher, text;
/// Memgraph (`bytes` false) keeps byte arrays as `0x…` text.
fn expected(i: usize, bytes: bool) -> Vec<Cell> {
    let mut r = row(i, true);
    for c in [2, 10] {
        if let Cell::Decimal(s) | Cell::Uuid(s) = &r[c] {
            r[c] = Cell::Text(s.clone());
        }
    }
    if let (Cell::Bytes(b), false) = (&r[5], bytes) {
        r[5] = Cell::Text(b.iter().fold(String::from("0x"), |s, x| s + &format!("{x:02X}")));
    }
    r
}

/// Deletes a label's nodes; on Neo4j a window at a time (a small server's
/// transaction memory doesn't hold 100,000 deletions).
async fn clear(s: &mut Box<dyn Session>, id: &str, label: &str) {
    let q = if id == "neo4j" {
        format!("MATCH (n:{label}) CALL (n) {{ DETACH DELETE n }} IN TRANSACTIONS OF 10000 ROWS")
    } else {
        format!("MATCH (n:{label}) DETACH DELETE n")
    };
    run(s, &q).await;
}

async fn transfer(id: &str, url: &str) {
    let mut s = open(id, url).await;
    clear(&mut s, id, "DbineXfer").await;
    // Memgraph has no byte arrays among its property types.
    let bytes = id == "neo4j";
    let load_bytes = true;
    let table = ObjectRef { kind: "label".into(), schema: None, name: "DbineXfer".into() };
    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i, load_bytes)).collect(), bytes: 0 })
        .collect();
    let spec = LoadSpec {
        table: table.clone(),
        columns: NAMES.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("{id}: loaded {loaded} nodes in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);
    // Windows capped at 10,000 rows (and 4 MiB), progress = committed rows.
    let reports = reports.into_inner().unwrap();
    assert!((10..=50).contains(&reports.len()), "{reports:?}");
    assert!(reports.windows(2).all(|w| w[1] > w[0] && w[1] - w[0] <= 10_000), "{reports:?}");
    assert_eq!(reports.last(), Some(&(ROWS as u64)));

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("{id}: read {read} nodes in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, ROWS as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let order: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    println!("{id}: columns {:?}", got.columns.iter().map(|c| format!("{} {}", c.name, c.type_name)).collect::<Vec<_>>());
    let id_col = order.iter().position(|n| *n == "id").unwrap();
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[id_col] {
        Cell::Int(i) => i,
        _ => -1,
    });
    for (i, r) in rows.iter().enumerate() {
        let want = expected(i, bytes);
        for (c, name) in order.iter().enumerate() {
            let w = &want[NAMES.iter().position(|n| n == name).unwrap()];
            assert_eq!(&r[c], w, "row {i}, property {name}");
        }
    }

    // A filtered read, and one that would write is refused.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let f = ReadSpec { table: table.clone(), columns: Some(vec!["id".into()]), filter: Some("n.id < 3".into()) };
    assert_eq!(s.read_batches(&f, sink.clone()).await.unwrap(), 3);
    let w = ReadSpec { table, columns: None, filter: Some("true CREATE (x:Oops)".into()) };
    assert!(s.read_batches(&w, sink).await.is_err());
    clear(&mut s, id, "DbineXfer").await;
}

#[tokio::test]
#[ignore]
async fn transfer_neo4j() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_NEO4J_URL").unwrap_or_else(|_| "neo4j:dbine-test-pass@localhost:17687".into());
    transfer("neo4j", &url).await;
}

#[tokio::test]
#[ignore]
async fn transfer_memgraph() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_MEMGRAPH_URL").unwrap_or_else(|_| "localhost:27687".into());
    transfer("memgraph", &url).await;
}

fn label(name: &str) -> ObjectRef {
    ObjectRef { kind: "label".into(), schema: None, name: name.into() }
}

fn load_spec(name: &str, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table: label(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

async fn load(s: &mut Box<dyn Session>, spec: &LoadSpec, rows: Vec<Vec<Cell>>) -> dbine_driver::Result<(u64, Vec<u64>)> {
    let reports = Mutex::new(Vec::new());
    let n = s.bulk_load(spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|n| reports.lock().unwrap().push(n)).await?;
    Ok((n, reports.into_inner().unwrap()))
}

async fn read(s: &mut Box<dyn Session>, name: &str, columns: Option<Vec<&str>>) -> dbine_driver::Result<Collect> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: label(name), columns: columns.map(|c| c.iter().map(|x| x.to_string()).collect()), filter: None };
    s.read_batches(&spec, sink.clone()).await?;
    let got = std::mem::take(&mut *sink.lock().unwrap());
    Ok(got)
}

/// Hands every row at once, says so, then never ends (the load waits for
/// more until it is dropped).
struct ThenStall(Option<RowBatch>, Arc<tokio::sync::Notify>);

#[dbine_driver::async_trait]
impl BatchSource for ThenStall {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.0.take() {
            Some(b) => {
                self.1.notify_one();
                Some(b)
            }
            None => std::future::pending().await,
        }
    }
}

/// The rules of `docs/bulk-transfer.md` 2.5–2.6 (review round 1).
async fn rules(id: &str, url: &str) {
    let mut s = open(id, url).await;
    let mut other = open(id, url).await;
    for l in ["RevCancel", "RevWide", "RevNull", "RevCol", "RevDate", "RevJson", "RevTyped"] {
        run(&mut s, &format!("MATCH (n:{l}) DETACH DELETE n")).await;
    }

    // 1. A load dropped with a window in flight commits nothing afterwards,
    // whether its session is kept (then reused) or dropped.
    for keep in [true, false] {
        let rows: Vec<Vec<Cell>> = (0..10_000).map(|i| vec![Cell::Int(i), Cell::Text(format!("{i:0>200}"))]).collect();
        let spec = load_spec("RevCancel", &["id", "t"]);
        let handed = Arc::new(tokio::sync::Notify::new());
        let mut src = ThenStall(Some(RowBatch { rows, bytes: 0 }), handed.clone());
        let reports = Mutex::new(Vec::<u64>::new());
        {
            let on_progress = |n| reports.lock().unwrap().push(n);
            let fut = s.bulk_load(&spec, &[], &mut src, &on_progress);
            tokio::pin!(fut);
            tokio::select! {
                r = &mut fut => panic!("the load ended: {r:?}"),
                _ = async { handed.notified().await; tokio::time::sleep(std::time::Duration::from_millis(5)).await } => {}
            }
            // Dropped here, with the window's statement in flight.
        }
        let reported = reports.into_inner().unwrap().last().copied().unwrap_or(0);
        if !keep {
            s = open(id, url).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert_eq!(count(&mut other, "RevCancel").await as u64, reported, "{id}: late commit (keep session {keep})");
        // The kept session still works (its connection is replaced).
        assert_eq!(count(&mut s, "RevCancel").await as u64, reported);
        run(&mut s, "MATCH (n:RevCancel) DETACH DELETE n").await;
    }

    // 2. Windows close by bytes too: 1 MiB rows go a few per window, and
    // commit_bytes is honored.
    let wide: Vec<Vec<Cell>> = (0..24).map(|i| vec![Cell::Int(i), Cell::Text("x".repeat(1024 * 1024))]).collect();
    let (n, reports) = load(&mut s, &load_spec("RevWide", &["id", "t"]), wide).await.unwrap();
    assert_eq!(n, 24);
    assert!(reports.len() >= 3, "{id}: {reports:?}");
    assert!(reports.windows(2).all(|w| w[1] - w[0] <= 8), "{id}: {reports:?}");
    run(&mut s, "MATCH (n:RevWide) DETACH DELETE n").await;
    let mut spec = load_spec("RevWide", &["id"]);
    spec.commit_bytes = 1_000;
    let (_, reports) = load(&mut s, &spec, (0..100).map(|i| vec![Cell::Int(i)]).collect()).await.unwrap();
    assert!(reports.len() >= 5, "{id}: {reports:?}");
    assert_eq!(count(&mut s, "RevWide").await, 100);
    run(&mut s, "MATCH (n:RevWide) DETACH DELETE n").await;

    // 3. Nodes without properties are read (rows without cells).
    let (n, _) = load(&mut s, &load_spec("RevNull", &["a"]), vec![vec![Cell::Null]; 3]).await.unwrap();
    assert_eq!(n, 3);
    let got = read(&mut s, "RevNull", None).await.unwrap();
    assert!(got.columns.is_empty());
    assert_eq!(got.rows, vec![Vec::<Cell>::new(); 3]);
    // Mixed: the node without properties comes as nulls.
    load(&mut s, &load_spec("RevNull", &["a"]), vec![vec![Cell::Int(1)]]).await.unwrap();
    let got = read(&mut s, "RevNull", None).await.unwrap();
    assert_eq!(got.rows.len(), 4);
    assert_eq!(got.rows.iter().filter(|r| r[0] == Cell::Null).count(), 3);

    // 4. An unknown column is an error, not a column of nulls.
    load(&mut s, &load_spec("RevCol", &["a", "b"]), vec![vec![Cell::Int(1), Cell::Int(2)]]).await.unwrap();
    assert!(read(&mut s, "RevCol", Some(vec!["nope"])).await.is_err());
    let got = read(&mut s, "RevCol", Some(vec!["b", "a"])).await.unwrap();
    assert_eq!(got.rows, vec![vec![Cell::Int(2), Cell::Int(1)]]);

    // 5. Dates: signed years kept (Neo4j) or refused (Memgraph: 0–9999);
    // trailing text refused.
    let spec = load_spec("RevDate", &["d"]);
    let r = load(&mut s, &spec, vec![vec![Cell::Date("-0044-03-15".into())]]).await;
    if id == "memgraph" {
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
        assert_eq!(count(&mut s, "RevDate").await, 0);
    } else {
        r.unwrap();
        assert_eq!(read(&mut s, "RevDate", None).await.unwrap().rows, vec![vec![Cell::Date("-0044-03-15".into())]]);
    }
    assert!(load(&mut s, &spec, vec![vec![Cell::Date("2024-01-31garbage".into())]]).await.is_err());

    // 6. JSON numbers that don't fit a list without loss stay exact text.
    let js = ["[18446744073709551615]", "[123456789012345678901234567890]"];
    load(&mut s, &load_spec("RevJson", &["i", "j"]), js.iter().enumerate().map(|(i, j)| vec![Cell::Int(i as i64), Cell::Json(j.to_string())]).collect())
        .await
        .unwrap();
    let mut got = read(&mut s, "RevJson", Some(vec!["i", "j"])).await.unwrap().rows;
    got.sort_by_key(|r| if let Cell::Int(i) = r[0] { i } else { -1 });
    assert_eq!(got.iter().map(|r| r[1].clone()).collect::<Vec<_>>(), js.iter().map(|j| Cell::Text(j.to_string())).collect::<Vec<_>>());

    // 7. Text and decimals stay text whatever type the label already has.
    run(&mut s, "CREATE (:RevTyped {i: 1, f: 1.5, b: true})").await;
    let cells = vec![Cell::Text("007".into()), Cell::Decimal("0.30000000000000000001".into()), Cell::Text("TRUE".into())];
    load(&mut s, &load_spec("RevTyped", &["i", "f", "b"]), vec![cells.clone()]).await.unwrap();
    let got = read(&mut s, "RevTyped", Some(vec!["i", "f", "b"])).await.unwrap().rows;
    let want: Vec<Cell> = cells.iter().map(|c| if let Cell::Decimal(d) = c { Cell::Text(d.clone()) } else { c.clone() }).collect();
    assert!(got.contains(&want), "{id}: {got:?}");

    for l in ["RevCancel", "RevWide", "RevNull", "RevCol", "RevDate", "RevJson", "RevTyped"] {
        run(&mut s, &format!("MATCH (n:{l}) DETACH DELETE n")).await;
    }
}

#[tokio::test]
#[ignore]
async fn transfer_rules_neo4j() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_NEO4J_URL").unwrap_or_else(|_| "neo4j:dbine-test-pass@localhost:17687".into());
    rules("neo4j", &url).await;
}

#[tokio::test]
#[ignore]
async fn transfer_rules_memgraph() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_MEMGRAPH_URL").unwrap_or_else(|_| "localhost:27687".into());
    rules("memgraph", &url).await;
}

/// `batches` batches of 10 rows of 200 KiB, made as they are asked for.
struct Wide(usize, usize);

#[dbine_driver::async_trait]
impl BatchSource for Wide {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.0 == self.1 {
            return None;
        }
        self.0 += 1;
        let base = self.0 * 10;
        Some(RowBatch { rows: (0..10).map(|i| vec![Cell::Int((base + i) as i64), Cell::Text("x".repeat(200 * 1024))]).collect(), bytes: 0 })
    }
}

/// Review round 2: the heap in flight, bad keys and leap seconds, and the
/// locks of a dropped load.
async fn rules2(id: &str, url: &str) {
    let mut s = open(id, url).await;
    let mut other = open(id, url).await;
    for l in ["RevMem", "RevKeys", "RevLock"] {
        run(&mut s, &format!("MATCH (n:{l}) DETACH DELETE n")).await;
    }

    // 1. Peak live heap of a load of 200 KiB rows, 10 per batch: bounded by
    // the window, not by the input (40 MiB here), and under 32 MiB.
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let n = s.bulk_load(&load_spec("RevMem", &["id", "t"]), &[], &mut Wide(0, 20), &|_| {}).await.unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    println!("{id}: peak heap in flight {:.1} MiB", peak as f64 / 1048576.0);
    assert_eq!(n, 200);
    assert!(peak < 24 * 1024 * 1024, "{id}: {peak} bytes");
    run(&mut s, "MATCH (n:RevMem) DETACH DELETE n").await;

    // 2. Repeated or empty column names, and leap seconds: refused before
    // anything is sent, the connection still usable.
    let one = vec![vec![Cell::Int(1), Cell::Int(2)]];
    let r = load(&mut s, &load_spec("RevKeys", &["a", "a"]), one.clone()).await;
    assert!(matches!(r, Err(dbine_driver::Error::Query(_))), "{r:?}");
    let r = load(&mut s, &load_spec("RevKeys", &["a", ""]), one).await;
    assert!(matches!(r, Err(dbine_driver::Error::Query(_))), "{r:?}");
    for c in [Cell::Time("23:59:60".into()), Cell::DateTime("2016-12-31 23:59:60".into()), Cell::DateTimeTz("2016-12-31 23:59:60+00:00".into())] {
        let r = load(&mut s, &load_spec("RevKeys", &["t"]), vec![vec![c.clone()]]).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{c:?}: {r:?}");
    }
    assert_eq!(count(&mut s, "RevKeys").await, 0);

    // 3. A load dropped with its window in flight, its session kept: its
    // transaction ends right then, so schema changes don't wait on it.
    let rows: Vec<Vec<Cell>> = (0..10_000).map(|i| vec![Cell::Int(i), Cell::Text(format!("{i:0>200}"))]).collect();
    let handed = Arc::new(tokio::sync::Notify::new());
    let mut src = ThenStall(Some(RowBatch { rows, bytes: 0 }), handed.clone());
    let reports = Mutex::new(Vec::<u64>::new());
    {
        let spec = load_spec("RevLock", &["id", "t"]);
        let on_progress = |n| reports.lock().unwrap().push(n);
        let fut = s.bulk_load(&spec, &[], &mut src, &on_progress);
        tokio::pin!(fut);
        tokio::select! {
            r = &mut fut => panic!("the load ended: {r:?}"),
            _ = async { handed.notified().await; tokio::time::sleep(Duration::from_millis(5)).await } => {}
        }
    }
    let reported = reports.into_inner().unwrap().last().copied().unwrap_or(0);
    println!("{id}: dropped after {reported} committed rows");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    let create = if id == "memgraph" { "CREATE INDEX ON :RevLock(id)" } else { "CREATE INDEX rev_lock_id IF NOT EXISTS FOR (n:RevLock) ON (n.id)" };
    tokio::time::timeout(Duration::from_secs(10), run(&mut other, create)).await.expect("the index waited on the dropped load");
    println!("{id}: index created {:.2}s after the drop", t.elapsed().as_secs_f64());
    assert_eq!(count(&mut other, "RevLock").await as u64, reported);
    run(&mut other, if id == "memgraph" { "DROP INDEX ON :RevLock(id)" } else { "DROP INDEX rev_lock_id IF EXISTS" }).await;
    // The session that dropped it still works.
    assert_eq!(count(&mut s, "RevLock").await as u64, reported);
    run(&mut s, "MATCH (n:RevLock) DETACH DELETE n").await;
    drop(s);
}

#[tokio::test]
#[ignore]
async fn transfer_rules2_neo4j() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_NEO4J_URL").unwrap_or_else(|_| "neo4j:dbine-test-pass@localhost:17687".into());
    rules2("neo4j", &url).await;
}

#[tokio::test]
#[ignore]
async fn transfer_rules2_memgraph() {
    let _one = SERIAL.lock().await;
    let url = std::env::var("DBINE_TEST_MEMGRAPH_URL").unwrap_or_else(|_| "localhost:27687".into());
    rules2("memgraph", &url).await;
}

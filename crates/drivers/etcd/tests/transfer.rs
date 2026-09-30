//! Bulk load and batched read against a real server (the `dbine-test-etcd`
//! container, or `DBINE_TEST_ETCD_URL`, default localhost:25379):
//! `cargo test -p dbine-driver-etcd --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;

/// The heap in use (every thread), and its highest mark since reset: what
/// a load or a read holds, without the allocator's retained pages that
/// resident memory also counts.
struct Counting;
static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static TOP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc(l) };
        if !p.is_null() {
            TOP.fetch_max(LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc_zeroed(l) };
        if !p.is_null() {
            TOP.fetch_max(LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        let q = unsafe { std::alloc::System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                TOP.fetch_max(LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + new - l.size(), Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The tests of this file run one at a time: memory is measured for the
/// whole process, and another test's rows would count as the measured one's.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn open() -> Box<dyn Session> {
    let url = std::env::var("DBINE_TEST_ETCD_URL").unwrap_or_else(|_| "localhost:25379".into());
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "etcd".into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() };
    let d = dbine_driver_etcd::drivers().remove(0);
    assert!(d.supports_bulk_load());
    d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("etcd en {url}: {e}"))
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
    batches: usize,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        assert!(b.len() <= dbine_driver::transfer::CHUNK_ROWS);
        self.batches += 1;
        self.rows.extend(b.rows);
        Ok(())
    }
}

fn key(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::KEY.into(), schema: None, name: name.into() }
}

async fn read(s: &mut Box<dyn Session>, name: &str, columns: Option<Vec<String>>) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&ReadSpec { table: key(name), columns, filter: None }, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

fn spec(table: &str, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table: key(table),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

fn value(i: usize) -> Cell {
    if i.is_multiple_of(3) {
        Cell::Bytes(vec![0xff, (i % 256) as u8, 0x80])
    } else {
        Cell::Text(format!("valor {i} ñ"))
    }
}

#[tokio::test]
#[ignore]
async fn transfer_etcd() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let prefix = format!("/dbine-transfer-{}/", std::process::id());

    // key / value rows, as browsing gives them.
    let rows: Vec<Vec<Cell>> = (0..ROWS).map(|i| vec![Cell::Text(format!("{prefix}{i:06}")), value(i)]).collect();
    let batches: Vec<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    let calls = AtomicU64::new(0);
    let last = AtomicU64::new(0);
    let progress = |n: u64| {
        calls.fetch_add(1, Ordering::SeqCst);
        last.store(n, Ordering::SeqCst);
    };
    let t = Instant::now();
    let loaded = s.bulk_load(&spec(&prefix, &["key", "value"]), &[], &mut Batches(batches.into_iter()), &progress).await.unwrap();
    let load_secs = t.elapsed().as_secs_f64();
    assert_eq!(loaded as usize, ROWS);
    assert_eq!(last.load(Ordering::SeqCst) as usize, ROWS);
    assert!(calls.load(Ordering::SeqCst) >= 10);

    let t = Instant::now();
    let got = read(&mut s, &prefix, None).await;
    let read_secs = t.elapsed().as_secs_f64();
    assert_eq!(
        got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["key", "value", "create_revision", "mod_revision", "version", "lease"]
    );
    assert_eq!(got.rows.len(), ROWS);
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(&r[..2], &rows[i][..], "fila {i}");
        assert_eq!(r[4], Cell::Int(1));
        assert_eq!(r[5], Cell::Null);
    }
    println!(
        "etcd: bulk_load {ROWS} filas en {load_secs:.2} s ({:.0} filas/s); read_batches {read_secs:.2} s ({:.0} filas/s, {} lotes)",
        ROWS as f64 / load_secs,
        ROWS as f64 / read_secs,
        got.batches
    );

    // Some columns, in another order; one exact key.
    let some = read(&mut s, &format!("{prefix}000001"), Some(vec!["value".into(), "key".into()])).await;
    assert_eq!(some.rows, vec![vec![value(1), Cell::Text(format!("{prefix}000001"))]]);

    // Other rows: JSON objects under `<target>/<first column>`, read back
    // by the target's name.
    let table = format!("{prefix}json");
    let jrows = vec![
        vec![Cell::Int(1), Cell::Text("Ana".into()), Cell::Decimal("1.50".into())],
        vec![Cell::Int(2), Cell::Null, Cell::Bool(true)],
    ];
    let n = s
        .bulk_load(&spec(&table, &["id", "nombre", "n"]), &[], &mut Batches(vec![RowBatch { rows: jrows, bytes: 0 }].into_iter()), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 2);
    let j = read(&mut s, &table, Some(vec!["key".into(), "value".into()])).await;
    assert_eq!(
        j.rows,
        vec![
            vec![Cell::Text(format!("{table}/1")), Cell::Text("{\"id\":1,\"nombre\":\"Ana\",\"n\":\"1.50\"}".into())],
            vec![Cell::Text(format!("{table}/2")), Cell::Text("{\"id\":2,\"nombre\":null,\"n\":true}".into())],
        ]
    );

    run(&mut s, &format!("del {prefix} --prefix")).await;
    assert!(read(&mut s, &prefix, None).await.rows.is_empty());
}

/// Key / value rows under `prefix`, `size`-byte values, forever (or up to
/// `limit` rows), `per` rows a batch.
struct Endless {
    prefix: String,
    next: usize,
    limit: usize,
    per: usize,
    size: usize,
}

#[dbine_driver::async_trait]
impl BatchSource for Endless {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.next >= self.limit {
            return None;
        }
        let end = (self.next + self.per).min(self.limit);
        let rows = (self.next..end).map(|i| vec![Cell::Text(format!("{}{i:08}", self.prefix)), Cell::Bytes(vec![(i % 251) as u8; self.size])]).collect();
        self.next = end;
        tokio::task::yield_now().await;
        Some(RowBatch { rows, bytes: 0 })
    }
}

fn endless(prefix: &str, size: usize) -> Endless {
    Endless { prefix: prefix.into(), next: 0, limit: usize::MAX, per: 64, size }
}

async fn count(s: &mut Box<dyn Session>, prefix: &str) -> usize {
    read(s, prefix, Some(vec!["key".into()])).await.rows.len()
}

async fn clean(s: &mut Box<dyn Session>, prefix: &str) {
    run(s, &format!("del {prefix} --prefix")).await;
}

/// Nothing commits after a load returns or is dropped, however it ends;
/// and progress got every committed row by then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_etcd_no_late_commits() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let base = format!("/dbine-transfer-late-{}/", std::process::id());

    // Dropped (the orchestrator drops the writer when the read fails).
    for round in 0..4 {
        let p = format!("{base}drop{round}/");
        let (sp, mut src) = (spec(&p, &["key", "value"]), endless(&p, 100));
        let fut = s.bulk_load(&sp, &[], &mut src, &|_| {});
        assert!(tokio::time::timeout(std::time::Duration::from_millis(150 + 50 * round), fut).await.is_err());
        let now = count(&mut s, &p).await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(count(&mut s, &p).await, now, "filas después de soltar la carga ({p})");
    }

    // Cancelled: progress has every committed row.
    let p = format!("{base}cancel/");
    let stop = s.interrupter().unwrap();
    let last = Arc::new(AtomicU64::new(0));
    let l = last.clone();
    let progress = move |n: u64| l.store(n, Ordering::SeqCst);
    let mut sp = spec(&p, &["key", "value"]);
    sp.commit_rows = 1000;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop();
    });
    let r = s.bulk_load(&sp, &[], &mut endless(&p, 100), &progress).await;
    assert!(matches!(r, Err(dbine_driver::Error::Cancelled)), "{r:?}");
    let now = count(&mut s, &p).await;
    assert!(now > 0);
    assert_eq!(last.load(Ordering::SeqCst) as usize, now, "progreso = filas confirmadas");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(count(&mut s, &p).await, now);

    // Failed (a key that already exists, after many rows).
    let p = format!("{base}fail/");
    run(&mut s, &format!("put {p}00005000 viejo")).await;
    let last = AtomicU64::new(0);
    let progress = |n: u64| last.store(n, Ordering::SeqCst);
    let mut src = endless(&p, 100);
    src.limit = 20_000;
    let mut sp = spec(&p, &["key", "value"]);
    sp.commit_rows = 1000;
    let r = s.bulk_load(&sp, &[], &mut src, &progress).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("ya existe")), "{r:?}");
    let now = count(&mut s, &p).await;
    assert_eq!(last.load(Ordering::SeqCst) as usize + 1, now, "progreso = filas confirmadas (más la que ya estaba)");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(count(&mut s, &p).await, now);
    let old = read(&mut s, &format!("{p}00005000"), Some(vec!["value".into()])).await;
    assert_eq!(old.rows, vec![vec![Cell::Text("viejo".into())]], "no pisa claves existentes");

    clean(&mut s, &base).await;
}

/// Rows read from one prefix and loaded into another stay inside the
/// target; the source is never written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_etcd_stays_in_the_target() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let base = format!("/dbine-transfer-out-{}/", std::process::id());
    let (src, dst) = (format!("{base}src/"), format!("{base}dst/"));
    for i in 0..3 {
        run(&mut s, &format!("put {src}{i} v{i}")).await;
    }
    let before = read(&mut s, &src, None).await;
    let batch = RowBatch { rows: before.rows.clone(), bytes: 0 };
    let all = ["key", "value", "create_revision", "mod_revision", "version", "lease"];
    let n = s.bulk_load(&spec(&dst, &all), &[], &mut Batches(vec![batch.clone()].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(n, 3);
    assert_eq!(read(&mut s, &src, None).await.rows, before.rows, "el origen no cambia");
    let got = read(&mut s, &dst, Some(vec!["key".into(), "value".into()])).await;
    assert_eq!(
        got.rows,
        (0..3).map(|i| vec![Cell::Text(format!("{dst}{}src/{i}", base.trim_start_matches('/'))), Cell::Text(format!("v{i}"))]).collect::<Vec<_>>()
    );
    // Into the source itself: an error, nothing written.
    let r = s.bulk_load(&spec(&src, &all), &[], &mut Batches(vec![batch].into_iter()), &|_| {}).await;
    assert!(r.is_err());
    assert_eq!(read(&mut s, &src, None).await.rows, before.rows, "el origen no cambia");
    clean(&mut s, &base).await;
}

/// Rows whose key repeats (or is null) fail the load clearly; values keep
/// their type; columns beyond key and value aren't dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_etcd_keys_and_values() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let base = format!("/dbine-transfer-keys-{}/", std::process::id());
    let load = |rows: Vec<Vec<Cell>>| Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());

    // Repeated first column in one transaction, and across transactions.
    let t = format!("{base}same");
    let rows = (0..10).map(|i| vec![Cell::Text(if i % 2 == 0 { "a" } else { "b" }.into()), Cell::Int(i)]).collect();
    let r = s.bulk_load(&spec(&t, &["k", "n"]), &[], &mut load(rows), &|_| {}).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("se repite")), "{r:?}");
    let t = format!("{base}across");
    let rows = (0..256).map(|i| vec![Cell::Int(i % 128), Cell::Int(i)]).collect();
    let r = s.bulk_load(&spec(&t, &["k", "n"]), &[], &mut load(rows), &|_| {}).await;
    assert!(r.is_err(), "{r:?}");
    let t = format!("{base}types");
    let r = s.bulk_load(&spec(&t, &["k"]), &[], &mut load(vec![vec![Cell::Int(1)], vec![Cell::Text("1".into())]]), &|_| {}).await;
    assert!(r.is_err(), "{r:?}");
    let t = format!("{base}null");
    let r = s.bulk_load(&spec(&t, &["k", "n"]), &[], &mut load(vec![vec![Cell::Null, Cell::Int(1)]]), &|_| {}).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("nula")), "{r:?}");
    let t = format!("{base}kvdup/");
    let rows = vec![vec![Cell::Text(format!("{t}x")), Cell::Text("1".into())], vec![Cell::Text(format!("{t}x")), Cell::Text("2".into())]];
    assert!(s.bulk_load(&spec(&t, &["key", "value"]), &[], &mut load(rows), &|_| {}).await.is_err());
    let rows = vec![vec![Cell::Text(format!("{t}n")), Cell::Null]];
    let r = s.bulk_load(&spec(&t, &["key", "value"]), &[], &mut load(rows), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");

    // Types kept; every column kept.
    let t = format!("{base}typed");
    let rows = vec![vec![Cell::Int(1), Cell::Bytes(vec![0xAB]), Cell::Text("0xAB".into())]];
    s.bulk_load(&spec(&t, &["id", "b", "t"]), &[], &mut load(rows), &|_| {}).await.unwrap();
    let v = read(&mut s, &format!("{t}/1"), Some(vec!["value".into()])).await;
    assert_eq!(v.rows, vec![vec![Cell::Text(r#"{"id":1,"b":{"$binary":{"base64":"qw==","subType":"00"}},"t":"0xAB"}"#.into())]]);
    let t = format!("{base}config");
    let rows = vec![vec![Cell::Text("a".into()), Cell::Text("1".into()), Cell::DateTime("2024-01-02 03:04:05".into()), Cell::Text("ana".into())]];
    s.bulk_load(&spec(&t, &["key", "value", "updated_at", "owner"]), &[], &mut load(rows), &|_| {}).await.unwrap();
    let v = read(&mut s, &format!("{t}/a"), Some(vec!["value".into()])).await;
    assert_eq!(v.rows, vec![vec![Cell::Text(r#"{"key":"a","value":"1","updated_at":"2024-01-02 03:04:05","owner":"ana"}"#.into())]]);

    clean(&mut s, &base).await;
}

/// This process's resident memory, in bytes.
fn rss() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0) * 1024
}

/// What `f` held at most while it ran: the heap's growth (the budget's
/// measure) and resident memory's (which also counts the allocator's
/// retained pages).
struct Held {
    heap: u64,
    rss: u64,
}

async fn peak<T>(f: impl std::future::Future<Output = T>) -> (T, Held) {
    let start = rss();
    let live = LIVE.load(Ordering::SeqCst);
    TOP.store(live, Ordering::SeqCst);
    let top = Arc::new(AtomicU64::new(start));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (t, st) = (top.clone(), stop.clone());
    let watcher = std::thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            t.fetch_max(rss(), Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let r = f.await;
    stop.store(true, Ordering::SeqCst);
    watcher.join().unwrap();
    let heap = TOP.load(Ordering::SeqCst).saturating_sub(live) as u64;
    (r, Held { heap, rss: top.load(Ordering::SeqCst).saturating_sub(start) })
}

struct Discard(usize);

impl BatchSink for Discard {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0 += b.len();
        Ok(())
    }
}

/// A table's memory budget (docs/transferencia-masiva.md).
const BUDGET: u64 = 32 << 20;

/// Loading and reading wide values stays within a table's memory budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_etcd_memory() {
    let _serial = SERIAL.lock().await;
    const N: usize = 240;
    let mut s = open().await;
    let p = format!("/dbine-transfer-mem-{}/", std::process::id());
    let mut src = Endless { prefix: p.clone(), next: 0, limit: N, per: 20, size: 100 * 1024 };
    let (n, held) = peak(s.bulk_load(&spec(&p, &["key", "value"]), &[], &mut src, &|_| {})).await;
    assert_eq!(n.unwrap() as usize, N);
    println!("etcd: carga de {N} valores de 100 KiB, heap +{} MiB, RSS +{} MiB", held.heap >> 20, held.rss >> 20);
    assert!(held.heap < BUDGET, "carga: heap +{} MiB", held.heap >> 20);
    let sink = Arc::new(Mutex::new(Discard(0)));
    let (n, held) = peak(s.read_batches(&ReadSpec { table: key(&p), columns: None, filter: None }, sink.clone())).await;
    assert_eq!(n.unwrap() as usize, N);
    assert_eq!(sink.lock().unwrap().0, N);
    println!("etcd: lectura de {N} valores de 100 KiB, heap +{} MiB, RSS +{} MiB", held.heap >> 20, held.rss >> 20);
    assert!(held.heap < BUDGET, "lectura: heap +{} MiB", held.heap >> 20);
    clean(&mut s, &p).await;
}

/// Small values and then large ones: the page after the small ones asks
/// for many keys and gets 32 MiB in one reply; the read still holds one
/// key-value at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_etcd_memory_small_then_large() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let p = format!("/dbine-transfer-mem2-{}/", std::process::id());
    let mut small = Endless { prefix: format!("{p}a/"), next: 0, limit: 112, per: 112, size: 10 };
    s.bulk_load(&spec(&p, &["key", "value"]), &[], &mut small, &|_| {}).await.unwrap();
    let mut large = Endless { prefix: format!("{p}b/"), next: 0, limit: 128, per: 8, size: 256 * 1024 };
    let (n, held) = peak(s.bulk_load(&spec(&p, &["key", "value"]), &[], &mut large, &|_| {})).await;
    assert_eq!(n.unwrap(), 128);
    println!("etcd: carga de 128 valores de 256 KiB, heap +{} MiB, RSS +{} MiB", held.heap >> 20, held.rss >> 20);
    assert!(held.heap < BUDGET, "carga: heap +{} MiB", held.heap >> 20);
    let sink = Arc::new(Mutex::new(Discard(0)));
    let (n, held) = peak(s.read_batches(&ReadSpec { table: key(&p), columns: None, filter: None }, sink.clone())).await;
    assert_eq!(n.unwrap(), 240);
    assert_eq!(sink.lock().unwrap().0, 240);
    println!("etcd: lectura de 112 valores chicos y 128 de 256 KiB, heap +{} MiB, RSS +{} MiB", held.heap >> 20, held.rss >> 20);
    assert!(held.heap < BUDGET, "lectura: heap +{} MiB", held.heap >> 20);
    clean(&mut s, &p).await;
}

/// JSON goes as its own text: big integers, long decimals, key order and
/// spacing survive, as a value and nested in a row's object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_etcd_json_lossless() {
    let _serial = SERIAL.lock().await;
    let mut s = open().await;
    let base = format!("/dbine-transfer-json-{}/", std::process::id());
    let doc = r#"{"z":1,"n":123456789012345678901234567890,"d":0.10000000000000000000001, "a":2}"#;
    let load = |rows: Vec<Vec<Cell>>| Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());

    let t = format!("{base}kv/");
    let rows = vec![vec![Cell::Text(format!("{t}doc")), Cell::Json(doc.into())]];
    s.bulk_load(&spec(&t, &["key", "value"]), &[], &mut load(rows), &|_| {}).await.unwrap();
    let v = read(&mut s, &format!("{t}doc"), Some(vec!["value".into()])).await;
    assert_eq!(v.rows, vec![vec![Cell::Text(doc.into())]]);

    let t = format!("{base}rows");
    let rows = vec![vec![Cell::Int(1), Cell::Json(doc.into()), Cell::Float(0.1)]];
    s.bulk_load(&spec(&t, &["id", "doc", "f"]), &[], &mut load(rows), &|_| {}).await.unwrap();
    let v = read(&mut s, &format!("{t}/1"), Some(vec!["value".into()])).await;
    assert_eq!(v.rows, vec![vec![Cell::Text(format!(r#"{{"id":1,"doc":{doc},"f":0.1}}"#))]]);

    // Not JSON: an error, nothing written in its place.
    let t = format!("{base}bad");
    let r = s.bulk_load(&spec(&t, &["id", "doc"]), &[], &mut load(vec![vec![Cell::Int(1), Cell::Json("{\"a\":1} x".into())]]), &|_| {}).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("JSON inválido")), "{r:?}");
    assert!(read(&mut s, &t, None).await.rows.is_empty());

    clean(&mut s, &base).await;
}

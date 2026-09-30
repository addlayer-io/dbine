//! Bulk load and batched read against real servers (the `dbine-test-*`
//! containers):
//! `cargo test -p dbine-driver-redis --release -- --ignored transfer --nocapture`.
//! Addresses: `DBINE_TEST_REDIS_URL` (localhost:25400), `DBINE_TEST_VALKEY_URL`
//! (localhost:25401), `DBINE_TEST_DRAGONFLY_URL` (localhost:25407).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;

async fn open(driver: &str, env: &str, default: &str) -> Box<dyn Session> {
    let url = std::env::var(env).unwrap_or_else(|_| default.into());
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() };
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_bulk_load());
    d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("{driver} en {url}: {e}"))
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 10, &mut out).await {
        panic!("{}: {e}", &text[..text.len().min(80)]);
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

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("{i:06}")),
        Cell::Text(format!("fila {i} ñ")),
        Cell::Int(i as i64 * 3 - 7),
        if i.is_multiple_of(5) { Cell::Null } else { Cell::Bytes(vec![0xff, (i % 256) as u8, 0x80]) },
    ]
}

/// What comes back: Redis keeps strings, so numbers come back as text.
fn expected(i: usize) -> Vec<Cell> {
    let mut r = row(i);
    r[2] = Cell::Text((i as i64 * 3 - 7).to_string());
    r
}

async fn delete_prefix(s: &mut Box<dyn Session>, prefix: &str) {
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(1000) {
        let keys: Vec<String> = chunk.iter().map(|i| format!("{prefix}:{i:06}")).collect();
        run(s, &format!("DEL {}", keys.join(" "))).await;
    }
}

async fn round_trip(driver: &str, env: &str, default: &str) {
    let mut s = open(driver, env, default).await;
    let prefix = format!("dbine:transfer:{}", std::process::id());
    let columns: Vec<String> = ["id", "name", "n", "blob"].iter().map(|c| c.to_string()).collect();

    let rows: Vec<Vec<Cell>> = (0..ROWS).map(row).collect();
    let batches: Vec<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    let spec = LoadSpec {
        table: key(&prefix),
        columns: columns.clone(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let calls = AtomicU64::new(0);
    let last = AtomicU64::new(0);
    let progress = |n: u64| {
        calls.fetch_add(1, Ordering::SeqCst);
        last.store(n, Ordering::SeqCst);
    };
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &progress).await.unwrap();
    let load_secs = t.elapsed().as_secs_f64();
    assert_eq!(loaded as usize, ROWS);
    assert_eq!(last.load(Ordering::SeqCst) as usize, ROWS);
    assert!(calls.load(Ordering::SeqCst) >= 10);

    let t = Instant::now();
    let got = read(&mut s, &prefix, Some(columns.clone())).await;
    let read_secs = t.elapsed().as_secs_f64();
    assert_eq!(got.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), columns);
    let mut back = got.rows;
    back.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    assert_eq!(back.len(), ROWS);
    for (i, r) in back.iter().enumerate() {
        assert_eq!(r, &expected(i), "fila {i}");
    }
    // Without columns: the fields found.
    let auto = read(&mut s, &prefix, None).await;
    let mut names: Vec<String> = auto.columns.iter().map(|c| c.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["blob", "id", "n", "name"]);
    assert_eq!(auto.rows.len(), ROWS);
    println!(
        "{driver}: bulk_load {ROWS} filas en {load_secs:.2} s ({:.0} filas/s); read_batches {read_secs:.2} s ({:.0} filas/s, {} lotes)",
        ROWS as f64 / load_secs,
        ROWS as f64 / read_secs,
        got.batches
    );

    // A row without a key name is refused, as in the insert script.
    let bad = vec![RowBatch { rows: vec![vec![Cell::Null, Cell::Text("x".into())]], bytes: 0 }];
    assert!(s.bulk_load(&spec, &[], &mut Batches(bad.into_iter()), &|_| {}).await.is_err());
    delete_prefix(&mut s, &prefix).await;

    // Keys of each type, read in pages as browsing shows them.
    let k = |t: &str| format!("{prefix}:{t}");
    let n = 2_500;
    let mut script = String::new();
    for c in (0..n).collect::<Vec<_>>().chunks(500) {
        let h: Vec<String> = c.iter().map(|i| format!("f{i} v{i}")).collect();
        let l: Vec<String> = c.iter().map(|i| format!("e{i}")).collect();
        let z: Vec<String> = c.iter().map(|i| format!("{i}.5 m{i}")).collect();
        script.push_str(&format!("HSET {} {}\nRPUSH {} {}\nSADD {} {}\nZADD {} {}\n", k("h"), h.join(" "), k("l"), l.join(" "), k("s"), l.join(" "), k("z"), z.join(" ")));
    }
    for i in 0..n {
        script.push_str(&format!("XADD {} * a {i}{}\n", k("x"), if i % 2 == 0 { " b x".to_string() } else { String::new() }));
    }
    script.push_str(&format!("SET {} \"hola mundo\"\n", k("str")));
    run(&mut s, &script).await;

    let h = read(&mut s, &k("h"), None).await;
    assert_eq!(h.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["field", "value"]);
    assert_eq!(h.rows.len(), n);
    assert!(h.rows.contains(&vec![Cell::Text("f7".into()), Cell::Text("v7".into())]));
    let l = read(&mut s, &k("l"), None).await;
    assert_eq!(l.rows, (0..n).map(|i| vec![Cell::Text(format!("e{i}"))]).collect::<Vec<_>>());
    assert_eq!(read(&mut s, &k("s"), None).await.rows.len(), n);
    let z = read(&mut s, &k("z"), Some(vec!["score".into(), "member".into()])).await;
    assert_eq!(z.rows.len(), n);
    assert!(z.rows.contains(&vec![Cell::Float(7.5), Cell::Text("m7".into())]));
    let x = read(&mut s, &k("x"), None).await;
    assert_eq!(x.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "a", "b"]);
    assert_eq!(x.rows.len(), n);
    assert_eq!(x.rows[n - 1][1], Cell::Text((n - 1).to_string()));
    assert_eq!(x.rows[1][2], Cell::Null);
    let st = read(&mut s, &k("str"), None).await;
    assert_eq!(st.rows, vec![vec![Cell::Text("hola mundo".into())]]);
    run(&mut s, &format!("DEL {} {} {} {} {} {}", k("h"), k("l"), k("s"), k("z"), k("x"), k("str"))).await;
}

#[tokio::test]
#[ignore]
async fn transfer_redis() {
    round_trip("redis", "DBINE_TEST_REDIS_URL", "localhost:25400").await;
}

#[tokio::test]
#[ignore]
async fn transfer_valkey() {
    round_trip("valkey", "DBINE_TEST_VALKEY_URL", "localhost:25401").await;
}

#[tokio::test]
#[ignore]
async fn transfer_dragonfly() {
    round_trip("dragonfly", "DBINE_TEST_DRAGONFLY_URL", "localhost:25407").await;
}

fn spec(table: &str, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table: key(table),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 100_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    Batches(rows.chunks(500).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect::<Vec<_>>().into_iter())
}

/// Rows of `table` found by a read.
async fn count(s: &mut Box<dyn Session>, table: &str) -> usize {
    read(s, table, Some(vec!["id".into()])).await.rows.len()
}

async fn del(s: &mut Box<dyn Session>, keys: Vec<String>) {
    for c in keys.chunks(1000) {
        run(s, &format!("DEL {}", c.join(" "))).await;
    }
}

fn wide(n: usize) -> Vec<Vec<Cell>> {
    (0..n).map(|i| vec![Cell::Text(format!("{i:06}")), Cell::Text(format!("{i}-{}", "x".repeat(1000)))]).collect()
}

/// Loads that fail, are dropped or partly fail, and reads of tables whose
/// fields vary from row to row.
async fn edges(driver: &str, env: &str, default: &str) {
    let mut s = open(driver, env, default).await;
    let p = format!("dbine:edge:{}", std::process::id());
    let last = Arc::new(AtomicU64::new(0));
    let l = last.clone();
    let progress = move |n: u64| l.store(n, Ordering::SeqCst);

    // A: a bad row after 5,000: the load fails and nothing is written after it returns.
    let a = format!("{p}:A");
    let mut rows = wide(5_000);
    rows.push(vec![Cell::Null, Cell::Text("x".into())]);
    let e = s.bulk_load(&spec(&a, &["id", "v"]), &[], &mut batches(rows), &progress).await.unwrap_err();
    assert!(e.to_string().contains("5001"), "{e}");
    let at_return = count(&mut s, &a).await;
    assert_eq!(at_return as u64, last.load(Ordering::SeqCst));
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(count(&mut s, &a).await, at_return, "{driver}: filas escritas después de que la carga falló");
    del(&mut s, (0..5_000).map(|i| format!("{a}:{i:06}")).collect()).await;

    // B: a load dropped mid-way (a few ms after its first commit, with
    // the next windows queued or running): nothing is written after it.
    let b = format!("{p}:B");
    last.store(0, Ordering::SeqCst);
    let mut src = batches(wide(20_000));
    let first = Arc::new(tokio::sync::Notify::new());
    let (l, f) = (last.clone(), first.clone());
    let progress_b = move |n: u64| {
        l.store(n, Ordering::SeqCst);
        f.notify_one();
    };
    let spec_b = spec(&b, &["id", "v"]);
    let cancelled = tokio::select! {
        r = s.bulk_load(&spec_b, &[], &mut src, &progress_b) => { r.unwrap(); false }
        _ = async { first.notified().await; tokio::time::sleep(std::time::Duration::from_millis(3)).await } => true,
    };
    let at_drop = count(&mut s, &b).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(count(&mut s, &b).await, at_drop, "{driver}: filas escritas después de cancelar la carga");
    // The drop waits for the EXEC it had running and counts it.
    assert_eq!(at_drop as u64, last.load(Ordering::SeqCst), "{driver}: el progreso no cuenta todas las filas confirmadas");
    println!("{driver}: carga cancelada={cancelled} con {} filas confirmadas: {at_drop} filas escritas, las mismas 1,5 s después", last.load(Ordering::SeqCst));
    del(&mut s, (0..20_000).map(|i| format!("{b}:{i:06}")).collect()).await;

    // B2: dropped at several moments (some while an EXEC runs): the
    // progress is exactly what is written, and nothing is written later.
    for ms in (1..=20u64).map(|i| i * 5) {
        last.store(0, Ordering::SeqCst);
        let mut src = batches(wide(8_000));
        let spec_b = LoadSpec { commit_rows: 1_000, ..spec(&b, &["id", "v"]) };
        tokio::select! {
            r = s.bulk_load(&spec_b, &[], &mut src, &progress) => { r.unwrap(); }
            _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
        };
        let at_drop = count(&mut s, &b).await as u64;
        assert_eq!(at_drop, last.load(Ordering::SeqCst), "{driver}: cancelada a los {ms} ms, el progreso no es lo escrito");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(count(&mut s, &b).await as u64, at_drop, "{driver}: filas escritas después de cancelar a los {ms} ms");
        println!("{driver}: cancelada a los {ms} ms: progreso {at_drop}, filas escritas {at_drop}");
        del(&mut s, (0..8_000).map(|i| format!("{b}:{i:06}")).collect()).await;
    }

    // G: one command of a window fails (WRONGTYPE): the others are written, counted and reported.
    let g = format!("{p}:G");
    last.store(0, Ordering::SeqCst);
    run(&mut s, &format!("SET {g}:000500 texto")).await;
    let e = s.bulk_load(&spec(&g, &["id", "v"]), &[], &mut batches(wide(1_000)), &progress).await.unwrap_err();
    assert!(e.to_string().contains("fila 501") && e.to_string().contains("WRONGTYPE"), "{e}");
    assert_eq!(last.load(Ordering::SeqCst), 999);
    assert_eq!(count(&mut s, &g).await, 999);
    del(&mut s, (0..1_000).map(|i| format!("{g}:{i:06}")).collect()).await;

    // C: JSON byte for byte; an asked-for column that doesn't exist is an error.
    let c = format!("{p}:C");
    let doc = r#"{"b":1,"a":123456789012345678901234567890,"c":1.10}"#;
    s.bulk_load(&spec(&c, &["id", "doc"]), &[], &mut batches(vec![vec![Cell::Text("k1".into()), Cell::Json(doc.into())]]), &|_| {}).await.unwrap();
    let got = read(&mut s, &c, Some(vec!["id".into(), "doc".into()])).await;
    assert_eq!(got.rows, vec![vec![Cell::Text("k1".into()), Cell::Text(doc.into())]]);
    let sink = Arc::new(Mutex::new(Collect::default()));
    let e = s.read_batches(&ReadSpec { table: key(&c), columns: Some(vec!["id".into(), "nope".into()]), filter: None }, sink).await.unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
    del(&mut s, vec![format!("{c}:k1")]).await;

    // D: a field only in the last row is a column (whatever the SCAN order).
    let d = format!("{p}:D");
    let mut rows: Vec<Vec<Cell>> = (0..3_000).map(|i| vec![Cell::Text(format!("{i:06}")), Cell::Int(i as i64), Cell::Null]).collect();
    rows[2_999][2] = Cell::Text("only-here".into());
    s.bulk_load(&spec(&d, &["id", "a", "note"]), &[], &mut batches(rows), &|_| {}).await.unwrap();
    let got = read(&mut s, &d, None).await;
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    let note = names.iter().position(|n| *n == "note").unwrap_or_else(|| panic!("{driver}: falta note en {names:?}"));
    assert_eq!(got.rows.len(), 3_000);
    assert_eq!(got.rows.iter().filter(|r| r[note] == Cell::Text("only-here".into())).count(), 1);
    del(&mut s, (0..3_000).map(|i| format!("{d}:{i:06}")).collect()).await;

    // E: `T:arch` is another table, not rows of `T`; an id with `:` is still a row of `T`.
    let t = format!("{p}:T");
    let arch = format!("{t}:arch");
    let mut rows: Vec<Vec<Cell>> = (0..2_000).map(|i| vec![Cell::Text(format!("{i:06}"))]).collect();
    rows.push(vec![Cell::Text("x:y".into())]);
    s.bulk_load(&spec(&t, &["id"]), &[], &mut batches(rows), &|_| {}).await.unwrap();
    s.bulk_load(&spec(&arch, &["id"]), &[], &mut batches(vec![vec![Cell::Text("1".into())]]), &|_| {}).await.unwrap();
    assert_eq!(count(&mut s, &t).await, 2_001);
    assert_eq!(read(&mut s, &t, None).await.rows.len(), 2_001);
    assert_eq!(count(&mut s, &arch).await, 1);
    // And the other way: a row of `T` with id `arch:5` is not a row of `T:arch`.
    s.bulk_load(&spec(&t, &["id", "v"]), &[], &mut batches(vec![vec![Cell::Text("arch:5".into()), Cell::Text("5".into())]]), &|_| {}).await.unwrap();
    assert_eq!(read(&mut s, &arch, None).await.rows, vec![vec![Cell::Text("1".into())]]);
    assert_eq!(count(&mut s, &t).await, 2_002);
    // A table that was never loaded has no rows.
    let never = format!("{t}:nunca");
    s.bulk_load(&spec(&t, &["id"]), &[], &mut batches(vec![vec![Cell::Text("nunca:7".into())]]), &|_| {}).await.unwrap();
    assert_eq!(read(&mut s, &never, None).await.rows.len(), 0);
    let mut keys: Vec<String> = (0..2_000).map(|i| format!("{t}:{i:06}")).collect();
    keys.extend([format!("{t}:x:y"), format!("{arch}:1"), format!("{arch}:5"), format!("{never}:7")]);
    del(&mut s, keys).await;

    // H: which field is the id doesn't depend on the fields' order, which
    // large hashes don't keep (Dragonfly past ~64-byte values): rows of `T`
    // with ids `a:<i>` and a field holding `<i>` are still rows of `T`.
    let h = format!("{p}:H");
    let rows: Vec<Vec<Cell>> = (0..200).map(|i| vec![Cell::Text(format!("a:{i}")), Cell::Text(i.to_string()), Cell::Text("x".repeat(100))]).collect();
    s.bulk_load(&spec(&h, &["id", "n", "big"]), &[], &mut batches(rows), &|_| {}).await.unwrap();
    assert_eq!(read(&mut s, &h, None).await.rows.len(), 200, "{driver}");
    assert_eq!(read(&mut s, &h, Some(vec!["id".into(), "n".into()])).await.rows.len(), 200, "{driver}");
    assert_eq!(read(&mut s, &format!("{h}:a"), None).await.rows.len(), 0, "{driver}");
    del(&mut s, (0..200).map(|i| format!("{h}:a:{i}")).collect()).await;
    // The other way (a row of `H:arch` holding `arch:5` would read as one of `H`): refused.
    let e = s
        .bulk_load(&spec(&format!("{h}:arch"), &["id", "note"]), &[], &mut batches(vec![vec![Cell::Int(5), Cell::Text("arch:5".into())]]), &|_| {})
        .await
        .unwrap_err();
    assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e:?}");
    assert_eq!(read(&mut s, &format!("{h}:arch"), None).await.rows.len(), 0);

    // F: a stream field that first appears after the first page.
    let f = format!("{p}:F");
    let mut script = String::new();
    for i in 0..1_500 {
        script.push_str(&format!("XADD {f} * a {i}{}\n", if i == 1_400 { " late tarde" } else { "" }));
    }
    run(&mut s, &script).await;
    let got = read(&mut s, &f, None).await;
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "a", "late"]);
    assert_eq!(got.rows[1_400][2], Cell::Text("tarde".into()));
    let got = read(&mut s, &f, Some(vec!["id".into(), "late".into()])).await;
    assert_eq!(got.rows.iter().filter(|r| r[1] != Cell::Null).count(), 1);
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert!(s.read_batches(&ReadSpec { table: key(&f), columns: Some(vec!["id".into(), "nope".into()]), filter: None }, sink).await.is_err());
    del(&mut s, vec![f]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_edges_redis() {
    edges("redis", "DBINE_TEST_REDIS_URL", "localhost:25400").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_edges_valkey() {
    edges("valkey", "DBINE_TEST_VALKEY_URL", "localhost:25401").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_edges_dragonfly() {
    edges("dragonfly", "DBINE_TEST_DRAGONFLY_URL", "localhost:25407").await;
}

/// Counts rows and throws them away.
#[derive(Default)]
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

/// Rows of 1 MiB, made as they are asked for.
struct Mib(usize, usize);

#[dbine_driver::async_trait]
impl BatchSource for Mib {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.0 == self.1 {
            return None;
        }
        self.0 += 1;
        Some(RowBatch { rows: vec![vec![Cell::Text(format!("{:04}", self.0)), Cell::Text("x".repeat(1 << 20))]], bytes: 0 })
    }
}

fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Reading hashes of 1 MiB holds a few at a time, not a whole `SCAN` page:
/// the process grows far less than the table. Run alone (it measures the
/// process): `cargo test -p dbine-driver-redis --release -- --ignored transfer_read_memory --test-threads 1`.
async fn read_memory(driver: &str, env: &str, default: &str) {
    const N: usize = 64;
    let mut s = open(driver, env, default).await;
    let t = format!("dbine:mem:{}", std::process::id());
    s.bulk_load(&spec(&t, &["id", "v"]), &[], &mut Mib(0, N), &|_| {}).await.unwrap();
    let base = rss_kib();
    let peak = Arc::new(AtomicU64::new(base));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (p, st) = (peak.clone(), stop.clone());
    let watch = std::thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            p.fetch_max(rss_kib(), Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });
    for columns in [None, Some(vec!["id".to_string(), "v".into()])] {
        let sink = Arc::new(Mutex::new(Discard::default()));
        let n = s.read_batches(&ReadSpec { table: key(&t), columns, filter: None }, sink.clone()).await.unwrap();
        assert_eq!((n as usize, sink.lock().unwrap().0), (N, N));
    }
    stop.store(true, Ordering::SeqCst);
    watch.join().unwrap();
    let grew = (peak.load(Ordering::SeqCst).saturating_sub(base)) >> 10;
    println!("{driver}: leer {N} filas de 1 MiB hizo crecer el proceso {grew} MiB");
    del(&mut s, (1..=N).map(|i| format!("{t}:{i:04}")).collect()).await;
    assert!(grew < 32, "{driver}: leer {N} MiB hizo crecer el proceso {grew} MiB");
}

#[tokio::test]
#[ignore]
async fn transfer_read_memory_redis() {
    read_memory("redis", "DBINE_TEST_REDIS_URL", "localhost:25400").await;
}

#[tokio::test]
#[ignore]
async fn transfer_read_memory_valkey() {
    read_memory("valkey", "DBINE_TEST_VALKEY_URL", "localhost:25401").await;
}

#[tokio::test]
#[ignore]
async fn transfer_read_memory_dragonfly() {
    read_memory("dragonfly", "DBINE_TEST_DRAGONFLY_URL", "localhost:25407").await;
}

//! Bulk transfer against the Linux emulator (ignored by default; container
//! as in `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef};
use dbine_driver_cosmosdb::auth_header;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
const ROWS: usize = 50_000;
const DB: &str = "dbine_xfer";

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

fn setup() -> (String, String) {
    let url = std::env::var("DBINE_TEST_COSMOSDB_URL").unwrap_or_else(|_| "https://localhost:25213".into());
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    (url.trim_end_matches('/').to_string(), key)
}

/// A raw signed REST call (database and container setup).
async fn rest(url: &str, key: &str, method: &str, rtype: &str, link: &str, path: &str, body: Option<Value>) -> u16 {
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut rq = http
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .header("Authorization", auth_header(key, method, rtype, link, &date).unwrap())
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31");
    if let Some(b) = body {
        rq = rq.header("Content-Type", "application/json").body(b.to_string());
    }
    rq.send().await.unwrap().status().as_u16()
}

const COLS: [&str; 7] = ["id", "cat", "flag", "meta", "n", "name", "price"];

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("d{i:06}")),
        Cell::Text(format!("c{}", i % 50)),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("{{\"k\":{i},\"tags\":[\"a\",\"b\"]}}")),
        Cell::Int(i as i64),
        Cell::Text(format!("name {i} \"q\" 'x'")),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Float(i as f64 + 0.25) },
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_cosmosdb() {
    let (url, key) = setup();
    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{DB}"), &format!("/dbs/{DB}"), None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": DB }))).await, 201);
    let body = json!({ "id": "items", "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
    assert_eq!(rest(&url, &key, "POST", "colls", &format!("dbs/{DB}"), &format!("/dbs/{DB}/colls"), Some(body)).await, 201);

    let driver = &dbine_driver_cosmosdb::drivers()[0];
    assert!(driver.supports_bulk_load());
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url.clone(), database: DB.into(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key.clone());
    let mut s = driver.connect(&c, None).await.expect("connect");

    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let table = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "items".into() };
    let spec = LoadSpec {
        table: table.clone(),
        columns: COLS.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &progress).await.expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    println!("cosmosdb: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    // The same ids again: a create never replaces.
    // A lone item (point create) and two of one partition (a batch, which
    // applies nothing: the new id isn't left behind).
    let again = vec![RowBatch { rows: vec![row(1)], bytes: 0 }];
    let e = s.bulk_load(&spec, &[], &mut Batches(again.into_iter()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("ya existe"), "{e}");
    let mut fresh = row(2);
    fresh[0] = Cell::Text("fresh".into());
    let again = vec![RowBatch { rows: vec![fresh, row(52)], bytes: 0 }];
    let e = s.bulk_load(&spec, &[], &mut Batches(again.into_iter()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("«d000052»"), "{e}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.expect("read");
    let secs = start.elapsed().as_secs_f64();
    println!("cosmosdb: read_batches {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read as usize, ROWS);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "id");
    assert!(names.contains(&"_ts"), "{names:?}");
    let pos: Vec<usize> = COLS.iter().map(|c| names.iter().position(|n| n == c).unwrap_or_else(|| panic!("{c} in {names:?}"))).collect();
    let by_id: HashMap<String, Vec<Cell>> = got
        .rows
        .into_iter()
        .map(|r| {
            let r: Vec<Cell> = pos.iter().map(|p| r[*p].clone()).collect();
            let Cell::Text(id) = &r[0] else { panic!("id {:?}", r[0]) };
            (id.clone(), r)
        })
        .collect();
    assert_eq!(by_id.len(), ROWS);
    for i in 0..ROWS {
        assert_eq!(by_id[&format!("d{i:06}")], row(i), "row {i}");
    }

    // A condition, with asked-for columns.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["id".into(), "n".into()]), filter: Some("c.n < 12".into()) };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 12);
    assert!(sink.lock().unwrap().rows.iter().all(|r| matches!(&r[1], Cell::Int(n) if Cell::Text(format!("d{n:06}")) == r[0])));

    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{DB}"), &format!("/dbs/{DB}"), None).await;
}

/// A raw signed REST call that returns the status and the reply.
#[allow(clippy::too_many_arguments)]
async fn rest_json(url: &str, key: &str, method: &str, rtype: &str, link: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (u16, Value) {
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut rq = http
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .header("Authorization", auth_header(key, method, rtype, link, &date).unwrap())
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31");
    for (k, v) in headers {
        rq = rq.header(*k, *v);
    }
    if let Some(b) = body {
        rq = rq.header("Content-Type", "application/json").body(b.to_string());
    }
    let r = rq.send().await.unwrap();
    let status = r.status().as_u16();
    (status, serde_json::from_str(&r.text().await.unwrap()).unwrap_or(Value::Null))
}

/// Another session on an existing database.
async fn fresh_session(db: &str) -> (String, String, Box<dyn dbine_driver::Session>) {
    let (url, key) = setup();
    let driver = &dbine_driver_cosmosdb::drivers()[0];
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url.clone(), database: db.into(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key.clone());
    let s = driver.connect(&c, None).await.expect("connect");
    (url, key, s)
}

/// A fresh database with containers partitioned by `/cat`, and a session.
async fn fresh(db: &str, containers: &[&str]) -> (String, String, Box<dyn dbine_driver::Session>) {
    let (url, key) = setup();
    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{db}"), &format!("/dbs/{db}"), None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": db }))).await, 201);
    for c in containers {
        let body = json!({ "id": c, "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
        assert_eq!(rest(&url, &key, "POST", "colls", &format!("dbs/{db}"), &format!("/dbs/{db}/colls"), Some(body)).await, 201);
    }
    let driver = &dbine_driver_cosmosdb::drivers()[0];
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url.clone(), database: db.into(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key.clone());
    let s = driver.connect(&c, None).await.expect("connect");
    (url, key, s)
}

fn coll(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: name.into() }
}

fn load_spec(table: &str, cols: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: coll(table),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// Items in a container (by reading their ids).
async fn count(s: &mut Box<dyn dbine_driver::Session>, table: &str) -> u64 {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: coll(table), columns: Some(vec!["id".into()]), filter: None }, sink).await.expect("count")
}

/// Rows `id, cat, pad` of `pad` bytes, `cats` partition key values, one
/// batch of `per` rows at a time, up to `limit` rows.
struct Wide {
    next: usize,
    limit: usize,
    per: usize,
    cats: usize,
    pad: String,
}

#[async_trait]
impl BatchSource for Wide {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.next >= self.limit {
            return None;
        }
        let end = (self.next + self.per).min(self.limit);
        let rows = (self.next..end)
            .map(|i| vec![Cell::Text(format!("w{i:06}")), Cell::Text(format!("c{}", i % self.cats)), Cell::Text(self.pad.clone())])
            .collect();
        self.next = end;
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// A failed or cancelled load leaves nothing written after it returns,
/// with batches in flight when it ends.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_cosmosdb_no_late_writes() {
    const DB: &str = "dbine_xfer_late";
    let (url, key, mut s) = fresh(DB, &["cancel", "fail"]).await;
    let pad = "x".repeat(15_000);

    // Cancelled: the load's future dropped mid-flight.
    let spec = load_spec("cancel", &["id", "cat", "pad"], 1);
    let mut src = Wide { next: 0, limit: 3_000, per: 50, cats: 40, pad: pad.clone() };
    let started = Instant::now();
    let r = tokio::time::timeout(std::time::Duration::from_millis(1_500), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
    assert!(r.is_err(), "the load should still be running when dropped");
    let dropped_after = started.elapsed();
    let at_drop = count(&mut s, "cancel").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let later = count(&mut s, "cancel").await;
    println!("cosmosdb cancel: {at_drop} items at drop ({dropped_after:?}), {later} 3 s later");
    assert_eq!(at_drop, later, "items written after the cancelled load returned");

    // Failed: an id that already exists, reached with batches in flight.
    let (st, _) = rest_json(
        &url,
        &key,
        "POST",
        "docs",
        &format!("dbs/{DB}/colls/fail"),
        &format!("/dbs/{DB}/colls/fail/docs"),
        Some(json!({"id": "w001500", "cat": "c20"})),
        &[("x-ms-documentdb-partitionkey", "[\"c20\"]")],
    )
    .await;
    assert_eq!(st, 201);
    let spec = load_spec("fail", &["id", "cat", "pad"], 1);
    let mut src = Wide { next: 0, limit: 3_000, per: 50, cats: 40, pad: "y".repeat(4_000) };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let e = s.bulk_load(&spec, &[], &mut src, &progress).await.unwrap_err();
    assert!(e.to_string().contains("«w001500»"), "{e}");
    let at_return = count(&mut s, "fail").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let later = count(&mut s, "fail").await;
    let last = reports.into_inner().unwrap().last().copied().unwrap_or(0);
    println!("cosmosdb failure: progress {last}, {at_return} items at return, {later} 3 s later");
    assert_eq!(at_return, later, "items written after the failed load returned");
    // Every row the load committed (drained ones included), not the old item.
    assert_eq!(last, at_return - 1, "progress counts the committed rows ({last} vs {at_return} incl. the old one)");

    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{DB}"), &format!("/dbs/{DB}"), None).await;
}

/// Keys that only appear past the first thousand items are read; an
/// explicit `null` reads as a null cell (what a SQL target writes as NULL),
/// and the native copy keeps it `null` (the partition key's too), not a
/// missing key.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_cosmosdb_heterogeneous_and_nulls() {
    const DB: &str = "dbine_xfer_shape";
    let (url, key, mut s) = fresh(DB, &["mixed", "nulls", "copy", "picked"]).await;

    let mut rows: Vec<Vec<Cell>> = (0..1_500).map(|i| vec![Cell::Text(format!("m{i:05}")), Cell::Text(format!("c{}", i % 7)), Cell::Int(i), Cell::Null]).collect();
    rows.extend((0..20).map(|i| vec![Cell::Text(format!("z{i:05}")), Cell::Text("c1".into()), Cell::Null, Cell::Int(1_000 + i)]));
    let spec = load_spec("mixed", &["id", "cat", "n", "late"], 10_000);
    assert_eq!(s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|_| {}).await.unwrap(), 1_520);
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(s.read_batches(&ReadSpec { table: coll("mixed"), columns: None, filter: None }, sink.clone()).await.unwrap(), 1_520);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&names[..4], &["id", "cat", "late", "n"], "{names:?}");
    let late: Vec<i64> = got.rows.iter().filter_map(|r| if let Cell::Int(n) = r[2] { Some(n) } else { None }).collect();
    assert_eq!(late.len(), 20, "the late key's values");

    // Explicit nulls, the partition key's included, and a missing key.
    let link = format!("dbs/{DB}/colls/nulls");
    let path = format!("/dbs/{DB}/colls/nulls/docs");
    for (doc, pk) in [(json!({"id": "a", "cat": null, "x": null}), "[null]"), (json!({"id": "b", "y": 1}), "[{}]")] {
        let (st, _) = rest_json(&url, &key, "POST", "docs", &link, &path, Some(doc), &[("x-ms-documentdb-partitionkey", pk)]).await;
        assert_eq!(st, 201);
    }
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: coll("nulls"), columns: None, filter: None }, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let cols: Vec<String> = got.columns.iter().map(|c| c.name.clone()).collect();
    let (ci, xi) = (cols.iter().position(|c| c == "cat").unwrap(), cols.iter().position(|c| c == "x").unwrap());
    let a = got.rows.iter().find(|r| r[0] == Cell::Text("a".into())).unwrap();
    assert_eq!((&a[ci], &a[xi]), (&Cell::Null, &Cell::Null), "explicit nulls read as NULL");

    // Cosmos DB to Cosmos DB: the native copy, asked for every column.
    let driver = &dbine_driver_cosmosdb::drivers()[0];
    assert!(driver.supports_native_copy("cosmosdb"));
    let (_, _, mut t) = fresh_session(DB).await;
    let copy = dbine_driver::transfer::CopySpec {
        source: ReadSpec { table: coll("nulls"), columns: None, filter: None },
        target: LoadSpec { columns: vec![], ..load_spec("copy", &[], 10_000) },
    };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    assert_eq!(driver.copy_native(&mut *s, &mut *t, &copy, &progress).await.unwrap(), 2);
    assert_eq!(reports.into_inner().unwrap().last(), Some(&2));
    let q = json!({"query": "SELECT c.id, IS_DEFINED(c.cat) dc, IS_NULL(c.cat) nc, IS_DEFINED(c.x) dx, IS_DEFINED(c.y) dy FROM c", "parameters": []});
    let (st, body) = rest_json(
        &url,
        &key,
        "POST",
        "docs",
        &format!("dbs/{DB}/colls/copy"),
        &format!("/dbs/{DB}/colls/copy/docs"),
        Some(q),
        &[("x-ms-documentdb-isquery", "True"), ("Content-Type", "application/query+json"), ("x-ms-documentdb-query-enablecrosspartition", "True")],
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let mut docs = body["Documents"].as_array().unwrap().clone();
    docs.sort_by_key(|d| d["id"].as_str().unwrap().to_string());
    assert_eq!(docs[0], json!({"id": "a", "dc": true, "nc": true, "dx": true, "dy": false}));
    assert_eq!(docs[1], json!({"id": "b", "dc": false, "nc": false, "dx": false, "dy": true}));
    // A point read in the null partition finds the copy.
    let (st, _) = rest_json(
        &url,
        &key,
        "GET",
        "docs",
        &format!("dbs/{DB}/colls/copy/docs/a"),
        &format!("/dbs/{DB}/colls/copy/docs/a"),
        None,
        &[("x-ms-documentdb-partitionkey", "[null]")],
    )
    .await;
    assert_eq!(st, 200);

    // Asked-for columns, renamed, and a filter: only those keys of the
    // matching items (the explicit null kept, `cat` left out).
    let copy = dbine_driver::transfer::CopySpec {
        source: ReadSpec { table: coll("nulls"), columns: Some(vec!["id".into(), "x".into()]), filter: Some("c.id = 'a'".into()) },
        target: load_spec("picked", &["id", "equis"], 10_000),
    };
    assert_eq!(driver.copy_native(&mut *s, &mut *t, &copy, &|_| {}).await.unwrap(), 1);
    let q = json!({"query": "SELECT c.id, IS_NULL(c.equis) ne, IS_DEFINED(c.x) dx, IS_DEFINED(c.cat) dc FROM c", "parameters": []});
    let (st, body) = rest_json(
        &url,
        &key,
        "POST",
        "docs",
        &format!("dbs/{DB}/colls/picked"),
        &format!("/dbs/{DB}/colls/picked/docs"),
        Some(q),
        &[("x-ms-documentdb-isquery", "True"), ("Content-Type", "application/query+json"), ("x-ms-documentdb-query-enablecrosspartition", "True")],
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["Documents"], json!([{"id": "a", "ne": true, "dx": false, "dc": false}]));
    // The same copy again: a create never replaces.
    let e = driver.copy_native(&mut *s, &mut *t, &copy, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("ya existe"), "{e}");

    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{DB}"), &format!("/dbs/{DB}"), None).await;
}

/// Wide rows with distinct partition key values: what the load has taken
/// from the source and not yet written stays within the budget.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_cosmosdb_memory_budget() {
    const DB: &str = "dbine_xfer_mem";
    const ROW: usize = 128 * 1024;
    const N: usize = 250;
    let (url, key, mut s) = fresh(DB, &["wide"]).await;

    struct Counted {
        inner: Wide,
        handed: Arc<std::sync::atomic::AtomicUsize>,
        written: Arc<std::sync::atomic::AtomicUsize>,
        worst: usize,
    }
    #[async_trait]
    impl BatchSource for Counted {
        async fn next(&mut self) -> Option<RowBatch> {
            use std::sync::atomic::Ordering::SeqCst;
            let b = self.inner.next().await?;
            let handed = self.handed.fetch_add(b.rows.len(), SeqCst) + b.rows.len();
            self.worst = self.worst.max(handed - self.written.load(SeqCst));
            Some(b)
        }
    }
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w = written.clone();
    let progress = move |n: u64| w.store(n as usize, std::sync::atomic::Ordering::SeqCst);
    let mut src = Counted {
        inner: Wide { next: 0, limit: N, per: 1, cats: N, pad: "p".repeat(ROW) },
        handed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        written,
        worst: 0,
    };
    let spec = load_spec("wide", &["id", "cat", "pad"], 1);
    assert_eq!(s.bulk_load(&spec, &[], &mut src, &progress).await.unwrap(), N as u64);
    let worst_mib = (src.worst * ROW) as f64 / (1 << 20) as f64;
    println!("cosmosdb: at most {} rows ({worst_mib:.1} MiB) taken and not yet written", src.worst);
    assert!(worst_mib <= 20.0, "{worst_mib:.1} MiB held by the load");
    assert_eq!(count(&mut s, "wide").await, N as u64);

    rest(&url, &key, "DELETE", "dbs", &format!("dbs/{DB}"), &format!("/dbs/{DB}"), None).await;
}

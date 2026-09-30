//! Bulk transfer against a real Couchbase Server (ignored by default; the
//! node initialized as in `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, QueryOutcome};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: usize = 50_000;
const BUCKET: &str = "dbine_xfer";

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

fn cfg() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_COUCHBASE_URL").unwrap_or_else(|_| "http://localhost:25893".into());
    let url = reqwest::Url::parse(&url).unwrap();
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(8093),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "25891".into()));
    c
}

const COLS: [&str; 7] = ["_id", "flag", "meta", "n", "name", "price", "big"];

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("d{i:06}")),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("{{\"k\":{i},\"tags\":[\"a\",\"b\"]}}")),
        Cell::Int(i as i64),
        Cell::Text(format!("name {i} \"q\" 'x'")),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Float(i as f64 + 0.25) },
        Cell::Int(9_007_199_254_740_993 + i as i64),
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchbase() {
    let c = cfg();
    let driver = dbine_driver_couchbase::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    let mut admin = driver.connect(&c, None).await.expect("connect");
    if admin.list_databases().await.unwrap().contains(&BUCKET.to_string()) {
        admin.drop_database(BUCKET).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    admin.create_database(BUCKET).await.expect("create bucket");
    let mut s = driver.connect(&c, Some(BUCKET)).await.expect("connect");
    // Reading needs an index (or 7.6's sequential scan).
    // The index service takes a moment with a new bucket.
    let mut indexed = Err(dbine_driver::Error::Cancelled);
    for _ in 0..30 {
        indexed = s.execute(&format!("CREATE PRIMARY INDEX ON `{BUCKET}`._default._default"), 10, &mut QueryOutcome::default()).await;
        if indexed.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    indexed.expect("primary index");

    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let table = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some(format!("{BUCKET}._default")), name: "_default".into() };
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
    println!("couchbase: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    // The same keys again: INSERT never replaces.
    let again = vec![RowBatch { rows: vec![row(1), row(2)], bytes: 0 }];
    let e = s.bulk_load(&spec, &[], &mut Batches(again.into_iter()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().to_lowercase().contains("duplicate"), "{e}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.expect("read");
    let secs = start.elapsed().as_secs_f64();
    println!("couchbase: read_batches {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read as usize, ROWS);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "_id");
    let pos: Vec<usize> = COLS.iter().map(|c| names.iter().position(|n| n == c).unwrap_or_else(|| panic!("{c} in {names:?}"))).collect();
    let by_id: HashMap<String, Vec<Cell>> = got
        .rows
        .into_iter()
        .map(|r| {
            let r: Vec<Cell> = pos.iter().map(|p| r[*p].clone()).collect();
            let Cell::Text(id) = &r[0] else { panic!("_id {:?}", r[0]) };
            (id.clone(), r)
        })
        .collect();
    assert_eq!(by_id.len(), ROWS);
    for i in 0..ROWS {
        assert_eq!(by_id[&format!("d{i:06}")], row(i), "row {i}");
    }

    // A SQL++ condition, with asked-for columns.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "n".into()]), filter: Some("d.n < 12".into()) };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 12);
    assert!(sink.lock().unwrap().rows.iter().all(|r| matches!(&r[1], Cell::Int(n) if Cell::Text(format!("d{n:06}")) == r[0])));

    admin.drop_database(BUCKET).await.unwrap();
}

// ---- Edge cases: document shapes, failed / cancelled loads, sizes ----

fn mgmt() -> String {
    let c = cfg();
    format!("http://{}:{}", c.host, c.options["mgmt_port"])
}

/// A SQL++ statement run directly (request_plus), its reply.
async fn sql(stmt: &str) -> serde_json::Value {
    let c = cfg();
    let r = reqwest::Client::new()
        .post(format!("http://{}:{}/query/service", c.host, c.port))
        .basic_auth("Administrator", Some("secreto1"))
        .json(&serde_json::json!({"statement": stmt, "scan_consistency": "request_plus"}))
        .send()
        .await
        .unwrap();
    r.json().await.unwrap()
}

async fn count(ks: &str, prefix: &str) -> u64 {
    let v = sql(&format!("SELECT RAW COUNT(*) FROM {ks} AS d WHERE META(d).id LIKE \"{prefix}%\"")).await;
    v["results"][0].as_u64().unwrap_or_else(|| panic!("{v}"))
}

/// A fresh bucket with `quota` MB and a primary index; a session on it.
async fn bucket(name: &str, quota: u32) -> Box<dyn dbine_driver::Session> {
    let c = cfg();
    let driver = dbine_driver_couchbase::drivers().remove(0);
    let mut admin = driver.connect(&c, None).await.expect("connect");
    if admin.list_databases().await.unwrap().contains(&name.to_string()) {
        admin.drop_database(name).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let r = reqwest::Client::new()
        .post(format!("{}/pools/default/buckets", mgmt()))
        .basic_auth("Administrator", Some("secreto1"))
        .form(&[("name", name), ("ramQuota", &quota.to_string()), ("bucketType", "couchbase"), ("flushEnabled", "0")])
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let mut s = driver.connect(&c, Some(name)).await.expect("connect");
    let mut indexed = Err(dbine_driver::Error::Cancelled);
    for _ in 0..60 {
        indexed = s.execute(&format!("CREATE PRIMARY INDEX ON `{name}`._default._default"), 10, &mut QueryOutcome::default()).await;
        if indexed.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    indexed.expect("primary index");
    s
}

async fn drop_bucket(name: &str) {
    let driver = dbine_driver_couchbase::drivers().remove(0);
    let mut admin = driver.connect(&cfg(), None).await.expect("connect");
    admin.drop_database(name).await.unwrap();
}

fn coll(bucket: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::COLLECTION.into(), schema: Some(format!("{bucket}._default")), name: name.into() }
}

fn load_spec(table: ObjectRef, columns: &[String]) -> LoadSpec {
    LoadSpec { table, columns: columns.to_vec(), table_lock: false, keep_identity: false, commit_rows: 1000, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES }
}

async fn read(s: &mut Box<dyn dbine_driver::Session>, table: &ObjectRef, filter: &str) -> dbine_driver::Result<Collect> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: Some(filter.into()) }, sink.clone()).await?;
    let got = std::mem::take(&mut *sink.lock().unwrap());
    Ok(got)
}

fn names(c: &Collect) -> Vec<String> {
    c.columns.iter().map(|c| c.name.clone()).collect()
}

/// Heterogeneous documents, explicit nulls, a field `_id`, non-objects.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchbase_documents() {
    const B: &str = "dbine_xfer_docs";
    let mut s = bucket(B, 100).await;
    let ks = format!("`{B}`._default._default");
    let src = coll(B, "_default");
    s.execute(&format!("CREATE COLLECTION `{B}`._default.copia"), 10, &mut QueryOutcome::default()).await.expect("collection");
    let copy = coll(B, "copia");
    let copy_ks = format!("`{B}`._default.copia");
    for _ in 0..30 {
        if sql(&format!("CREATE PRIMARY INDEX ON {copy_ks}")).await["status"] == "success" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // 1,000 documents with `a`, then one with `extra` too.
    let v = sql(&format!("INSERT INTO {ks} (KEY k, VALUE v) SELECT \"pr\" || TOSTRING(10000 + i) AS k, {{\"a\": i}} AS v FROM ARRAY_RANGE(0, 1000) AS i")).await;
    assert_eq!(v["status"], "success", "{v}");
    sql(&format!("INSERT INTO {ks} (KEY, VALUE) VALUES (\"pr99999\", {{\"a\": 1, \"extra\": \"x\"}})")).await;
    let got = read(&mut s, &src, "META(d).id LIKE \"pr%\"").await.unwrap();
    assert_eq!(names(&got), ["_id", "a", "extra"]);
    assert_eq!(got.rows.len(), 1001);
    let last = got.rows.iter().find(|r| r[0] == Cell::Text("pr99999".into())).unwrap();
    assert_eq!(last[2], Cell::Text("x".into()));

    // Explicit null vs missing, and a field `_id`: copied as they are.
    sql(&format!("INSERT INTO {ks} (KEY, VALUE) VALUES (\"nl1\", {{\"a\": null, \"b\": 1}}), (\"nl2\", {{\"b\": 2}}), (\"sh_idfield\", {{\"_id\": \"inner\", \"x\": 1}})")).await;
    for (filter, cols) in [("META(d).id LIKE \"nl%\"", vec!["_id", "a", "b"]), ("META(d).id = \"sh_idfield\"", vec!["meta_id", "_id", "x"])] {
        let got = read(&mut s, &src, filter).await.unwrap();
        assert_eq!(names(&got), cols);
        let spec = load_spec(copy.clone(), &names(&got));
        let batches = vec![RowBatch { rows: got.rows, bytes: 0 }];
        s.bulk_load(&spec, &got.columns, &mut Batches(batches.into_iter()), &|_| {}).await.expect("load");
    }
    let docs = sql(&format!("SELECT META(d).id AS k, d AS v FROM {copy_ks} AS d ORDER BY META(d).id")).await;
    assert_eq!(
        docs["results"],
        serde_json::json!([
            {"k": "nl1", "v": {"a": null, "b": 1}},
            {"k": "nl2", "v": {"b": 2}},
            {"k": "sh_idfield", "v": {"_id": "inner", "x": 1}},
        ])
    );
    assert_eq!(sql(&format!("SELECT RAW META(d).id FROM {copy_ks} AS d WHERE d.a IS NULL")).await["results"], serde_json::json!(["nl1"]));

    // A field `meta_id` of their own (and no `_id`): same keys, same documents.
    sql(&format!("INSERT INTO {ks} (KEY, VALUE) VALUES (\"mi1\", {{\"meta_id\": \"m1\", \"x\": 1}}), (\"mi2\", {{\"x\": 2}})")).await;
    let got = read(&mut s, &src, "META(d).id LIKE \"mi%\"").await.unwrap();
    assert_eq!(names(&got), ["_id", "meta_id", "x"]);
    let spec = load_spec(copy.clone(), &names(&got));
    s.bulk_load(&spec, &got.columns, &mut Batches(vec![RowBatch { rows: got.rows, bytes: 0 }].into_iter()), &|_| {}).await.expect("load meta_id");
    let docs = sql(&format!("SELECT META(d).id AS k, d AS v FROM {copy_ks} AS d WHERE META(d).id LIKE \"mi%\" ORDER BY META(d).id")).await;
    assert_eq!(docs["results"], serde_json::json!([{"k": "mi1", "v": {"meta_id": "m1", "x": 1}}, {"k": "mi2", "v": {"x": 2}}]));
    assert_eq!(count(&copy_ks, "m1").await, 0, "nothing loaded under the field's value");

    // Unqualified fields and META().id in the filter, no columns asked for
    // (the field discovery runs the filter too).
    for filter in ["x = 1 AND META().id LIKE \"mi%\"", "META().id = \"mi1\"", "d.x = 1 AND meta_id IS VALUED"] {
        let got = read(&mut s, &src, filter).await.unwrap_or_else(|e| panic!("{filter}: {e:?}"));
        assert_eq!((names(&got), got.rows.len()), (vec!["_id".to_string(), "meta_id".into(), "x".into()], 1), "{filter}");
        assert_eq!(got.rows[0][0], Cell::Text("mi1".into()), "{filter}");
    }

    // Documents that aren't objects fail the read (they have no fields).
    sql(&format!("INSERT INTO {ks} (KEY, VALUE) VALUES (\"sh_arr\", [1, 2, 3]), (\"sh_scalar\", \"hello\")")).await;
    for key in ["sh_arr", "sh_scalar"] {
        let e = read(&mut s, &src, &format!("META(d).id = \"{key}\"")).await.err().expect("non-object read");
        assert!(matches!(&e, dbine_driver::Error::Unsupported(m) if m.contains(key)), "{e:?}");
    }
    // A binary (non-JSON) document, written through the cluster manager.
    let r = reqwest::Client::new()
        .post(format!("{}/pools/default/buckets/{B}/scopes/_default/collections/_default/docs/sh_bin", mgmt()))
        .basic_auth("Administrator", Some("secreto1"))
        .form(&[("value", "not json \u{1}")])
        .send()
        .await
        .unwrap();
    if r.status().is_success() {
        let e = read(&mut s, &src, "META(d).id = \"sh_bin\"").await.err().expect("binary read");
        assert!(matches!(&e, dbine_driver::Error::Unsupported(_)), "{e:?}");
    } else {
        println!("binary document not written: {}", r.text().await.unwrap_or_default());
    }

    drop_bucket(B).await;
}

/// Source of `n` batches of 1,000 rows `prefix{i}`, then (optionally) a
/// malformed row; `None` for `n`: endless.
struct Rows {
    prefix: String,
    next: usize,
    batches: Option<usize>,
    bad_at_end: bool,
}

#[async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.batches == Some(0) {
            if std::mem::take(&mut self.bad_at_end) {
                return Some(RowBatch { rows: vec![vec![Cell::Text("bad".into())]], bytes: 0 });
            }
            return None;
        }
        if let Some(b) = &mut self.batches {
            *b -= 1;
        }
        let rows = (self.next..self.next + 1000).map(|i| vec![Cell::Text(format!("{}{i:06}", self.prefix)), Cell::Int(i as i64)]).collect();
        self.next += 1000;
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// A failed or cancelled load writes nothing after it returns, and its
/// progress counts what's committed; the interrupter stops a read.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchbase_failures() {
    const B: &str = "dbine_xfer_fail";
    let mut s = bucket(B, 100).await;
    let ks = format!("`{B}`._default._default");
    let t = coll(B, "_default");
    let cols = vec!["_id".to_string(), "n".to_string()];

    // A malformed row after many statements in flight.
    for run in 0..4 {
        let prefix = format!("f{run}_");
        let reports = Mutex::new(Vec::new());
        let progress = |n: u64| reports.lock().unwrap().push(n);
        let mut src = Rows { prefix: prefix.clone(), next: 0, batches: Some(8), bad_at_end: true };
        let e = s.bulk_load(&load_spec(t.clone(), &cols), &[], &mut src, &progress).await.unwrap_err();
        assert!(e.to_string().contains("valores"), "{e}");
        let c1 = count(&ks, &prefix).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let c2 = count(&ks, &prefix).await;
        assert_eq!(c1, c2, "run {run}: rows written after the failed load returned");
        assert_eq!(reports.lock().unwrap().last().copied().unwrap_or(0), c1, "run {run}: progress");
        println!("failed load {run}: {c1} committed");
    }

    // Cancelled as the engine does: interrupt, then drop the load.
    for run in 0..3 {
        let prefix = format!("c{run}_");
        let stop = s.interrupter().expect("interrupter");
        let mut src = Rows { prefix: prefix.clone(), next: 0, batches: None, bad_at_end: false };
        let spec = load_spec(t.clone(), &cols);
        {
            let load = s.bulk_load(&spec, &[], &mut src, &|_| {});
            tokio::pin!(load);
            tokio::select! {
                r = &mut load => panic!("endless load ended: {r:?}"),
                _ = tokio::time::sleep(Duration::from_millis(150 + 100 * run)) => stop(),
            }
            // Dropped here, as the engine drops its work.
        }
        let c1 = count(&ks, &prefix).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let c2 = count(&ks, &prefix).await;
        assert_eq!(c1, c2, "cancel {run}: rows written after the cancelled load was dropped");
        println!("cancelled load {run}: {c1} committed");
    }
    // Interrupted and awaited (not dropped): the statements in flight end on
    // their own, so the last progress is exactly what was committed.
    for run in 0..3u64 {
        let prefix = format!("i{run}_");
        let stop = s.interrupter().expect("interrupter");
        let reports = Mutex::new(Vec::new());
        let progress = |n: u64| reports.lock().unwrap().push(n);
        let mut src = Rows { prefix: prefix.clone(), next: 0, batches: None, bad_at_end: false };
        let spec = load_spec(t.clone(), &cols);
        let load = s.bulk_load(&spec, &[], &mut src, &progress);
        tokio::pin!(load);
        tokio::select! {
            r = &mut load => panic!("endless load ended: {r:?}"),
            _ = tokio::time::sleep(Duration::from_millis(300 + 200 * run)) => stop(),
        }
        let r = tokio::time::timeout(Duration::from_secs(60), load).await.expect("interrupted load ended");
        assert!(matches!(r, Err(dbine_driver::Error::Cancelled)), "{r:?}");
        let c1 = count(&ks, &prefix).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(c1, count(&ks, &prefix).await, "interrupt {run}: rows written after the load returned");
        assert!(c1 > 0, "interrupt {run}: nothing written before the interrupt");
        assert_eq!(reports.lock().unwrap().last().copied().unwrap_or(0), c1, "interrupt {run}: progress");
        println!("interrupted load {run}: {c1} committed");
    }
    // An interrupt after a transfer ended (stale) doesn't cancel the next one.
    {
        let stop = s.interrupter().expect("interrupter");
        let mut src = Rows { prefix: "before_".into(), next: 0, batches: Some(1), bad_at_end: false };
        assert_eq!(s.bulk_load(&load_spec(t.clone(), &cols), &[], &mut src, &|_| {}).await.expect("before"), 1000);
        stop();
        let mut src = Rows { prefix: "after_".into(), next: 0, batches: Some(3), bad_at_end: false };
        assert_eq!(s.bulk_load(&load_spec(t.clone(), &cols), &[], &mut src, &|_| {}).await.expect("after a stale interrupt"), 3000);
        assert_eq!(count(&ks, "after_").await, 3000);
        stop();
        let got = read(&mut s, &t, "META(d).id LIKE \"after_%\"").await.expect("read after a stale interrupt");
        assert_eq!(got.rows.len(), 3000);
    }
    // Dropped without the interrupter too.
    {
        let mut src = Rows { prefix: "d_".into(), next: 0, batches: None, bad_at_end: false };
        let spec = load_spec(t.clone(), &cols);
        let r = tokio::time::timeout(Duration::from_millis(200), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
        assert!(r.is_err());
        let c1 = count(&ks, "d_").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(c1, count(&ks, "d_").await, "rows written after the dropped load");
    }

    // A duplicate key: progress counts every document written.
    sql(&format!("INSERT INTO {ks} (KEY, VALUE) VALUES (\"dup002500\", {{\"n\": -1}})")).await;
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let mut src = Rows { prefix: "dup".into(), next: 0, batches: Some(6), bad_at_end: false };
    let e = s.bulk_load(&load_spec(t.clone(), &cols), &[], &mut src, &progress).await.unwrap_err();
    assert!(e.to_string().to_lowercase().contains("duplicate"), "{e}");
    let written = count(&ks, "dup").await - 1;
    assert_eq!(reports.lock().unwrap().last().copied(), Some(written), "{:?}", reports.lock().unwrap());
    println!("duplicate: {written} committed");

    // The interrupter stops a read that pushes nothing (a sparse filter).
    let stop = s.interrupter().expect("interrupter");
    let marker = "ARRAY_LENGTH(ARRAY_RANGE(0, 30000 + d.n)) < 0";
    let reader = async {
        let sink = Arc::new(Mutex::new(Collect::default()));
        s.read_batches(&ReadSpec { table: t.clone(), columns: Some(cols.clone()), filter: Some(marker.into()) }, sink).await
    };
    tokio::pin!(reader);
    let start = Instant::now();
    tokio::select! {
        r = &mut reader => panic!("the read ended before the interrupt: {r:?}"),
        _ = tokio::time::sleep(Duration::from_millis(500)) => stop(),
    }
    let r = tokio::time::timeout(Duration::from_secs(5), reader).await.expect("read stopped");
    assert!(matches!(r, Err(dbine_driver::Error::Cancelled)), "{r:?}");
    println!("read interrupted after {:?}", start.elapsed());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let active = sql("SELECT RAW r.statement FROM system:active_requests AS r").await;
    let left: Vec<_> = active["results"].as_array().unwrap().iter().filter(|x| x.as_str().is_some_and(|x| x.contains("ARRAY_RANGE(0, 30000 + d.n)") && !x.contains("active_requests"))).collect();
    assert!(left.is_empty(), "still running on the server: {left:?}");

    drop_bucket(B).await;
}

/// Documents past the old per-statement limit (250 × 300 KB was over the
/// Query service's 64 MiB request cap) load; one past 20 MiB is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchbase_large_documents() {
    const B: &str = "dbine_xfer_big";
    let mut s = bucket(B, 256).await;
    let ks = format!("`{B}`._default._default");
    let t = coll(B, "_default");
    let cols = vec!["_id".to_string(), "blob".to_string()];
    let body = "x".repeat(300 * 1024);
    let rows: Vec<Vec<Cell>> = (0..250).map(|i| vec![Cell::Text(format!("big{i:04}")), Cell::Text(body.clone())]).collect();
    let batches: Vec<RowBatch> = rows.chunks(10).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    drop(rows);
    let start = Instant::now();
    let n = s.bulk_load(&load_spec(t.clone(), &cols), &[], &mut Batches(batches.into_iter()), &|_| {}).await.expect("large documents");
    println!("250 × 300 KB in {:?}", start.elapsed());
    assert_eq!(n, 250);
    assert_eq!(count(&ks, "big").await, 250);

    let huge = vec![RowBatch { rows: vec![vec![Cell::Text("huge".into()), Cell::Text("x".repeat(21 * 1024 * 1024))]], bytes: 0 }];
    let e = s.bulk_load(&load_spec(t, &cols), &[], &mut Batches(huge.into_iter()), &|_| {}).await.unwrap_err();
    assert!(matches!(&e, dbine_driver::Error::Unsupported(_)), "{e:?}");
    assert_eq!(count(&ks, "huge").await, 0);

    drop_bucket(B).await;
}

/// A load dropped on a current-thread runtime (which can't run its requests
/// while the drop waits) leaves nothing to be written afterwards either.
#[tokio::test]
#[ignore]
async fn transfer_couchbase_dropped_on_current_thread() {
    const B: &str = "dbine_xfer_ct";
    let mut s = bucket(B, 100).await;
    let ks = format!("`{B}`._default._default");
    let t = coll(B, "_default");
    let cols = vec!["_id".to_string(), "n".to_string()];
    for run in 0..3u64 {
        let prefix = format!("ct{run}_");
        let mut src = Rows { prefix: prefix.clone(), next: 0, batches: None, bad_at_end: false };
        let spec = load_spec(t.clone(), &cols);
        let r = tokio::time::timeout(Duration::from_millis(150 + 100 * run), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
        assert!(r.is_err());
        let c1 = count(&ks, &prefix).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let c2 = count(&ks, &prefix).await;
        assert_eq!(c1, c2, "run {run}: rows written after the dropped load");
        println!("current-thread drop {run}: {c1} committed");
    }
    drop_bucket(B).await;
}

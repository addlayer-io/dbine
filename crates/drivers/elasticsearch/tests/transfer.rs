//! Bulk transfer against real servers (ignored by default):
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch -- --ignored transfer --nocapture
//! ```
//! (containers as in `integration.rs`; `DBINE_TEST_OPENDISTRO_URL` too, if set).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, Driver, ObjectRef, QueryOutcome};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const INDEX: &str = "dbine_transfer";

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

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("d{i}")),
        Cell::Int(i as i64),
        Cell::Text(format!("name {i}")),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Float(i as f64 / 4.0) },
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("{{\"k\":{i},\"tags\":[\"a\",\"b\"]}}")),
        Cell::Bytes(vec![(i % 256) as u8, 0, 255]),
    ]
}

const COLS: [&str; 7] = ["_id", "n", "name", "price", "flag", "meta", "blob"];

async fn exercise(id: &str, url: &str) {
    let driver: Arc<dyn Driver> = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(driver.supports_bulk_load());
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() };
    let mut s = driver.connect(&cfg, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    let _ = s.execute(&format!("DELETE /{INDEX}"), 1, &mut out).await;
    let mapping = r#"{"settings":{"number_of_replicas":0},"mappings":{"properties":{
        "n":{"type":"long"},"name":{"type":"keyword"},"price":{"type":"double"},"flag":{"type":"boolean"},
        "meta":{"properties":{"k":{"type":"long"},"tags":{"type":"keyword"}}},"blob":{"type":"binary"}}}}"#;
    s.execute(&format!("PUT /{INDEX}\n{mapping}"), 1, &mut QueryOutcome::default()).await.expect("create index");

    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let table = ObjectRef { kind: "index".into(), schema: None, name: INDEX.into() };
    let spec = LoadSpec {
        table: table.clone(),
        columns: COLS.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 20_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &progress).await.expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    println!("{id}: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s
        .read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone())
        .await
        .expect("read_batches");
    let secs = start.elapsed().as_secs_f64();
    println!("{id}: read_batches {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read as usize, ROWS);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    // A full read also brings `_routing` (none here) and `_source` (the
    // keys no column holds: none here).
    let mut want_names = COLS.to_vec();
    want_names.push("_routing");
    want_names.push("_source");
    want_names[1..].sort();
    let mut sorted_got = names.clone();
    sorted_got[1..].sort();
    assert_eq!(sorted_got, want_names);
    let routing = names.iter().position(|n| *n == "_routing").unwrap();
    assert!(got.rows.iter().all(|r| r[routing] == Cell::Null));
    let rest = names.iter().position(|n| *n == "_source").unwrap();
    assert!(got.rows.iter().all(|r| r[rest] == Cell::Null));
    let pos: Vec<usize> = COLS.iter().map(|c| names.iter().position(|n| n == c).unwrap()).collect();
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
        let mut want = row(i);
        // Nested JSON comes back compact (keys in document order).
        want[5] = Cell::Json(serde_json::from_str::<serde_json::Value>(match &want[5] {
            Cell::Json(s) => s,
            _ => unreachable!(),
        })
        .unwrap()
        .to_string());
        assert_eq!(by_id[&format!("d{i}")], want, "row {i}");
    }

    // Asked-for columns (a dotted path) and a filter.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table,
        columns: Some(vec!["_id".into(), "meta.k".into()]),
        filter: Some(r#"{"range": {"n": {"lt": 10}}}"#.into()),
    };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 10);
    assert!(sink.lock().unwrap().rows.iter().all(|r| matches!(&r[1], Cell::Int(k) if Cell::Text(format!("d{k}")) == r[0])));
    s.execute(&format!("DELETE /{INDEX}"), 1, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_elasticsearch() {
    let url = std::env::var("DBINE_TEST_ELASTICSEARCH_URL").unwrap_or_else(|_| "http://localhost:25520".into());
    exercise("elasticsearch", &url).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_opensearch() {
    let url = std::env::var("DBINE_TEST_OPENSEARCH_URL").unwrap_or_else(|_| "http://localhost:25521".into());
    exercise("opensearch", &url).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_opendistro() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else { return };
    exercise("opendistro", &url).await;
}

// --- Review regressions ------------------------------------------------

fn es_url() -> String {
    std::env::var("DBINE_TEST_ELASTICSEARCH_URL").unwrap_or_else(|_| "http://localhost:25520".into())
}

fn os_url() -> String {
    std::env::var("DBINE_TEST_OPENSEARCH_URL").unwrap_or_else(|_| "http://localhost:25521".into())
}

async fn connect(id: &str, url: &str) -> Box<dyn dbine_driver::Session> {
    let driver = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    driver.connect(&ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() }, None).await.expect("connect")
}

/// Raw REST call: status and body text.
async fn rest(method: &str, url: &str, body: Option<&str>) -> (u16, String) {
    let c = reqwest::Client::new();
    let mut rb = c.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url);
    if let Some(b) = body {
        rb = rb.header("Content-Type", "application/json").body(b.to_string());
    }
    let r = rb.send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn count(url: &str, index: &str) -> u64 {
    rest("POST", &format!("{url}/{index}/_refresh"), None).await;
    let (_, t) = rest("GET", &format!("{url}/{index}/_count"), None).await;
    serde_json::from_str::<serde_json::Value>(&t).unwrap()["count"].as_u64().unwrap()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "index".into(), schema: None, name: name.into() }
}

fn load_spec(name: &str, columns: &[&str], commit_rows: u64, commit_bytes: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes,
    }
}

fn padded(i: usize) -> Vec<Cell> {
    vec![Cell::Text(format!("p{i}")), Cell::Text("x".repeat(2048))]
}

/// Problems 2-6: an ES → ES copy through read_batches / bulk_load keeps
/// strings as strings, big numbers' text, unmapped fields, `_routing` and
/// explicit nulls; unknown columns fail.
async fn faithful_copy(id: &str, url: &str) {
    let (src, dst) = ("dbine_faithful_src", "dbine_faithful_dst");
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
    }
    let mapping = r#"{"settings":{"number_of_replicas":0},"mappings":{"dynamic":false,"properties":{
        "code":{"type":"keyword"},"k":{"type":"keyword"},"ws":{"type":"keyword"},"t":{"type":"text"},
        "nv":{"type":"keyword","null_value":"NONE"},"pi":{"type":"double"}}}}"#;
    for i in [src, dst] {
        let (st, t) = rest("PUT", &format!("{url}/{i}"), Some(mapping)).await;
        assert!(st < 300, "{t}");
    }
    let doc1 = r#"{"code":12345678901234567890123,"k":"[1,2]","ws":"  [3]  ","t":"{}","nv":null,"pi":3.14159265358979323846,"extra":"unmapped value"}"#;
    let (st, t) = rest("PUT", &format!("{url}/{src}/_doc/1?routing=r7&refresh=true"), Some(doc1)).await;
    assert!(st < 300, "{t}");
    let (st, t) = rest("PUT", &format!("{url}/{src}/_doc/2?refresh=true"), Some(r#"{"k":"plain"}"#)).await;
    assert!(st < 300, "{t}");

    let mut s = connect(id, url).await;
    // Problem 4.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table(src), columns: Some(vec!["_id".into(), "no_such_column".into()]), filter: None };
    let err = s.read_batches(&spec, sink).await.unwrap_err().to_string();
    assert!(err.contains("la lectura no trae la columna «no_such_column»"), "{err}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = s.read_batches(&ReadSpec { table: table(src), columns: None, filter: None }, sink.clone()).await.unwrap();
    assert_eq!(read, 2);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"_routing") && names.contains(&"_source"), "{names:?}");
    let spec = load_spec(dst, &names, 1000, LoadSpec::DEFAULT_COMMIT_BYTES);
    let batches = vec![RowBatch { rows: got.rows, bytes: 0 }];
    let loaded = s.bulk_load(&spec, &got.columns, &mut Batches(batches.into_iter()), &|_| {}).await.unwrap();
    assert_eq!(loaded, 2);

    // The copied document, as text: numbers exactly, strings as strings.
    let (st, t) = rest("GET", &format!("{url}/{dst}/_doc/1?routing=r7"), None).await;
    assert_eq!(st, 200, "{t}");
    let src_text = t.split("\"_source\":").nth(1).unwrap();
    for part in [
        r#""code":12345678901234567890123"#,
        r#""k":"[1,2]""#,
        r#""ws":"  [3]  ""#,
        r#""t":"{}""#,
        r#""nv":null"#,
        r#""pi":3.14159265358979323846"#,
        r#""extra":"unmapped value""#,
    ] {
        assert!(src_text.contains(part), "{part} not in {src_text}");
    }
    assert!(t.contains(r#""_routing":"r7""#), "{t}");
    // Doc 2 has no `nv`: still missing in the target.
    let (_, t2) = rest("GET", &format!("{url}/{dst}/_doc/2"), None).await;
    assert!(!t2.contains("\"nv\""), "{t2}");
    // null_value indexes the explicit null alike on both sides.
    let q = r#"{"query":{"term":{"nv":"NONE"}}}"#;
    for i in [src, dst] {
        let (_, c) = rest("POST", &format!("{url}/{i}/_count"), Some(q)).await;
        assert!(c.contains("\"count\":1"), "{i}: {c}");
    }
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_faithful_elasticsearch() {
    faithful_copy("elasticsearch", &es_url()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_faithful_opensearch() {
    faithful_copy("opensearch", &os_url()).await;
}

/// Round 2, problems 1 and 4: on a dynamic index, `_source` keys with dots
/// (`"host.name"`) survive a full ES → ES copy as they came; a multi-field
/// (`title.keyword`) is not a column.
async fn dotted_keys_copy(id: &str, url: &str) {
    let (src, dst) = ("dbine_dotted_src", "dbine_dotted_dst");
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
    }
    let doc = r#"{"host.name":"web-1","k":"x","title":"Dune","a.b":{"c":[1,2]}}"#;
    let (st, t) = rest("PUT", &format!("{url}/{src}/_doc/1?refresh=true"), Some(doc)).await;
    assert!(st < 300, "{t}");

    let mut s = connect(id, url).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table(src), columns: Some(vec!["_id".into(), "title.keyword".into()]), filter: None };
    let err = s.read_batches(&spec, sink).await.unwrap_err().to_string();
    assert!(err.contains("la lectura no trae la columna «title.keyword»"), "{err}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(s.read_batches(&ReadSpec { table: table(src), columns: None, filter: None }, sink.clone()).await.unwrap(), 1);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    let spec = load_spec(dst, &names, 1000, LoadSpec::DEFAULT_COMMIT_BYTES);
    let loaded = s.bulk_load(&spec, &got.columns, &mut Batches(vec![RowBatch { rows: got.rows, bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(loaded, 1);
    let (st, t) = rest("GET", &format!("{url}/{dst}/_doc/1"), None).await;
    assert_eq!(st, 200, "{t}");
    let copied: serde_json::Value = serde_json::from_str(&t).unwrap();
    assert_eq!(copied["_source"], serde_json::from_str::<serde_json::Value>(doc).unwrap(), "{t}");
    // Searchable the same way on both sides.
    let q = r#"{"query":{"term":{"host.name.keyword":"web-1"}}}"#;
    for i in [src, dst] {
        let (_, c) = rest("POST", &format!("{url}/{i}/_count"), Some(q)).await;
        assert!(c.contains("\"count\":1"), "{i}: {c}");
    }
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dotted_keys_elasticsearch() {
    dotted_keys_copy("elasticsearch", &es_url()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dotted_keys_opensearch() {
    dotted_keys_copy("opensearch", &os_url()).await;
}

/// Round 21: a column that reads part of an entry (`metrics.mem`, a
/// `subobjects: false` field, of `{"metrics": {"mem", "disk"}}`) doesn't take
/// the rest of it out of `_source`; a `copy_to` target isn't a column.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_partly_read_entries_elasticsearch() {
    let url = es_url();
    let (src, dst) = ("dbine_partial_src", "dbine_partial_dst");
    let mapping = r#"{"settings":{"number_of_replicas":0},"mappings":{"subobjects":false,"dynamic":false,"properties":{"metrics.mem":{"type":"long"},"k":{"type":"keyword"},
        "first":{"type":"text","copy_to":"full"},"full":{"type":"text"}}}}"#;
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
        let (st, t) = rest("PUT", &format!("{url}/{i}"), Some(mapping)).await;
        assert!(st < 300, "{t}");
    }
    let doc = r#"{"k":"x","metrics":{"mem":2,"disk":3},"first":"Ana"}"#;
    let (st, t) = rest("PUT", &format!("{url}/{src}/_doc/1?refresh=true"), Some(doc)).await;
    assert!(st < 300, "{t}");

    let mut s = connect("elasticsearch", &url).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table(src), columns: Some(vec!["_id".into(), "full".into()]), filter: None };
    let err = s.read_batches(&spec, sink).await.unwrap_err().to_string();
    assert!(err.contains("la lectura no trae la columna «full»"), "{err}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(s.read_batches(&ReadSpec { table: table(src), columns: None, filter: None }, sink.clone()).await.unwrap(), 1);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"full"), "{names:?}");
    let spec = load_spec(dst, &names, 1000, LoadSpec::DEFAULT_COMMIT_BYTES);
    s.bulk_load(&spec, &got.columns, &mut Batches(vec![RowBatch { rows: got.rows, bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    let (st, t) = rest("GET", &format!("{url}/{dst}/_doc/1"), None).await;
    assert_eq!(st, 200, "{t}");
    let copied: serde_json::Value = serde_json::from_str(&t).unwrap();
    let source = &copied["_source"];
    assert_eq!(source["metrics.mem"], 2, "{t}");
    assert_eq!(source["metrics"]["disk"], 3, "metrics.disk se perdió: {t}");
    assert_eq!((source["k"].as_str(), source["first"].as_str()), (Some("x"), Some("Ana")), "{t}");
    for i in [src, dst] {
        rest("DELETE", &format!("{url}/{i}"), None).await;
    }
}

async fn fresh(url: &str, index: &str) {
    rest("DELETE", &format!("{url}/{index}"), None).await;
    let (st, t) = rest(
        "PUT",
        &format!("{url}/{index}"),
        Some(r#"{"settings":{"number_of_replicas":0},"mappings":{"properties":{"k":{"type":"keyword"},"pad":{"type":"keyword","index":false,"doc_values":false}}}}"#),
    )
    .await;
    assert!(st < 300, "{t}");
}

/// Problem 1: a failed load leaves nothing to commit after it returns.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_failed_load_commits_nothing_late() {
    let (url, index) = (es_url(), "dbine_late_fail");
    fresh(&url, index).await;
    let mut s = connect("elasticsearch", &url).await;
    let mut batches: Vec<RowBatch> =
        (0..30).map(|b| RowBatch { rows: (b * 1000..(b + 1) * 1000).map(padded).collect(), bytes: 0 }).collect();
    batches.push(RowBatch { rows: vec![vec![Cell::Text("bad".into())]], bytes: 0 });
    let last = Mutex::new(0u64);
    let spec = load_spec(index, &["_id", "pad"], 1000, LoadSpec::DEFAULT_COMMIT_BYTES);
    let err = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| *last.lock().unwrap() = n).await.unwrap_err();
    assert!(err.to_string().contains("la fila tiene 1 valores"), "{err}");
    let now = count(&url, index).await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let later = count(&url, index).await;
    let last = *last.lock().unwrap();
    println!("failed load: last progress {last}, count at return {now}, 4 s later {later}");
    assert_eq!(now, later);
    // Round 2, problem 3: the last progress is what's committed.
    assert_eq!(last, now);
    rest("DELETE", &format!("{url}/{index}"), None).await;
}

/// A source that never ends (the load is dropped while it runs).
struct Endless(usize);

#[async_trait]
impl BatchSource for Endless {
    async fn next(&mut self) -> Option<RowBatch> {
        let b = self.0;
        self.0 += 1;
        Some(RowBatch { rows: (b * 1000..(b + 1) * 1000).map(padded).collect(), bytes: 0 })
    }
}

/// Problem 1: a load dropped (cancelled) mid-way doesn't let go of its
/// requests in flight until they end.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dropped_load_commits_nothing_late() {
    let (url, index) = (es_url(), "dbine_late_drop");
    fresh(&url, index).await;
    let mut s = connect("elasticsearch", &url).await;
    let spec = load_spec(index, &["_id", "pad"], 1000, LoadSpec::DEFAULT_COMMIT_BYTES);
    let mut endless = Endless(0);
    let load = s.bulk_load(&spec, &[], &mut endless, &|_| {});
    assert!(tokio::time::timeout(std::time::Duration::from_millis(1500), load).await.is_err());
    let now = count(&url, index).await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let later = count(&url, index).await;
    println!("dropped load: count at drop {now}, 4 s later {later}");
    assert!(now > 0);
    assert_eq!(now, later);
    rest("DELETE", &format!("{url}/{index}"), None).await;
}

/// Problem 7: `commit_bytes` bounds each commit window, and progress
/// follows the windows.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_commit_bytes_bound_windows() {
    let (url, index) = (es_url(), "dbine_commit_bytes");
    fresh(&url, index).await;
    let mut s = connect("elasticsearch", &url).await;
    let batches: Vec<RowBatch> = (0..5).map(|b| RowBatch { rows: (b * 1000..(b + 1) * 1000).map(padded).collect(), bytes: 0 }).collect();
    let reports = Mutex::new(Vec::new());
    // ~2.1 KB per row: at most 31 rows per 64 KiB window.
    let spec = load_spec(index, &["_id", "pad"], 100_000, 64 * 1024);
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    assert_eq!(loaded, 5000);
    let reports = reports.into_inner().unwrap();
    assert_eq!(*reports.last().unwrap(), 5000);
    let steps: Vec<u64> = std::iter::once(reports[0]).chain(reports.windows(2).map(|w| w[1] - w[0])).collect();
    assert!(steps.iter().all(|d| *d <= 31), "{steps:?}");
    assert_eq!(count(&url, index).await, 5000);
    rest("DELETE", &format!("{url}/{index}"), None).await;
}

/// Problem 9: pages are sized by bytes (large documents read in full, with
/// a point in time and with a scroll).
async fn big_documents(id: &str, url: &str) {
    let index = "dbine_big_docs";
    fresh(url, index).await;
    let mut s = connect(id, url).await;
    let rows: Vec<Vec<Cell>> = (0..400).map(|i| vec![Cell::Text(format!("b{i}")), Cell::Text(format!("{i:06}").repeat(10_000))]).collect();
    let spec = load_spec(index, &["_id", "pad"], 100_000, LoadSpec::DEFAULT_COMMIT_BYTES);
    s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    // 400 documents of 60 KB: 24 MB, over one page's bound.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = s.read_batches(&ReadSpec { table: table(index), columns: None, filter: None }, sink.clone()).await.unwrap();
    assert_eq!(read, 400);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let pad = got.columns.iter().position(|c| c.name == "pad").unwrap();
    assert!(got.rows.iter().all(|r| matches!(&r[pad], Cell::Text(t) if t.len() == 60_000)));
    rest("DELETE", &format!("{url}/{index}"), None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_big_documents_elasticsearch() {
    big_documents("elasticsearch", &es_url()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_big_documents_opensearch() {
    big_documents("opensearch", &os_url()).await;
}

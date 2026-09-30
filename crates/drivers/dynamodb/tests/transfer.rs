//! Bulk transfer against DynamoDB Local (ignored by default; container as
//! in `integration.rs`). Small tables: DynamoDB Local holds everything in
//! memory.
//!
//! ```sh
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300 \
//!   cargo test -p dbine-driver-dynamodb --test transfer -- --ignored --test-threads=1 --nocapture
//! ```

use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{AttributeValue as A, Select};
use aws_sdk_dynamodb::Client;
use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, Session};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ROWS: usize = 20_000;
const TABLE: &str = "dbine_xfer";
/// Rows loaded after, into the non-empty table.
const EXTRA: usize = 1_000;

struct Batches(std::vec::IntoIter<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    Batches(vec![RowBatch { rows, bytes: 0 }].into_iter())
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

fn url() -> String {
    std::env::var("DBINE_TEST_DYNAMODB_URL").unwrap_or_else(|_| "http://localhost:25300".into())
}

fn cfg(url: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig { driver: "dynamodb".into(), ..Default::default() };
    for (k, v) in [("region", "us-east-1"), ("auth_mode", "keys"), ("access_key_id", "dummy"), ("secret_access_key", "dummy"), ("endpoint_url", url)] {
        c.options.insert(k.into(), v.into());
    }
    c
}

async fn session(url: &str) -> Box<dyn Session> {
    dbine_driver_dynamodb::drivers().remove(0).connect(&cfg(url), None).await.expect("connect")
}

async fn client() -> Client {
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(url())
        .credentials_provider(aws_credential_types::Credentials::new("dummy", "dummy", None, None, "t"))
        .load()
        .await;
    Client::new(&conf)
}

fn obj(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
}

fn spec(table: &str, cols: &[&str]) -> LoadSpec {
    LoadSpec {
        table: obj(table),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

async fn create(client: &Client, name: &str) {
    use aws_sdk_dynamodb::types::*;
    let _ = client.delete_table().table_name(name).send().await;
    client
        .create_table()
        .table_name(name)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(AttributeDefinition::builder().attribute_name("id").attribute_type(ScalarAttributeType::S).build().unwrap())
        .key_schema(KeySchemaElement::builder().attribute_name("id").key_type(KeyType::Hash).build().unwrap())
        .send()
        .await
        .expect("create table");
}

async fn drop_table(client: &Client, name: &str) {
    let _ = client.delete_table().table_name(name).send().await;
}

async fn count(client: &Client, name: &str) -> i32 {
    let (mut n, mut start) = (0, None);
    loop {
        let out = client.scan().table_name(name).select(Select::Count).set_exclusive_start_key(start).send().await.unwrap();
        n += out.count;
        start = out.last_evaluated_key.filter(|k| !k.is_empty());
        if start.is_none() {
            return n;
        }
    }
}

async fn get(client: &Client, name: &str, id: &str) -> HashMap<String, A> {
    client.get_item().table_name(name).key("id", A::S(id.into())).consistent_read(true).send().await.unwrap().item.unwrap_or_default()
}

const COLS: [&str; 8] = ["id", "flag", "meta", "n", "name", "price", "blob", "big"];

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("d{i:06}")),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("{{\"k\":{i},\"tags\":[\"a\",\"b\"]}}")),
        Cell::Int(i as i64),
        Cell::Text(format!("name {i} \"q\" 'x'")),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Decimal(format!("{i}.25")) },
        Cell::Bytes(vec![(i % 256) as u8, 0, 255]),
        Cell::Decimal(format!("1234567890123456789012345678901234567{}", i % 10)),
    ]
}

fn kv(id: &str, v: &str) -> Vec<Cell> {
    vec![Cell::Text(id.into()), Cell::Text(v.into())]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dynamodb() {
    let client = client().await;
    create(&client, TABLE).await;
    let mut s = session(&url()).await;
    assert!(dbine_driver_dynamodb::drivers()[0].supports_bulk_load());

    let chunks: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let spec = spec(TABLE, &COLS);
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(chunks.into_iter()), &progress).await.expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    println!("dynamodb: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 2 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");
    assert_eq!(count(&client, TABLE).await as usize, ROWS);

    // Existing keys are never replaced.
    let e = s.bulk_load(&spec, &[], &mut batches(vec![row(1), row(2)]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("Ya hay un ítem con la clave id=\"d00000"), "{e}");
    let extra: Vec<Vec<Cell>> = (0..EXTRA)
        .map(|i| {
            let mut r = row(i);
            r[0] = Cell::Text(format!("x{i:06}"));
            r
        })
        .collect();
    let n = s.bulk_load(&spec, &[], &mut batches(extra), &|_| {}).await.expect("into a table with items");
    assert_eq!(n as usize, EXTRA);

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table: obj(TABLE), columns: None, filter: None }, sink.clone()).await.expect("read");
    let secs = start.elapsed().as_secs_f64();
    println!("dynamodb: read_batches {read} rows in {secs:.2}s = {:.0} rows/s (two passes)", read as f64 / secs);
    assert_eq!(read as usize, ROWS + EXTRA);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "id");
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
    assert_eq!(by_id.len(), ROWS + EXTRA);
    for i in 0..ROWS {
        assert_eq!(by_id[&format!("d{i:06}")], row(i), "row {i}");
    }
    assert_eq!(by_id["x000007"][1..], row(7)[1..]);

    // A PartiQL condition, with asked-for columns (on a small table:
    // DynamoDB Local's PartiQL SELECT holds the whole table in memory).
    create(&client, TABLE).await;
    assert_eq!(s.bulk_load(&spec, &[], &mut batches((0..100).map(row).collect()), &|_| {}).await.unwrap(), 100);
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = ReadSpec { table: obj(TABLE), columns: Some(vec!["id".into(), "n".into()]), filter: Some("n < 12".into()) };
    assert_eq!(s.read_batches(&read, sink.clone()).await.unwrap(), 12);
    assert!(sink.lock().unwrap().rows.iter().all(|r| matches!(&r[1], Cell::Int(n) if Cell::Text(format!("d{n:06}")) == r[0])));
    drop_table(&client, TABLE).await;
}

/// A TCP proxy to DynamoDB Local that holds what the client sends for
/// `delay` (answers come back at once): requests are still on their way
/// when the load returns.
async fn slow_proxy(delay: Duration) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let up = url().trim_start_matches("http://").trim_end_matches('/').to_string();
    tokio::spawn(async move {
        while let Ok((c, _)) = l.accept().await {
            let up = up.clone();
            tokio::spawn(async move {
                let Ok(s) = tokio::net::TcpStream::connect(&up).await else { return };
                let (mut cr, mut cw) = c.into_split();
                let (mut sr, mut sw) = s.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut sr, &mut cw).await;
                });
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = match cr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    tokio::time::sleep(delay).await;
                    if sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

/// Gives its first rows, then (300 ms later, with the requests on their
/// way) the second ones, and then waits forever (a source stuck upstream).
struct Stuck(Option<Vec<Vec<Cell>>>, Option<Vec<Vec<Cell>>>);

#[async_trait]
impl BatchSource for Stuck {
    async fn next(&mut self) -> Option<RowBatch> {
        if let Some(rows) = self.0.take() {
            return Some(RowBatch { rows, bytes: 0 });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        match self.1.take() {
            Some(rows) => Some(RowBatch { rows, bytes: 0 }),
            None => std::future::pending().await,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_no_writes_after_a_failed_or_cancelled_load() {
    const T: &str = "dbine_xfer_late";
    let client = client().await;
    let proxy = slow_proxy(Duration::from_millis(1500)).await;
    let rows = |n: usize| (0..n).map(|i| kv(&format!("k{i:04}"), "v")).collect::<Vec<_>>();

    // A bad row after 250 good ones: two transactions are in flight.
    create(&client, T).await;
    let mut s = session(&proxy).await;
    let mut src = Stuck(Some(rows(250)), Some(vec![vec![Cell::Text("bad".into())]]));
    let e = s.bulk_load(&spec(T, &["id", "v"]), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("la fila tiene 1 valores"), "{e}");
    let at_return = count(&client, T).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let later = count(&client, T).await;
    println!("failed load: {at_return} items at return, {later} after 4 s");
    assert_eq!(at_return, later, "items written after the load returned");
    assert_eq!(at_return % 100, 0, "transactions are whole");

    // Cancelled (the task aborted, as the driver host does).
    create(&client, T).await;
    let s = session(&proxy).await;
    let task = tokio::spawn(async move {
        let mut s = s;
        let mut src = Stuck(Some(rows(250)), None);
        s.bulk_load(&spec(T, &["id", "v"]), &[], &mut src, &|_| {}).await
    });
    // The table's description takes one delayed round trip; then the
    // transactions go out and are held by the proxy.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let at_return = count(&client, T).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let later = count(&client, T).await;
    println!("cancelled load: {at_return} items at return, {later} after 4 s");
    assert_eq!(at_return, later, "items written after the load was cancelled");
    drop_table(&client, T).await;
}

/// A load that fails while transactions are on their way: the ones that
/// get written while it waits for them are in the progress and the error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_failed_load_reports_every_written_row() {
    const T: &str = "dbine_xfer_drain";
    let client = client().await;
    let proxy = slow_proxy(Duration::from_millis(1500)).await;
    create(&client, T).await;
    client.put_item().table_name(T).item("id", A::S("k0150".into())).send().await.unwrap();
    let mut s = session(&proxy).await;
    let mut sp = spec(T, &["id", "v"]);
    sp.commit_rows = 1;
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    let rows: Vec<Vec<Cell>> = (0..600).map(|i| kv(&format!("k{i:04}"), "v")).collect();
    let e = s.bulk_load(&sp, &[], &mut batches(rows), &progress).await.unwrap_err().to_string();
    let written = count(&client, T).await - 1;
    let last = reports.lock().unwrap().last().copied().unwrap_or(0);
    println!("failed load: {written} items written, progress {last}: {e}");
    assert!(e.contains("Ya hay un ítem con la clave id=\"k0150\""), "{e}");
    assert_eq!(written, 500, "every transaction but k0150's");
    assert_eq!(last, written as u64, "progress = rows written");
    assert!(e.contains(&format!("quedaron escritos {written} ítems en {T}")), "{e}");
    drop_table(&client, T).await;
}

/// JSON from other engines (a PostgreSQL `jsonb`, a column without a
/// type) is plain JSON: keys that look like this crate's tags are kept.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_keeps_json_from_other_engines() {
    const T: &str = "dbine_xfer_json";
    let client = client().await;
    create(&client, T).await;
    let mut s = session(&url()).await;
    let docs = ["{\"$M\":{\"k\":1}}", "{\"$B\":\"00ff\"}", "{\"$SS\":[\"x\",\"y\"]}", "{\"$B\":\"zz\"}"];
    let rows = |p: &str| docs.iter().enumerate().map(|(i, d)| vec![Cell::Text(format!("{p}{i}")), Cell::Json(d.to_string())]).collect::<Vec<_>>();
    let col = |n: &str, t: &str| TransferColumn { name: n.into(), type_name: t.into(), nullable: true };
    let pg = [col("id", "text"), col("j", "jsonb")];
    assert_eq!(s.bulk_load(&spec(T, &["id", "j"]), &pg, &mut batches(rows("pg")), &|_| {}).await.unwrap(), 4);
    assert_eq!(s.bulk_load(&spec(T, &["id", "j"]), &[], &mut batches(rows("none")), &|_| {}).await.unwrap(), 4);
    let m = |k: &str, v: A| A::M(HashMap::from([(k.to_string(), v)]));
    let want = [
        m("$M", m("k", A::N("1".into()))),
        m("$B", A::S("00ff".into())),
        m("$SS", A::L(vec![A::S("x".into()), A::S("y".into())])),
        m("$B", A::S("zz".into())),
    ];
    for p in ["pg", "none"] {
        for (i, w) in want.iter().enumerate() {
            assert_eq!(get(&client, T, &format!("{p}{i}")).await.get("j"), Some(w), "{p}{i}");
        }
    }
    // Read back from DynamoDB and loaded again, each keeps its type.
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: obj(T), columns: None, filter: None }, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert!(got.columns.iter().all(|c| !c.type_name.is_empty()), "{:?}", got.columns);
    create(&client, T).await;
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(s.bulk_load(&spec(T, &names), &got.columns, &mut batches(got.rows), &|_| {}).await.unwrap(), 8);
    for p in ["pg", "none"] {
        for (i, w) in want.iter().enumerate() {
            assert_eq!(get(&client, T, &format!("{p}{i}")).await.get("j"), Some(w), "copied {p}{i}");
        }
    }
    drop_table(&client, T).await;
}

/// Another client writes the key right before the load's first row.
struct Racer {
    client: Client,
    table: &'static str,
    done: bool,
}

#[async_trait]
impl BatchSource for Racer {
    async fn next(&mut self) -> Option<RowBatch> {
        if std::mem::replace(&mut self.done, true) {
            return None;
        }
        self.client.put_item().table_name(self.table).item("id", A::S("k0".into())).item("owner", A::S("other-writer".into())).send().await.unwrap();
        Some(RowBatch { rows: vec![kv("k0", "from-load")], bytes: 0 })
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_never_replaces_items() {
    const T: &str = "dbine_xfer_keys";
    let client = client().await;
    let mut s = session(&url()).await;
    let sp = spec(T, &["id", "v"]);

    // Another writer's item, written after the load started.
    create(&client, T).await;
    let e = s.bulk_load(&sp, &[], &mut Racer { client: client.clone(), table: T, done: false }, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("Ya hay un ítem con la clave id=\"k0\""), "{e}");
    let item = get(&client, T, "k0").await;
    assert_eq!(item.get("owner"), Some(&A::S("other-writer".into())));
    assert!(!item.contains_key("v"));

    // A key repeated across requests (k0 again after 125 rows): whichever
    // transaction comes second fails whole, and nothing is replaced.
    create(&client, T).await;
    let mut data: Vec<Vec<Cell>> = (0..125).map(|i| kv(&format!("k{i}"), "first")).collect();
    data.push(kv("k0", "second"));
    let e = s.bulk_load(&sp, &[], &mut batches(data), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("clave id=\"k0\""), "{e}");
    let (k0, n) = (get(&client, T, "k0").await, count(&client, T).await);
    match k0.get("v") {
        Some(A::S(v)) if v == "first" => assert_eq!(n, 100, "the first transaction only"),
        Some(A::S(v)) if v == "second" => assert_eq!(n, 26, "the second transaction only"),
        v => panic!("k0 = {v:?}"),
    }
    // …and within one request.
    create(&client, T).await;
    let e = s.bulk_load(&sp, &[], &mut batches(vec![kv("a", "1"), kv("a", "2")]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("repetida"), "{e}");
    assert_eq!(count(&client, T).await, 0);
    // A row without its key.
    let e = s.bulk_load(&sp, &[], &mut batches(vec![vec![Cell::Null, Cell::Text("x".into())]]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("clave de la tabla"), "{e}");
    let e = s.bulk_load(&spec(T, &["v"]), &[], &mut batches(vec![vec![Cell::Text("x".into())]]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("no trae el atributo «id»"), "{e}");
    drop_table(&client, T).await;
}

fn sorted(v: &A) -> A {
    match v {
        A::Ss(s) => A::Ss({
            let mut s = s.clone();
            s.sort();
            s
        }),
        A::Ns(n) => A::Ns({
            let mut n = n.clone();
            n.sort();
            n
        }),
        A::Bs(b) => A::Bs({
            let mut b = b.clone();
            b.sort_by(|x, y| x.as_ref().cmp(y.as_ref()));
            b
        }),
        A::L(l) => A::L(l.iter().map(sorted).collect()),
        A::M(m) => A::M(m.iter().map(|(k, v)| (k.clone(), sorted(v))).collect()),
        v => v.clone(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_copies_dynamodb_without_loss() {
    const SRC: &str = "dbine_xfer_src";
    const DST: &str = "dbine_xfer_dst";
    let client = client().await;
    create(&client, SRC).await;
    create(&client, DST).await;
    let bin = A::B(Blob::new((0..2000).map(|i| (i % 251) as u8).collect::<Vec<u8>>()));
    let m = |kv: Vec<(&str, A)>| A::M(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    let item: HashMap<String, A> = [
        ("id", A::S("a".into())),
        ("m", m(vec![("bin", bin.clone()), ("num", A::N("12345678901234567890.123".into())), ("tag", m(vec![("$B", A::S("x".into()))]))])),
        ("l", A::L(vec![bin.clone(), A::Null(true), A::N("-0.5".into()), A::Bs(vec![Blob::new(vec![9])])])),
        ("ss", A::Ss(vec!["x".into(), "y".into()])),
        ("ns", A::Ns(vec!["1".into(), "99999999999999999999999999999999999999".into()])),
        ("bs", A::Bs(vec![Blob::new(vec![1]), Blob::new(vec![2, 3])])),
        ("nul", A::Null(true)),
        ("n", A::N("12345678901234567890123456789012345678".into())),
        ("b", bin.clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    client.put_item().table_name(SRC).set_item(Some(item.clone())).send().await.unwrap();
    client.put_item().table_name(SRC).item("id", A::S("b".into())).send().await.unwrap();

    let mut s = session(&url()).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(s.read_batches(&ReadSpec { table: obj(SRC), columns: None, filter: None }, sink.clone()).await.unwrap(), 2);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    let n = s.bulk_load(&spec(DST, &names), &got.columns, &mut batches(got.rows), &|_| {}).await.unwrap();
    assert_eq!(n, 2);
    let copied = get(&client, DST, "a").await;
    assert_eq!(copied.len(), item.len(), "{:?}", copied.keys().collect::<Vec<_>>());
    for (k, v) in &item {
        assert_eq!(sorted(&copied[k]), sorted(v), "attribute {k}");
    }
    // A missing attribute stays missing (not NULL).
    assert_eq!(get(&client, DST, "b").await.len(), 1);
    drop_table(&client, SRC).await;
    drop_table(&client, DST).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_reads_attributes_that_appear_late() {
    const T: &str = "dbine_xfer_wide";
    let client = client().await;
    create(&client, T).await;
    let mut s = session(&url()).await;
    let rows: Vec<Vec<Cell>> = (0..3000).map(|i| kv(&format!("k{i:05}"), "v")).collect();
    s.bulk_load(&spec(T, &["id", "v"]), &[], &mut batches(rows), &|_| {}).await.unwrap();
    for (i, id) in ["k00000", "k01500", "k02999", "k00777"].iter().enumerate() {
        client.update_item().table_name(T).key("id", A::S(id.to_string())).update_expression("SET #a = :v").expression_attribute_names("#a", format!("late{i}")).expression_attribute_values(":v", A::S(id.to_string())).send().await.unwrap();
    }
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(s.read_batches(&ReadSpec { table: obj(T), columns: None, filter: None }, sink.clone()).await.unwrap(), 3000);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "late0", "late1", "late2", "late3", "v"]);
    for (i, id) in ["k00000", "k01500", "k02999", "k00777"].iter().enumerate() {
        let r = got.rows.iter().find(|r| r[0] == Cell::Text(id.to_string())).unwrap();
        assert_eq!(r[1 + i], Cell::Text(id.to_string()));
    }
    drop_table(&client, T).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_refuses_what_dynamodb_cannot_hold() {
    const T: &str = "dbine_xfer_big";
    let client = client().await;
    create(&client, T).await;
    let mut s = session(&url()).await;
    let sp = spec(T, &["id", "v"]);
    let e = s.bulk_load(&sp, &[], &mut batches(vec![kv("huge", &"x".repeat(410 * 1024))]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("clave id=\"huge\"") && e.to_string().contains("400 KB"), "{e}");
    let e = s.bulk_load(&sp, &[], &mut batches(vec![vec![Cell::Text("f".into()), Cell::Float(1e300)]]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("atributo «v»") && e.to_string().contains("38 dígitos"), "{e}");
    let e = s.bulk_load(&sp, &[], &mut batches(vec![vec![Cell::Text("d".into()), Cell::Decimal("1".repeat(39))]]), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("38 dígitos"), "{e}");
    assert_eq!(count(&client, T).await, 0);
    // Large items that fit, several per transaction.
    let rows: Vec<Vec<Cell>> = (0..12).map(|i| kv(&format!("b{i}"), &"y".repeat(350 * 1024))).collect();
    assert_eq!(s.bulk_load(&sp, &[], &mut batches(rows), &|_| {}).await.unwrap(), 12);
    assert_eq!(count(&client, T).await, 12);
    drop_table(&client, T).await;
}

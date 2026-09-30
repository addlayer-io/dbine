//! Bulk transfer against a real server (ignored by default; container as
//! in `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, ObjectRef};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const DB: &str = "dbine_transfer";

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
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").unwrap_or_else(|_| "http://admin:secret@localhost:25202".into());
    let rest = url.strip_prefix("http://").unwrap();
    let (auth, host) = rest.split_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (h, p) = host.trim_end_matches('/').split_once(':').unwrap();
    ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

const COLS: [&str; 6] = ["_id", "flag", "meta", "n", "name", "price"];

fn row(i: usize) -> Vec<Cell> {
    vec![
        Cell::Text(format!("d{i:06}")),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("{{\"k\":{i},\"tags\":[\"a\",\"b\"]}}")),
        Cell::Int(i as i64),
        Cell::Text(format!("name {i}")),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Float(i as f64 / 4.0 + 0.5) },
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchdb() {
    let driver = dbine_driver_couchdb::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    let mut admin = driver.connect(&cfg(), Some("_users")).await.expect("connect");
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.expect("create db");
    let mut s = driver.connect(&cfg(), Some(DB)).await.expect("connect");

    let batches: Vec<RowBatch> = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 })
        .collect();
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
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
    println!("couchdb: bulk_load {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    // Loading the same ids again fails with the per-document conflicts.
    let again = vec![RowBatch { rows: vec![row(1), row(2)], bytes: 0 }];
    let e = s.bulk_load(&spec, &[], &mut Batches(again.into_iter()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("conflict"), "{e}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.expect("read");
    let secs = start.elapsed().as_secs_f64();
    println!("couchdb: read_batches {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read as usize, ROWS);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "_id");
    assert!(names.contains(&"_rev"));
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
        assert_eq!(by_id[&format!("d{i:06}")], row(i), "row {i}");
    }

    // A Mango filter, with asked-for columns.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "n".into()]), filter: Some(r#"{"n": {"$lt": 12}}"#.into()) };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 12);
    assert!(sink.lock().unwrap().rows.iter().all(|r| matches!(&r[1], Cell::Int(n) if Cell::Text(format!("d{n:06}")) == r[0])));

    admin.drop_database(DB).await.unwrap();
}

/// Rows of every kind CouchDB keeps come back exactly as they were:
/// fields found only in later documents (past the first fetches, or
/// sorting last), integers past 64 bits, explicit nulls apart from missing
/// fields, attachments with their content.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_couchdb_lossless() {
    const SRC: &str = "dbine_transfer_lossless_src";
    const DST: &str = "dbine_transfer_lossless_dst";
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut admin = driver.connect(&cfg(), Some("_users")).await.expect("connect");
    for db in [SRC, DST] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create db");
    }
    let http = reqwest::Client::new();
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").unwrap_or_else(|_| "http://admin:secret@localhost:25202".into());
    let url = url.trim_end_matches('/');
    let (auth, host) = url.strip_prefix("http://").unwrap().split_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let base = format!("http://{host}");
    let post = |path: String, body: String| {
        http.post(format!("{base}{path}")).basic_auth(user, Some(pass)).header("Content-Type", "application/json").body(body).send()
    };
    // Written as raw JSON: serde_json would turn the big integer into a double.
    let mut docs: Vec<String> = (0..5_100).map(|i| format!(r#"{{"_id":"p{i:05}","n":{i}}}"#)).collect();
    docs.push(r#"{"_id":"p05099b","late_field":"tarde"}"#.into());
    docs.push(r#"{"_id":"ñandú 🦙","t":"último"}"#.into());
    docs.push(r#"{"_id":"big","n":123456789012345678901234567890,"nested":{"b":-98765432109876543210987654321}}"#.into());
    docs.push(r#"{"_id":"nulls","a":null,"b":"","c":[],"d":{}}"#.into());
    docs.push(r#"{"_id":"withatt","_attachments":{"a.txt":{"content_type":"text/plain","data":"aG9sYQ=="}}}"#.into());
    docs.push(r#"{"_id":"emptykey","":"vacío"}"#.into());
    docs.push(r#"{"_id":"_design/x","views":{}}"#.into());
    let r = post(format!("/{SRC}/_bulk_docs"), format!(r#"{{"docs":[{}]}}"#, docs.join(","))).await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());

    // Every field, whatever document brings it.
    let mut src = driver.connect(&cfg(), Some(SRC)).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = src.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.expect("read");
    assert_eq!(read, 5_106);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<String> = got.columns.iter().map(|c| c.name.clone()).collect();
    for f in ["_id", "_rev", "n", "late_field", "t", "nested", "a", "b", "c", "d", "_attachments"] {
        assert!(names.iter().any(|n| n == f), "{f} not in {names:?}");
    }
    let col = |n: &str| names.iter().position(|c| c == n).unwrap();
    let row = |id: &str| got.rows.iter().find(|r| r[0] == Cell::Text(id.into())).unwrap().clone();
    assert_eq!(row("big")[col("n")], Cell::Json("123456789012345678901234567890".into()));
    assert_eq!(row("nulls")[col("a")], Cell::Json("null".into()));
    assert_eq!(row("nulls")[col("late_field")], Cell::Null);
    assert!(matches!(&row("withatt")[col("_attachments")], Cell::Json(j) if j.contains("aG9sYQ==")));

    // Asked-for columns: a field no document has is an error, not nulls.
    let spec = ReadSpec { table: table.clone(), columns: Some(vec!["_id".into(), "no_such".into()]), filter: None };
    let e = src.read_batches(&spec, Arc::new(Mutex::new(Collect::default()))).await.unwrap_err();
    assert!(e.to_string().contains("no_such"), "{e}");
    // A real field missing from the filtered documents is fine.
    let spec = ReadSpec { table: table.clone(), columns: Some(vec!["_id".into(), "t".into()]), filter: Some(r#"{"n": 3}"#.into()) };
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(src.read_batches(&spec, sink.clone()).await.unwrap(), 1);
    assert_eq!(sink.lock().unwrap().rows, vec![vec![Cell::Text("p00003".into()), Cell::Null]]);
    // An empty field name is a field too (Mango can't ask for it).
    let spec = ReadSpec {
        table: table.clone(),
        columns: Some(vec!["_id".into(), "".into()]),
        filter: Some(r#"{"_id": "emptykey"}"#.into()),
    };
    let sink = Arc::new(Mutex::new(Collect::default()));
    assert_eq!(src.read_batches(&spec, sink.clone()).await.expect("empty field name"), 1);
    assert_eq!(sink.lock().unwrap().rows, vec![vec![Cell::Text("emptykey".into()), Cell::Text("vacío".into())]]);

    // And loaded into another database, the same documents.
    let mut dst = driver.connect(&cfg(), Some(DST)).await.expect("connect");
    let spec = LoadSpec {
        table: table.clone(),
        columns: names.clone(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batches: Vec<RowBatch> = got.rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    assert_eq!(dst.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|_| {}).await.expect("load"), 5_106);
    let get = |db: &str, id: &str| {
        let path = format!("/{db}/{}?attachments=true", percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC));
        let rq = http.get(format!("{base}{path}")).basic_auth(user, Some(pass)).header("Accept", "application/json");
        async move { rq.send().await.unwrap().text().await.unwrap() }
    };
    let strip = |s: String| {
        let s = s.split(r#","_rev":""#).collect::<Vec<_>>();
        let rest = s[1].split_once('"').unwrap().1;
        // The attachment's revpos/digest are the target's own.
        format!("{}{rest}", s[0])
    };
    for id in ["big", "nulls", "p05099b", "ñandú 🦙", "p00042", "emptykey"] {
        assert_eq!(strip(get(DST, id).await), strip(get(SRC, id).await), "{id}");
    }
    let att = get(DST, "withatt").await;
    assert!(att.contains(r#""data":"aG9sYQ==""#), "{att}");

    admin.drop_database(SRC).await.unwrap();
    admin.drop_database(DST).await.unwrap();
}

/// A tiny CouchDB stand-in: `_bulk_docs` answers after `delay` (a document
/// with `_id` "conflict" fails at once) and it counts what was committed,
/// what was in flight and the largest requests; `_all_docs` serves `docs`
/// documents of `doc_bytes` each (the first `tiny_first` of them empty);
/// `_find` knows only the field `pad`.
#[derive(Default)]
struct Fake {
    delay_ms: u64,
    docs: usize,
    doc_bytes: usize,
    tiny_first: usize,
    fetches: Mutex<Vec<(String, usize)>>,
    finds: AtomicUsize,
    active: AtomicUsize,
    active_bytes: AtomicUsize,
    max_active: AtomicUsize,
    max_active_bytes: AtomicUsize,
    max_body: AtomicUsize,
    max_docs: AtomicUsize,
    max_reply: AtomicUsize,
    first_fetch: AtomicUsize,
    committed: AtomicUsize,
    /// When each reply past 6 MiB was asked for and when it was all sent
    /// (only the ones the client read whole).
    whole: Mutex<Vec<(Instant, Instant)>>,
}

impl Fake {
    async fn start(self) -> (Arc<Fake>, ConnectionConfig) {
        let fake = Arc::new(self);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let f = fake.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                tokio::spawn(f.clone().serve(sock));
            }
        });
        let cfg = ConnectionConfig { driver: "couchdb".into(), host: "127.0.0.1".into(), port, ..Default::default() };
        (fake, cfg)
    }

    async fn serve(self: Arc<Self>, mut sock: tokio::net::TcpStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
            .unwrap_or(0);
        while buf.len() < head_end + len {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = &buf[head_end..head_end + len];
        let mut parts = head.split_whitespace();
        let (method, target) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let asked = Instant::now();
        let reply = if path == "/" {
            r#"{"couchdb":"Welcome","version":"3.3.3"}"#.to_string()
        } else if path.ends_with("/_bulk_docs") {
            self.bulk_docs(body).await
        } else if path.ends_with("/_all_docs") && method == "GET" {
            let start = query.split('&').find_map(|p| p.strip_prefix("startkey=")).map(|k| {
                let k = percent_encoding::percent_decode_str(k).decode_utf8().unwrap();
                serde_json::from_str::<String>(&k).unwrap()
            });
            let limit: usize = query.split('&').find_map(|p| p.strip_prefix("limit=")).unwrap().parse().unwrap();
            let from = start.map_or(0, |k| k[1..].parse::<usize>().unwrap());
            let rows: Vec<String> =
                (from..self.docs.min(from + limit)).map(|i| format!(r#"{{"id":"d{i:06}","key":"d{i:06}","value":{{}}}}"#)).collect();
            format!(r#"{{"rows":[{}]}}"#, rows.join(","))
        } else if path.ends_with("/_all_docs") {
            let keys: serde_json::Value = serde_json::from_slice(body).unwrap();
            let keys = keys["keys"].as_array().unwrap();
            let _ = self.first_fetch.compare_exchange(0, keys.len(), Ordering::SeqCst, Ordering::SeqCst);
            let row = |k: &serde_json::Value, pad: &str| {
                format!(r#"{{"id":{k},"key":{k},"value":{{}},"doc":{{"_id":{k},"_rev":"1-a","pad":"{pad}"}}}}"#)
            };
            let pad_of = |k: &serde_json::Value| {
                let i: usize = k.as_str().unwrap()[1..].parse().unwrap();
                if i < self.tiny_first {
                    0
                } else {
                    self.doc_bytes
                }
            };
            let total = r#"{"rows":[]}"#.len()
                + keys.iter().map(|k| row(k, "").len() + pad_of(k)).sum::<usize>()
                + keys.len().saturating_sub(1);
            if keys.len() > 1 && total > 6 * 1024 * 1024 {
                // Past the client's cut-off: not built, only what it may
                // read before it gives up.
                self.max_reply.fetch_max(total, Ordering::SeqCst);
                self.fetches.lock().unwrap().push((keys[0].as_str().unwrap().to_string(), total));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&vec![b' '; 7 * 1024 * 1024]).await;
                return;
            }
            let pad = "x".repeat(self.doc_bytes);
            let rows: Vec<String> = keys.iter().map(|k| row(k, &pad[..pad_of(k)])).collect();
            let r = format!(r#"{{"rows":[{}]}}"#, rows.join(","));
            self.max_reply.fetch_max(r.len(), Ordering::SeqCst);
            if let Some(k) = keys.first() {
                self.fetches.lock().unwrap().push((k.as_str().unwrap().to_string(), r.len()));
            }
            r
        } else if path.ends_with("/_find") {
            self.finds.fetch_add(1, Ordering::SeqCst);
            let q: serde_json::Value = serde_json::from_slice(body).unwrap();
            if q["selector"].get("pad").is_some() {
                r#"{"docs":[{"_id":"d000000"}],"bookmark":"x"}"#.to_string()
            } else {
                r#"{"docs":[],"bookmark":"nil"}"#.to_string()
            }
        } else {
            r#"{"error":"not_found","reason":"missing"}"#.to_string()
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let big = method == "POST" && path.ends_with("/_all_docs") && reply.len() > 6 * 1024 * 1024;
        if sock.write_all(resp.as_bytes()).await.is_ok() && big {
            self.whole.lock().unwrap().push((asked, Instant::now()));
        }
    }

    async fn bulk_docs(&self, body: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        let docs = v["docs"].as_array().unwrap();
        self.max_body.fetch_max(body.len(), Ordering::SeqCst);
        self.max_docs.fetch_max(docs.len(), Ordering::SeqCst);
        let ids: Vec<&str> = docs.iter().map(|d| d["_id"].as_str().unwrap()).collect();
        if ids.contains(&"conflict") {
            self.committed.fetch_add(docs.len() - 1, Ordering::SeqCst);
            let r: Vec<String> = ids
                .iter()
                .map(|id| {
                    if *id == "conflict" {
                        format!(r#"{{"id":"{id}","error":"conflict","reason":"Document update conflict."}}"#)
                    } else {
                        format!(r#"{{"id":"{id}","ok":true,"rev":"1-a"}}"#)
                    }
                })
                .collect();
            return format!("[{}]", r.join(","));
        }
        let a = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(a, Ordering::SeqCst);
        let b = self.active_bytes.fetch_add(body.len(), Ordering::SeqCst) + body.len();
        self.max_active_bytes.fetch_max(b, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        // Committed before the reply, as the server does.
        self.committed.fetch_add(docs.len(), Ordering::SeqCst);
        self.active_bytes.fetch_sub(body.len(), Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
        let r: Vec<String> = ids.iter().map(|id| format!(r#"{{"id":"{id}","ok":true,"rev":"1-a"}}"#)).collect();
        format!("[{}]", r.join(","))
    }
}

/// Rows made as they're asked for: `n` rows of `pad` bytes, `per` per batch.
struct Gen {
    next: usize,
    n: usize,
    per: usize,
    pad: usize,
    first_id: &'static str,
}

#[async_trait]
impl BatchSource for Gen {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.next >= self.n {
            return None;
        }
        let end = self.n.min(self.next + self.per);
        let rows = (self.next..end)
            .map(|i| {
                let id = if i == 0 { self.first_id.to_string() } else { format!("g{i:07}") };
                vec![Cell::Text(id), Cell::Text("x".repeat(self.pad))]
            })
            .collect();
        self.next = end;
        Some(RowBatch { rows, bytes: 0 })
    }
}

fn load_spec(commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() },
        columns: vec!["_id".into(), "pad".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// A failed load waits for the requests the server already has: no row is
/// committed after it returns.
#[tokio::test(flavor = "multi_thread")]
async fn failed_load_commits_nothing_after_returning() {
    let (fake, cfg) = Fake { delay_ms: 400, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let mut src = Gen { next: 0, n: 20_000, per: 1_000, pad: 10, first_id: "conflict" };
    let e = s.bulk_load(&load_spec(100_000), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("conflict"), "{e}");
    assert_eq!(fake.active.load(Ordering::SeqCst), 0, "requests still in flight after the error");
    let at_return = fake.committed.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert_eq!(fake.committed.load(Ordering::SeqCst), at_return);
}

/// A cancelled (dropped) load waits for its requests too.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_load_commits_nothing_after_returning() {
    let (fake, cfg) = Fake { delay_ms: 500, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let mut src = Gen { next: 0, n: 30_000, per: 1_000, pad: 2_000, first_id: "g0000000" };
    let spec = load_spec(100_000);
    let r = tokio::time::timeout(std::time::Duration::from_millis(150), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
    assert!(r.is_err(), "the load should have been cancelled");
    assert!(fake.max_active.load(Ordering::SeqCst) > 0);
    assert_eq!(fake.active.load(Ordering::SeqCst), 0, "requests still in flight after the cancel");
    let at_return = fake.committed.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    assert_eq!(fake.committed.load(Ordering::SeqCst), at_return);
}

/// Requests are bounded by bytes (not only documents) and by the commit
/// window, and so is what's in flight.
#[tokio::test(flavor = "multi_thread")]
async fn load_memory_is_bounded_by_bytes() {
    const MIB: usize = 1024 * 1024;
    let (fake, cfg) = Fake { delay_ms: 150, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let mut src = Gen { next: 0, n: 800, per: 32, pad: 64 * 1024, first_id: "g0000000" };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    assert_eq!(s.bulk_load(&load_spec(100_000), &[], &mut src, &progress).await.unwrap(), 800);
    assert_eq!(fake.committed.load(Ordering::SeqCst), 800);
    let body = fake.max_body.load(Ordering::SeqCst);
    assert!(body <= 4 * MIB, "request of {body} bytes");
    let in_flight = fake.max_active_bytes.load(Ordering::SeqCst);
    assert!(in_flight <= 16 * MIB, "{in_flight} bytes in flight");
    assert_eq!(reports.into_inner().unwrap(), vec![800]);

    // A commit window smaller than a request: the request shrinks to it,
    // and progress is the committed rows by window.
    let (fake, cfg) = Fake { delay_ms: 10, ..Default::default() }.start().await;
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let mut src = Gen { next: 0, n: 1_000, per: 1_000, pad: 10, first_id: "g0000000" };
    let reports = Mutex::new(Vec::new());
    let progress = |n: u64| reports.lock().unwrap().push(n);
    assert_eq!(s.bulk_load(&load_spec(250), &[], &mut src, &progress).await.unwrap(), 1_000);
    assert_eq!(fake.max_docs.load(Ordering::SeqCst), 250);
    assert_eq!(reports.into_inner().unwrap(), vec![250, 500, 750, 1_000]);
}

/// The read fetches documents by bytes: small first fetches, then fetches
/// of about 2 MiB, a few at once.
#[tokio::test(flavor = "multi_thread")]
async fn read_memory_is_bounded_by_bytes() {
    const MIB: usize = 1024 * 1024;
    let (fake, cfg) = Fake { docs: 3_000, doc_bytes: 16 * 1024, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let sink = Arc::new(Mutex::new(Count::default()));
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "pad".into()]), filter: None };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 3_000);
    assert_eq!(sink.lock().unwrap().0, 3_000);
    assert!(fake.first_fetch.load(Ordering::SeqCst) <= 50);
    let reply = fake.max_reply.load(Ordering::SeqCst);
    assert!(reply <= 2 * MIB + 64 * 1024, "reply of {reply} bytes");
}

/// A run of small documents before large ones doesn't make the next
/// fetches large: a reply past 6 MiB is cut off and its ids fetched again
/// (a fetch whose first id is asked for again was cut off), so no reply
/// larger than that is taken, and the rows still arrive whole and in order.
#[tokio::test(flavor = "multi_thread")]
async fn read_memory_is_bounded_when_documents_grow() {
    const MIB: usize = 1024 * 1024;
    let (fake, cfg) = Fake { docs: 1_500, doc_bytes: 32 * 1024, tiny_first: 250, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "pad".into()]), filter: None };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 1_500);
    let got = sink.lock().unwrap();
    assert_eq!(got.rows.len(), 1_500);
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r[0], Cell::Text(format!("d{i:06}")));
        let pad = if i < 250 { 0 } else { 32 * 1024 };
        assert!(matches!(&r[1], Cell::Text(p) if p.len() == pad), "row {i}");
    }
    let fetches = fake.fetches.lock().unwrap();
    let taken = fetches
        .iter()
        .enumerate()
        .filter(|(i, (k, _))| !fetches[i + 1..].iter().any(|(k2, _)| k2 == k))
        .map(|(_, (_, len))| *len);
    let largest = taken.max().unwrap();
    assert!(largest <= 6 * MIB, "reply of {largest} bytes taken");
}

/// Documents past the 6 MiB cut-off are fetched whole one at a time, with
/// no other whole fetch next to them: what's in flight is about one such
/// document, not one per read-ahead fetch.
#[tokio::test(flavor = "multi_thread")]
async fn read_fetches_large_documents_one_at_a_time() {
    const MIB: usize = 1024 * 1024;
    let (fake, cfg) = Fake { docs: 6, doc_bytes: 12 * MIB, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "pad".into()]), filter: None };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 6);
    let got = sink.lock().unwrap();
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r[0], Cell::Text(format!("d{i:06}")));
        assert!(matches!(&r[1], Cell::Text(p) if p.len() == 12 * MIB), "row {i}");
    }
    let whole = fake.whole.lock().unwrap();
    assert_eq!(whole.len(), 6, "each document fetched whole once");
    for (i, a) in whole.iter().enumerate() {
        for b in &whole[i + 1..] {
            assert!(a.1 <= b.0 || b.1 <= a.0, "two whole documents fetched at once");
        }
    }
}

/// An empty column name can't go to Mango (it takes it as no field at
/// all): the documents' fields are looked at instead.
#[tokio::test(flavor = "multi_thread")]
async fn empty_column_name_is_looked_for_in_the_documents() {
    let (fake, cfg) = Fake { docs: 20, doc_bytes: 10, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "".into()]), filter: None };
    let e = s.read_batches(&spec, Arc::new(Mutex::new(Collect::default()))).await.unwrap_err();
    assert!(e.to_string().contains("ningún documento"), "{e}");
    assert_eq!(fake.finds.load(Ordering::SeqCst), 0, "asked Mango for an empty field name");
}

/// An asked-for column that no document has fails before any row goes out.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_column_fails_before_the_first_row() {
    let (fake, cfg) = Fake { docs: 3_000, doc_bytes: 10, ..Default::default() }.start().await;
    let driver = dbine_driver_couchdb::drivers().remove(0);
    let mut s = driver.connect(&cfg, Some("db")).await.expect("connect");
    let table = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table.clone(), columns: Some(vec!["_id".into(), "no_such".into()]), filter: None };
    let e = s.read_batches(&spec, sink.clone()).await.unwrap_err();
    assert!(e.to_string().contains("no_such"), "{e}");
    {
        let got = sink.lock().unwrap();
        assert!(got.columns.is_empty() && got.rows.is_empty(), "{} rows went out", got.rows.len());
    }
    assert_eq!(fake.max_reply.load(Ordering::SeqCst), 0, "documents were fetched");
    // A field some document has reads as usual.
    let spec = ReadSpec { table, columns: Some(vec!["_id".into(), "pad".into()]), filter: None };
    assert_eq!(s.read_batches(&spec, Arc::new(Mutex::new(Count::default()))).await.unwrap(), 3_000);
    assert!(fake.finds.load(Ordering::SeqCst) >= 2);
}

#[derive(Default)]
struct Count(usize);

impl BatchSink for Count {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.0 += batch.rows.len();
        Ok(())
    }
}

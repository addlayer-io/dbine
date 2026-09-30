//! Bulk transfer against real servers (ignored by default):
//!
//! ```sh
//! docker start dbine-test-solr        # solr:9 solr-precreate testcore, port 25522
//! DBINE_TEST_SOLR_URL=http://localhost:25522 cargo test -p dbine-driver-solr -- --ignored transfer
//! docker start dbine-test-solrcloud   # solr:9 solr-foreground -c, port 25523
//! DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523 cargo test -p dbine-driver-solr -- --ignored transfer
//! ```
//! Standalone: empties and fills the `testcore` core (adds the `payload_bin`
//! and `uid` fields it needs). SolrCloud: creates and drops its own
//! `dbine_xfer` collection.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

async fn session(url: &str) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "solr".into(), host: url.into(), ..Default::default() };
    dbine_driver_solr::drivers()[0].connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, dbine_driver::Error> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}
impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> io::Result<()> {
        self.cols = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);
#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: name.into() }
}

async fn read(s: &mut Box<dyn Session>, c: &str, columns: Option<Vec<&str>>, filter: Option<&str>) -> Collect {
    let sink = Arc::new(Mutex::new(Collect { cols: vec![], rows: vec![] }));
    let spec = ReadSpec {
        table: table(c),
        columns: columns.map(|c| c.into_iter().map(String::from).collect()),
        filter: filter.map(String::from),
    };
    let n = s.read_batches(&spec, sink.clone()).await.expect("read_batches");
    let out = std::mem::replace(&mut *sink.lock().unwrap(), Collect { cols: vec![], rows: vec![] });
    assert_eq!(n as usize, out.rows.len());
    out
}

async fn load(s: &mut Box<dyn Session>, c: &str, names: &[&str], rows: Vec<Vec<Cell>>) -> (u64, Vec<u64>) {
    let spec = LoadSpec {
        table: table(c),
        columns: names.iter().map(|s| s.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 20_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batches: Vec<RowBatch> = rows.chunks(1_000).map(|r| RowBatch { rows: r.to_vec(), bytes: 0 }).collect();
    let seen = Mutex::new(Vec::new());
    let progress = |n: u64| seen.lock().unwrap().push(n);
    let n = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &progress).await.expect("bulk_load");
    (n, seen.into_inner().unwrap())
}

const NAMES: [&str; 13] =
    ["id", "n_i", "big_l", "f_d", "ff_f", "ok_b", "when_dt", "title_t", "code_s", "tags_ss", "nums_is", "payload_bin", "uid"];

fn row(i: u64) -> Vec<Cell> {
    let n = |v: Cell| if i % 7 == 3 { Cell::Null } else { v };
    vec![
        Cell::Text(format!("r{i:06}")),
        n(Cell::Int(i as i64 - 25_000)),
        n(Cell::Int(i as i64 * 1_000_000_007)),
        n(Cell::Float(i as f64 * 0.5)),
        n(Cell::Float(1.5)),
        n(Cell::Bool(i.is_multiple_of(2))),
        n(Cell::DateTimeTz(format!("2024-01-{:02} 03:04:05.{:03}+00:00", i % 28 + 1, i % 900 + 100))),
        n(Cell::Text(format!("título «{i}» con ñ"))),
        n(Cell::Text(format!("c-{i}"))),
        n(Cell::Json(format!("[\"a{i}\",\"b\"]"))),
        n(Cell::Json(format!("[{i},-1]"))),
        n(Cell::Bytes(vec![(i % 256) as u8, 0, 255])),
        n(Cell::Uuid(format!("0f8fad5b-d9cb-469f-a165-{i:012}"))),
    ]
}

async fn round_trip(url: &str, c: &str) {
    let mut s = session(url).await;
    run(&mut s, &format!("POST /solr/{c}/update?commit=true\n{{\"delete\": {{\"query\": \"*:*\"}}}}")).await.unwrap();
    // The fields `_default` lacks: a UUID and a binary field.
    let _ = run(&mut s, &format!("POST /solr/{c}/schema\n{{\"add-field-type\": {{\"name\": \"uuid\", \"class\": \"solr.UUIDField\"}}}}")).await;
    for (f, t) in [("payload_bin", "binary"), ("uid", "uuid")] {
        let _ = run(&mut s, &format!("POST /solr/{c}/schema\n{{\"add-field\": {{\"name\": \"{f}\", \"type\": \"{t}\", \"stored\": true}}}}")).await;
    }

    // All types, NULLs (missing fields) and a large binary.
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
    let mut first = row(3);
    first[0] = Cell::Text("big".into());
    first[11] = Cell::Bytes(big.clone());
    first[6] = Cell::DateTimeTz("2024-01-01 01:30:00-03:00".into());
    let (n, _) = load(&mut s, c, &NAMES, vec![first, row(1)]).await;
    assert_eq!(n, 2);
    let got = read(&mut s, c, Some(NAMES.to_vec()), None).await;
    assert_eq!(got.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), NAMES);
    assert_eq!(got.cols[9].type_name, "strings[]");
    assert_eq!(got.cols[11].type_name, "binary");
    let big_row = &got.rows[0];
    assert_eq!(big_row[0], Cell::Text("big".into()));
    assert_eq!(big_row[11], Cell::Bytes(big));
    assert_eq!(big_row[6], Cell::DateTimeTz("2024-01-01 04:30:00+00:00".into()));
    assert!(big_row[1..6].iter().all(|c| *c == Cell::Null));
    assert_eq!(got.rows[1], row(1));
    // Columns in the asked order; a filter the engine applies (fq).
    let got = read(&mut s, c, Some(vec!["uid", "id"]), Some("id:r000001")).await;
    assert_eq!(got.rows, vec![vec![row(1)[12].clone(), Cell::Text("r000001".into())]]);
    // Every column by default, the uniqueKey included.
    let got = read(&mut s, c, None, None).await;
    assert!(got.cols.iter().any(|c| c.name == "id") && got.cols.iter().any(|c| c.name == "payload_bin"));

    // 50k rows: load, read back, compare.
    run(&mut s, &format!("POST /solr/{c}/update?commit=true\n{{\"delete\": {{\"query\": \"*:*\"}}}}")).await.unwrap();
    const N: u64 = 50_000;
    let rows: Vec<Vec<Cell>> = (0..N).map(row).collect();
    let t = Instant::now();
    let (n, progress) = load(&mut s, c, &NAMES, rows.clone()).await;
    let secs = t.elapsed().as_secs_f64();
    assert_eq!(n, N);
    assert_eq!(progress.last(), Some(&N));
    assert!(progress.windows(2).all(|w| w[0] < w[1]));
    eprintln!("{c}: bulk_load {N} filas en {secs:.2}s = {:.0} filas/s", N as f64 / secs);
    let t = Instant::now();
    let got = read(&mut s, c, Some(NAMES.to_vec()), None).await;
    let secs = t.elapsed().as_secs_f64();
    eprintln!("{c}: read_batches {N} filas en {secs:.2}s = {:.0} filas/s", N as f64 / secs);
    assert_eq!(got.rows.len(), N as usize);
    for (a, b) in got.rows.iter().zip(&rows) {
        assert_eq!(a, b);
    }
}

async fn try_load(
    s: &mut Box<dyn Session>,
    c: &str,
    names: &[&str],
    rows: Vec<Vec<Cell>>,
    commit_rows: u64,
) -> (Result<u64, dbine_driver::Error>, Vec<u64>) {
    let spec = LoadSpec {
        table: table(c),
        columns: names.iter().map(|s| s.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batches: Vec<RowBatch> = rows.chunks(500).map(|r| RowBatch { rows: r.to_vec(), bytes: 0 }).collect();
    let seen = Mutex::new(Vec::new());
    let progress = |n: u64| seen.lock().unwrap().push(n);
    let r = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &progress).await;
    (r, seen.into_inner().unwrap())
}

async fn count(s: &mut Box<dyn Session>, c: &str) -> usize {
    // A commit first: whatever landed is visible.
    run(s, &format!("GET /solr/{c}/update?commit=true")).await.unwrap();
    read(s, c, Some(vec!["id"]), None).await.rows.len()
}

async fn clear(s: &mut Box<dyn Session>, c: &str) {
    run(s, &format!("POST /solr/{c}/update?commit=true\n{{\"delete\": {{\"query\": \"*:*\"}}}}")).await.unwrap();
}

/// The verifier's findings, against a real server.
async fn regressions(url: &str, c: &str) {
    let mut s = session(url).await;
    clear(&mut s, c).await;
    let id = |i: u64| Cell::Text(format!("x{i:05}"));

    // Values Solr would truncate (12.50 → 12) or wrap (3e9 → -1294967296)
    // are errors, and nothing is left behind.
    for bad in [Cell::Decimal("12.50".into()), Cell::Float(-7.99), Cell::Int(3_000_000_000)] {
        let (r, _) = try_load(&mut s, c, &["id", "n_i"], vec![vec![id(1), Cell::Int(1)], vec![id(2), bad.clone()]], 20_000).await;
        assert!(r.is_err(), "{bad:?} no debería cargarse");
        assert_eq!(count(&mut s, c).await, 0, "{bad:?}");
    }
    let (r, _) = try_load(&mut s, c, &["id", "big_l"], vec![vec![id(1), Cell::Float(1.9)]], 20_000).await;
    assert!(r.is_err());
    // What fits goes in exactly.
    let (r, _) = try_load(&mut s, c, &["id", "n_i", "big_l"], vec![vec![id(1), Cell::Decimal("-12.00".into()), Cell::Int(3_000_000_000)]], 20_000).await;
    assert_eq!(r.unwrap(), 1);
    let got = read(&mut s, c, Some(vec!["id", "n_i", "big_l"]), None).await;
    assert_eq!(got.rows, vec![vec![id(1), Cell::Int(-12), Cell::Int(3_000_000_000)]]);
    clear(&mut s, c).await;

    // Commits every `commit_rows`, progress is committed rows (a prefix),
    // and a failure deletes the uncommitted window's documents.
    let mut rows: Vec<Vec<Cell>> = (0..2_600).map(|i| vec![id(i), Cell::Int(i as i64)]).collect();
    rows[2_500][1] = Cell::Decimal("0.5".into());
    let (r, progress) = try_load(&mut s, c, &["id", "n_i"], rows, 1_000).await;
    assert!(r.is_err());
    assert_eq!(progress, vec![1_000, 2_000]);
    assert_eq!(count(&mut s, c).await, 2_000);
    let got = read(&mut s, c, Some(vec!["id"]), None).await;
    assert!(got.rows.iter().all(|r| matches!(&r[0], Cell::Text(t) if t.as_str() < "x02000")));
    clear(&mut s, c).await;
    // Same, with a request Solr rejects halfway (the documents before the
    // bad one were indexed already): they are deleted too.
    let mut rows: Vec<Vec<Cell>> = (0..2_600).map(|i| vec![id(i), Cell::Int(i as i64)]).collect();
    rows[2_500][1] = Cell::Text("no es un número".into());
    let (r, progress) = try_load(&mut s, c, &["id", "n_i"], rows, 1_000).await;
    assert!(r.is_err());
    assert_eq!(progress, vec![1_000, 2_000]);
    assert_eq!(count(&mut s, c).await, 2_000);
    clear(&mut s, c).await;

    // Refused engine limits: an empty list, sub-millisecond digits.
    let (r, _) = try_load(&mut s, c, &["id", "tags_ss"], vec![vec![id(1), Cell::Json("[]".into())]], 20_000).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    let (r, _) = try_load(&mut s, c, &["id", "when_dt"], vec![vec![id(1), Cell::DateTime("2024-01-02 03:04:05.123456".into())]], 20_000).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    // Without the key the load can't be undone: refused up front.
    let (r, _) = try_load(&mut s, c, &["n_i"], vec![vec![Cell::Int(1)]], 20_000).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    assert_eq!(count(&mut s, c).await, 0);

    // copyField destinations: a Solr copy carries them, Solr fills them
    // again; they must not end up doubled.
    let _ = run(&mut s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"title_t\", \"dest\": \"copy_ss\"}}}}")).await;
    let (r, _) = try_load(
        &mut s,
        c,
        &["id", "title_t", "copy_ss"],
        vec![
            vec![id(1), Cell::Text("uno".into()), Cell::Json("[\"uno\"]".into())],
            // No source value: the destination is written as it came.
            vec![id(2), Cell::Null, Cell::Json("[\"solo\"]".into())],
        ],
        20_000,
    )
    .await;
    assert_eq!(r.unwrap(), 2);
    let got = read(&mut s, c, Some(vec!["id", "copy_ss"]), None).await;
    run(&mut s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"title_t\", \"dest\": \"copy_ss\"}}}}")).await.unwrap();
    assert_eq!(
        got.rows,
        vec![vec![id(1), Cell::Json("[\"uno\"]".into())], vec![id(2), Cell::Json("[\"solo\"]".into())]]
    );

    // A column the collection can't have is an error, not NULLs.
    let sink = Arc::new(Mutex::new(Collect { cols: vec![], rows: vec![] }));
    let spec = ReadSpec { table: table(c), columns: Some(vec!["id".into(), "no_such_field".into()]), filter: None };
    assert!(s.read_batches(&spec, sink).await.is_err());
    clear(&mut s, c).await;

    round2(&mut s, c).await;
}

/// The second verification's findings.
async fn round2(s: &mut Box<dyn Session>, c: &str) {
    let t = |v: &str| Cell::Text(v.into());
    // 1. A document that existed before the load is never replaced, and a
    // failed load never deletes it.
    run(s, &format!("POST /solr/{c}/update?commit=true\n[{{\"id\": \"keep\", \"n_i\": 42, \"title_t\": \"original\"}}]")).await.unwrap();
    let original = vec![vec![t("keep"), Cell::Int(42), t("original")]];
    let cols = Some(vec!["id", "n_i", "title_t"]);
    let (r, _) = try_load(
        s,
        c,
        &["id", "n_i"],
        vec![vec![t("new1"), Cell::Int(1)], vec![t("keep"), Cell::Int(5)], vec![t("bad"), Cell::Decimal("0.5".into())]],
        20_000,
    )
    .await;
    assert!(r.is_err());
    assert_eq!(count(s, c).await, 1);
    assert_eq!(read(s, c, cols.clone(), None).await.rows, original);
    // Loading over it (no other error) is refused too: it stays whole.
    let (r, _) = try_load(s, c, &["id", "n_i"], vec![vec![t("keep"), Cell::Int(5)]], 20_000).await;
    assert!(matches!(&r, Err(e) if e.to_string().contains("keep")), "{r:?}");
    assert_eq!(read(s, c, cols.clone(), None).await.rows, original);
    clear(s, c).await;

    // 2. A key repeated in the input is an error and never undoes rows of
    // a committed window: progress stays a committed prefix.
    let (r, progress) = try_load(
        s,
        c,
        &["id", "n_i"],
        vec![
            vec![t("k1"), Cell::Int(1)],
            vec![t("k2"), Cell::Int(2)],
            vec![t("k1"), Cell::Int(3)],
            vec![t("k3"), Cell::Decimal("0.5".into())],
        ],
        2,
    )
    .await;
    assert!(r.is_err());
    assert_eq!(progress, vec![2]);
    // Window commits don't open a searcher: count() commits first.
    assert_eq!(count(s, c).await, 2);
    assert_eq!(read(s, c, Some(vec!["id", "n_i"]), None).await.rows, vec![vec![t("k1"), Cell::Int(1)], vec![t("k2"), Cell::Int(2)]]);
    clear(s, c).await;
    // Repeated in a later window, with no other error: found before
    // writing, the committed window stays as it was.
    let (r, progress) =
        try_load(s, c, &["id", "n_i"], vec![vec![t("k1"), Cell::Int(1)], vec![t("k2"), Cell::Int(2)], vec![t("k1"), Cell::Int(3)]], 2).await;
    assert!(matches!(&r, Err(e) if e.to_string().contains("k1")), "{r:?}");
    assert_eq!(progress, vec![2]);
    // Window commits don't open a searcher: count() commits first.
    assert_eq!(count(s, c).await, 2);
    assert_eq!(read(s, c, Some(vec!["id", "n_i"]), None).await.rows, vec![vec![t("k1"), Cell::Int(1)], vec![t("k2"), Cell::Int(2)]]);
    clear(s, c).await;
    // Repeated inside one window: an error, nothing left.
    let (r, _) = try_load(s, c, &["id", "n_i"], vec![vec![t("k1"), Cell::Int(1)], vec![t("k1"), Cell::Int(2)]], 20_000).await;
    assert!(r.is_err());
    assert_eq!(count(s, c).await, 0);

    // 3. A copyField destination with its own value: refused, nothing lost
    // silently; with exactly the copy, loaded once.
    let _ = run(s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"title_t\", \"dest\": \"alt_s\"}}}}")).await;
    let (r, _) = try_load(s, c, &["id", "title_t", "alt_s"], vec![vec![t("1"), t("A"), t("B-distinto")]], 20_000).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Unsupported(m)) if m.contains("alt_s")), "{r:?}");
    assert_eq!(count(s, c).await, 0);
    let (r, _) = try_load(s, c, &["id", "title_t", "alt_s"], vec![vec![t("1"), t("A"), t("A")]], 20_000).await;
    let got = read(s, c, Some(vec!["id", "alt_s"]), None).await;
    run(s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"title_t\", \"dest\": \"alt_s\"}}}}")).await.unwrap();
    assert_eq!(r.unwrap(), 1);
    assert_eq!(got.rows, vec![vec![t("1"), t("A")]]);
    clear(s, c).await;

    // 5. A field the schema doesn't define (schemaless would guess plongs
    // from 5 and store 12.75 as 12): refused before writing anything.
    let (r, _) =
        try_load(s, c, &["id", "precio_x"], vec![vec![t("1"), Cell::Int(5)], vec![t("2"), Cell::Float(12.75)]], 20_000).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Unsupported(m)) if m.contains("precio_x")), "{r:?}");
    assert_eq!(count(s, c).await, 0);
    // A pdoubles item with more digits than a double holds.
    let (r, _) = try_load(s, c, &["id", "xs_ds"], vec![vec![t("1"), Cell::Json("[0.12345678901234567890]".into())]], 20_000).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(count(s, c).await, 0);

    round3(s, c).await;
}

/// The third verification's findings.
async fn round3(s: &mut Box<dyn Session>, c: &str) {
    let t = |v: &str| Cell::Text(v.into());
    let j = |v: &str| Cell::Json(v.into());
    // 1-2. Numeric text in a list and JSON scalars: Solr would round them.
    for (field, v) in [
        ("m_fs", j("[\"0.1234567891234\"]")),
        ("m_ds", j("[\"0.12345678901234567890\"]")),
        ("one_f", j("0.1234567891234")),
        ("one_f", j("\"0.1234567891234\"")),
    ] {
        let (r, _) = try_load(s, c, &["id", field], vec![vec![t("1"), v.clone()]], 20_000).await;
        assert!(r.is_err(), "{field} {v:?}: {r:?}");
        assert_eq!(count(s, c).await, 0);
    }
    // What fits still goes in exactly.
    let (r, _) = try_load(s, c, &["id", "m_fs", "one_f"], vec![vec![t("1"), j("[\"0.5\", 2]"), j("0.25")]], 20_000).await;
    assert_eq!(r.unwrap(), 1);
    let got = read(s, c, Some(vec!["id", "m_fs", "one_f"]), None).await;
    assert_eq!(got.rows, vec![vec![t("1"), j("[0.5,2.0]"), Cell::Float(0.25)]]);
    clear(s, c).await;
    // 3. Null items would be dropped.
    for v in ["[1,null,2]", "[null]"] {
        let (r, _) = try_load(s, c, &["id", "m_is"], vec![vec![t("1"), j(v)]], 20_000).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{v}: {r:?}");
        assert_eq!(count(s, c).await, 0);
    }
    // 4. A field neither stored nor docValues keeps nothing.
    let (r, _) = try_load(s, c, &["id", "ignored_note"], vec![vec![t("1"), t("nota")]], 20_000).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Unsupported(m)) if m.contains("ignored_note")), "{r:?}");
    assert_eq!(count(s, c).await, 0);
    // 5. maxChars cuts UTF-16 units: half an emoji isn't the row's value.
    let _ = run(s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"emo_t\", \"dest\": \"emo_s\", \"maxChars\": 1}}}}")).await;
    let (r, _) = try_load(s, c, &["id", "emo_t", "emo_s"], vec![vec![t("1"), t("😀😀"), t("😀")]], 20_000).await;
    let n = count(s, c).await;
    run(s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"emo_t\", \"dest\": \"emo_s\"}}}}")).await.unwrap();
    assert!(matches!(&r, Err(dbine_driver::Error::Unsupported(m)) if m.contains("emo_s")), "{r:?}");
    assert_eq!(n, 0);

    round4(s, c).await;
}

/// The fourth verification's findings.
async fn round4(s: &mut Box<dyn Session>, c: &str) {
    let t = |v: &str| Cell::Text(v.into());
    let j = |v: &str| Cell::Json(v.into());
    clear(s, c).await;
    let _ = run(s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"px_d\", \"dest\": \"px_s\"}}}}")).await;
    // X1a. Solr copies the double as Java prints it (`1.0E-7`): a row
    // holding `1e-7` in the destination is refused, not changed.
    let (x1a, _) = try_load(s, c, &["id", "px_d", "px_s"], vec![vec![t("x1a"), Cell::Float(1e-7), t("1e-7")]], 20_000).await;
    let x1a_left = count(s, c).await;
    // X1. Documents Solr indexed itself (`100.0` and `100` give different
    // copies), read and loaded again into the same schema: loaded, and
    // they read back the same.
    run(
        s,
        &format!(
            "POST /solr/{c}/update?commit=true\n[{{\"id\": \"n1\", \"px_d\": 100.0}}, {{\"id\": \"n2\", \"px_d\": 0.0000001}}, {{\"id\": \"n3\", \"px_d\": 2.5}}, {{\"id\": \"n4\", \"px_d\": 100}}, {{\"id\": \"n5\", \"px_d\": -0.0}}]"
        ),
    )
    .await
    .unwrap();
    let native = read(s, c, Some(vec!["id", "px_d", "px_s"]), None).await.rows;
    clear(s, c).await;
    let (x1, _) = try_load(s, c, &["id", "px_d", "px_s"], native.clone(), 20_000).await;
    let reloaded = read(s, c, Some(vec!["id", "px_d", "px_s"]), None).await.rows;
    clear(s, c).await;
    run(s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"px_d\", \"dest\": \"px_s\"}}}}")).await.unwrap();
    assert!(matches!(&x1a, Err(dbine_driver::Error::Unsupported(m)) if m.contains("px_s")), "{x1a:?}");
    assert_eq!(x1a_left, 0);
    assert_eq!(
        native.iter().map(|r| r[2].clone()).collect::<Vec<_>>(),
        vec![t("100.0"), t("1.0E-7"), t("2.5"), t("100"), t("-0.0")]
    );
    assert_eq!(x1.unwrap(), 5);
    assert_eq!(reloaded, native);
    assert!(matches!(reloaded[4][1], Cell::Float(z) if z == 0.0 && z.is_sign_negative()), "{:?}", reloaded[4]);

    // X2. A copyField destination left NULL would be filled by Solr.
    let _ = run(s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"title_t\", \"dest\": \"alt_s\"}}}}")).await;
    let (x2, _) = try_load(s, c, &["id", "title_t", "alt_s"], vec![vec![t("1"), t("A"), Cell::Null]], 20_000).await;
    let x2_left = count(s, c).await;
    run(s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"title_t\", \"dest\": \"alt_s\"}}}}")).await.unwrap();
    assert!(matches!(&x2, Err(dbine_driver::Error::Unsupported(m)) if m.contains("alt_s") && m.contains("NULL")), "{x2:?}");
    assert_eq!(x2_left, 0);

    // 4. A negative zero keeps its sign, alone and in a list.
    let (r, _) = try_load(s, c, &["id", "nzs_fs", "nz_d"], vec![vec![t("1"), j("[16777216, -0.0]"), Cell::Float(-0.0)]], 20_000).await;
    assert_eq!(r.unwrap(), 1);
    let got = read(s, c, Some(vec!["id", "nzs_fs", "nz_d"]), None).await.rows;
    assert_eq!(got[0][1], j("[16777216.0,-0.0]"));
    assert!(matches!(got[0][2], Cell::Float(z) if z == 0.0 && z.is_sign_negative()), "{:?}", got[0]);
    clear(s, c).await;
    // A pfloat takes `1e-45` as the float Solr prints as `1.4E-45`: refused.
    let (r, _) = try_load(s, c, &["id", "sub_f"], vec![vec![t("1"), Cell::Decimal("1e-45".into())]], 20_000).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(count(s, c).await, 0);
    // What Solr prints for it loads again from Java 19; Java 17 prints
    // subnormal floats with digits DBine can't anticipate: refused there,
    // with that reason, never changed.
    run(s, &format!("POST /solr/{c}/update?commit=true\n[{{\"id\": \"s1\", \"sub_f\": 1.4E-45}}]")).await.unwrap();
    let native = read(s, c, Some(vec!["id", "sub_f"]), None).await.rows;
    assert_eq!(native, vec![vec![t("s1"), Cell::Float(1.4e-45)]]);
    clear(s, c).await;
    let (r, _) = try_load(s, c, &["id", "sub_f"], native.clone(), 20_000).await;
    match r {
        Ok(n) => {
            assert_eq!(n, 1);
            assert_eq!(read(s, c, Some(vec!["id", "sub_f"]), None).await.rows, native);
        }
        Err(e) => {
            assert!(e.to_string().contains("versión de Java"), "{e}");
            assert_eq!(count(s, c).await, 0);
        }
    }
    clear(s, c).await;

    // 5. A load failing before sending anything of its window leaves the
    // windows committed before it visible (no later commit needed).
    let mut rows: Vec<Vec<Cell>> = (0..25).map(|i| vec![t(&format!("w{i:02}")), Cell::Int(i)]).collect();
    rows[20][1] = Cell::Decimal("0.5".into());
    let (r, progress) = try_load(s, c, &["id", "n_i"], rows, 10).await;
    assert!(r.is_err());
    assert_eq!(progress, vec![10, 20]);
    assert_eq!(read(s, c, Some(vec!["id"]), None).await.rows.len(), 20);
    clear(s, c).await;

    round5(s, c).await;
}

/// The fifth verification's findings: Java 17 prints some doubles and
/// floats with more digits than the shortest (`2⁻²⁴` is
/// `5.9604644775390625E-8`). Whatever the server's Java, a value is either
/// kept exactly or refused, and what Solr indexed itself loads again.
async fn round5(s: &mut Box<dyn Session>, c: &str) {
    let t = |v: &str| Cell::Text(v.into());
    clear(s, c).await;
    let _ = run(s, &format!("POST /solr/{c}/schema\n{{\"add-copy-field\": {{\"source\": \"px_d\", \"dest\": \"px_s\"}}}}")).await;
    let powers = [2f64.powi(-24), 2f64.powi(-31), 2f64.powi(-44)];
    // Solr's own copies, indexed natively.
    let docs: Vec<String> = powers.iter().enumerate().map(|(i, p)| format!("{{\"id\": \"p{i}\", \"px_d\": {p:e}}}")).collect();
    run(s, &format!("POST /solr/{c}/update?commit=true\n[{}]", docs.join(","))).await.unwrap();
    let native = read(s, c, Some(vec!["id", "px_d", "px_s"]), None).await.rows;
    let java17 = native[0][2] == t("5.9604644775390625E-8");
    clear(s, c).await;
    // 1. Read and loaded again: loaded, and the same.
    let (r, _) = try_load(s, c, &["id", "px_d", "px_s"], native.clone(), 20_000).await;
    assert_eq!(r.unwrap(), 3);
    assert_eq!(read(s, c, Some(vec!["id", "px_d", "px_s"]), None).await.rows, native);
    clear(s, c).await;
    // 1. A row whose destination holds the shortest text: kept or refused.
    let shortest = ["5.960464477539063E-8", "4.656612873077393E-10", "5.684341886080802E-14"];
    for (i, (p, short)) in powers.iter().zip(shortest).enumerate() {
        let row = vec![t(&format!("q{i}")), Cell::Float(*p), t(short)];
        let (r, _) = try_load(s, c, &["id", "px_d", "px_s"], vec![row.clone()], 20_000).await;
        match r {
            Ok(n) => {
                assert!(!java17, "Java 17 cambia {short}");
                assert_eq!(n, 1);
                assert_eq!(read(s, c, Some(vec!["id", "px_d", "px_s"]), None).await.rows, vec![row]);
            }
            Err(e) => {
                assert!(matches!(&e, dbine_driver::Error::Unsupported(m) if m.contains("px_s")), "{e:?}");
                assert_eq!(count(s, c).await, 0);
            }
        }
        clear(s, c).await;
    }
    run(s, &format!("POST /solr/{c}/schema\n{{\"delete-copy-field\": {{\"source\": \"px_d\", \"dest\": \"px_s\"}}}}")).await.unwrap();

    // 2. A pfloat decimal that Java 17 prints with other digits.
    for dec in ["3.637979E-12", "1.2621775E-29"] {
        let (r, _) = try_load(s, c, &["id", "sub_f"], vec![vec![t("f"), Cell::Decimal(dec.into())]], 20_000).await;
        match r {
            Ok(n) => {
                assert!(!java17, "Java 17 cambia {dec}");
                assert_eq!(n, 1);
                let got = read(s, c, Some(vec!["id", "sub_f"]), None).await.rows;
                assert_eq!(got, vec![vec![t("f"), Cell::Float(dec.parse().unwrap())]]);
            }
            Err(e) => {
                assert!(matches!(&e, dbine_driver::Error::Query(m) if m.contains(dec)), "{e:?}");
                assert_eq!(count(s, c).await, 0);
            }
        }
        clear(s, c).await;
        // What Solr prints for that float loads again, unchanged.
        run(s, &format!("POST /solr/{c}/update?commit=true\n[{{\"id\": \"f\", \"sub_f\": {dec}}}]")).await.unwrap();
        let native = read(s, c, Some(vec!["id", "sub_f"]), None).await.rows;
        clear(s, c).await;
        let (r, _) = try_load(s, c, &["id", "sub_f"], native.clone(), 20_000).await;
        assert_eq!(r.unwrap(), 1);
        assert_eq!(read(s, c, Some(vec!["id", "sub_f"]), None).await.rows, native);
        clear(s, c).await;
    }

    // 3. A float into a string field keeps DBine's text, whatever the
    // server's Java.
    let rows = vec![vec![t("s1"), Cell::Float(1e-7)], vec![t("s2"), Cell::Float(2f64.powi(-24))], vec![t("s3"), Cell::Float(100.0)]];
    let (r, _) = try_load(s, c, &["id", "fx_s"], rows, 20_000).await;
    assert_eq!(r.unwrap(), 3);
    let got = read(s, c, Some(vec!["id", "fx_s"]), None).await.rows;
    assert_eq!(
        got,
        vec![vec![t("s1"), t("1e-7")], vec![t("s2"), t("5.960464477539063e-8")], vec![t("s3"), t("100.0")]]
    );
    clear(s, c).await;
}

/// Resident memory of this process, in KiB (`ps`, macOS and Linux).
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Discards the batches: only the reader's own memory counts.
struct Discard(u64);
impl BatchSink for Discard {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.0 += b.rows.len() as u64;
        Ok(())
    }
}

/// Problem 4: many small documents, then wide ones. The read holds about
/// one document at a time, not a page of them.
async fn read_memory(url: &str, c: &str) {
    let mut s = session(url).await;
    clear(&mut s, c).await;
    let small: Vec<Vec<Cell>> = (0..1_000).map(|i| vec![Cell::Text(format!("a{i:05}")), Cell::Null]).collect();
    load(&mut s, c, &["id", "payload_bin"], small).await;
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
    let wide: Vec<Vec<Cell>> = (0..30).map(|i| vec![Cell::Text(format!("b{i:05}")), Cell::Bytes(big.clone())]).collect();
    drop(big);
    load(&mut s, c, &["id", "payload_bin"], wide).await;
    let before = rss_kib();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut peak = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(rss_kib());
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            peak
        })
    };
    let sink = Arc::new(Mutex::new(Discard(0)));
    let spec = ReadSpec { table: table(c), columns: Some(vec!["id".into(), "payload_bin".into()]), filter: None };
    let n = s.read_batches(&spec, sink.clone()).await.unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak = sampler.join().unwrap();
    assert_eq!(n, 1_030);
    let grew = peak.saturating_sub(before) / 1024;
    eprintln!("{c}: lectura de 1.030 documentos (30 de 3 MiB): RSS {} MiB antes, pico +{grew} MiB", before / 1024);
    assert!(grew < 64, "la lectura creció {grew} MiB");
    clear(&mut s, c).await;
}

#[tokio::test]
#[ignore]
async fn transfer_standalone() {
    let url = std::env::var("DBINE_TEST_SOLR_URL").unwrap_or_else(|_| "http://localhost:25522".into());
    regressions(&url, "testcore").await;
    round_trip(&url, "testcore").await;
    read_memory(&url, "testcore").await;
}

#[tokio::test]
#[ignore]
async fn transfer_solrcloud() {
    let url = std::env::var("DBINE_TEST_SOLRCLOUD_URL").unwrap_or_else(|_| "http://localhost:25523".into());
    let mut s = session(&url).await;
    run(&mut s, "DELETE /solr/dbine_xfer?if_exists=true").await.unwrap();
    run(&mut s, "PUT /solr/dbine_xfer\n{\"numShards\": 2, \"replicationFactor\": 1}").await.unwrap();
    regressions(&url, "dbine_xfer").await;
    round_trip(&url, "dbine_xfer").await;
    read_memory(&url, "dbine_xfer").await;
    run(&mut s, "DELETE /solr/dbine_xfer").await.unwrap();
}

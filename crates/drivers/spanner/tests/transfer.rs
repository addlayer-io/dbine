//! Bulk load and typed read against the Cloud Spanner emulator (see
//! `integration.rs` for the container):
//! `DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 50_000;

async fn open(url: &str, read_only: bool) -> Box<dyn Session> {
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), read_only, ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url)] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.supports_bulk_load());
    d.connect(&cfg, None).await.unwrap()
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

fn batches(rows: Vec<Vec<Cell>>, per: usize) -> Batches {
    let mut out = Vec::new();
    let mut it = rows.into_iter().peekable();
    while it.peek().is_some() {
        out.push(RowBatch { rows: it.by_ref().take(per).collect(), bytes: 0 });
    }
    Batches(out.into_iter())
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

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
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

async fn read(s: &mut Box<dyn Session>, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

#[tokio::test]
#[ignore]
async fn transfer_types_and_nulls() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let mut s = open(&url, false).await;
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE xfer_types", 10, &mut out).await;
    run(
        &mut s,
        "CREATE TABLE xfer_types (id INT64 NOT NULL, b BOOL, i INT64, f FLOAT64, f32 FLOAT32, n NUMERIC, s STRING(MAX),
           bin BYTES(MAX), d DATE, ts TIMESTAMP, j JSON, ai ARRAY<INT64>, astr ARRAY<STRING(MAX)>, ab ARRAY<BYTES(MAX)>,
           ats ARRAY<TIMESTAMP>) PRIMARY KEY (id)",
    )
    .await;
    let cols = ["id", "b", "i", "f", "f32", "n", "s", "bin", "d", "ts", "j", "ai", "astr", "ab", "ats"];
    let big: Vec<u8> = (0..6 * 1024 * 1024).map(|i: u32| (i * 7 % 251) as u8).collect();
    let full = vec![
        Cell::Int(1),
        Cell::Bool(true),
        Cell::Int(i64::MIN),
        Cell::Float(f64::NAN),
        Cell::Float(1.5),
        Cell::Decimal("99999999999999999999999999999.999999999".into()),
        Cell::Text("O'Brien \"ñ\" 😀\n".into()),
        Cell::Bytes(big.clone()),
        Cell::Date("2024-02-29".into()),
        Cell::DateTimeTz("2024-01-31 23:30:00.123456789-03:00".into()),
        Cell::Json("{\"a\":[1,2,{\"b\":null}]}".into()),
        Cell::Json("[9223372036854775807,null,-1]".into()),
        Cell::Json("[\"x\",\"\",null]".into()),
        Cell::Json("[\"0x00FF\",null]".into()),
        Cell::Json("[\"0001-01-01 00:00:00+00:00\"]".into()),
    ];
    let mut nulls = vec![Cell::Int(2)];
    nulls.extend(std::iter::repeat_n(Cell::Null, cols.len() - 1));
    let extremes = vec![
        Cell::Int(3),
        Cell::Bool(false),
        Cell::Int(i64::MAX),
        Cell::Float(f64::NEG_INFINITY),
        Cell::Float(-0.25),
        Cell::Decimal("-0.000000001".into()),
        Cell::Text(String::new()),
        Cell::Bytes(Vec::new()),
        Cell::Date("0001-01-01".into()),
        Cell::DateTime("9999-12-31 23:59:59.999999999".into()),
        Cell::Json("null".into()),
        Cell::Json("[]".into()),
        Cell::Json("[]".into()),
        Cell::Json("[]".into()),
        Cell::Json("[]".into()),
    ];
    let reported = Mutex::new(Vec::new());
    let n = s
        .bulk_load(&load_spec("xfer_types", &cols), &[], &mut batches(vec![full, nulls, extremes], 2), &|r| reported.lock().unwrap().push(r))
        .await
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(reported.lock().unwrap().last(), Some(&3));

    // Insert never overwrites: the same key again is refused.
    let dup = vec![vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null]];
    let e = s.bulk_load(&load_spec("xfer_types", &cols), &[], &mut batches(dup, 10), &|_| {}).await.unwrap_err();
    println!("duplicate: {e}");

    // Columns in the requested order (not the table's).
    let mut wanted: Vec<String> = cols.iter().rev().map(|c| c.to_string()).collect();
    wanted.retain(|c| c != "id");
    wanted.insert(0, "id".into());
    let got = read(&mut s, ReadSpec { table: table("xfer_types"), columns: Some(wanted.clone()), filter: None }).await;
    assert_eq!(got.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), wanted);
    assert_eq!(got.columns[1].type_name, "ARRAY<TIMESTAMP>");
    assert!(!got.columns[0].nullable);
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    let by = |r: &Vec<Cell>, c: &str| r[wanted.iter().position(|w| w == c).unwrap()].clone();
    let r = &rows[0];
    assert_eq!(by(r, "b"), Cell::Bool(true));
    assert_eq!(by(r, "i"), Cell::Int(i64::MIN));
    assert!(matches!(by(r, "f"), Cell::Float(f) if f.is_nan()));
    assert_eq!(by(r, "f32"), Cell::Float(1.5));
    assert_eq!(by(r, "n"), Cell::Decimal("99999999999999999999999999999.999999999".into()));
    assert_eq!(by(r, "s"), Cell::Text("O'Brien \"ñ\" 😀\n".into()));
    assert_eq!(by(r, "bin"), Cell::Bytes(big), "the large binary comes back whole");
    assert_eq!(by(r, "d"), Cell::Date("2024-02-29".into()));
    assert_eq!(by(r, "ts"), Cell::DateTimeTz("2024-02-01 02:30:00.123456789+00:00".into()));
    let Cell::Json(j) = by(r, "j") else { panic!() };
    assert_eq!(serde_json::from_str::<serde_json::Value>(&j).unwrap(), json!({ "a": [1, 2, { "b": null }] }));
    assert_eq!(by(r, "ai"), Cell::Json("[9223372036854775807,null,-1]".into()));
    assert_eq!(by(r, "astr"), Cell::Json("[\"x\",\"\",null]".into()));
    assert_eq!(by(r, "ab"), Cell::Json("[\"0x00FF\",null]".into()));
    assert_eq!(by(r, "ats"), Cell::Json("[\"0001-01-01T00:00:00Z\"]".into()));
    assert!(rows[1][1..].iter().all(|c| *c == Cell::Null), "{:?}", rows[1]);
    let r = &rows[2];
    assert_eq!(by(r, "i"), Cell::Int(i64::MAX));
    assert_eq!(by(r, "f"), Cell::Float(f64::NEG_INFINITY));
    assert_eq!(by(r, "n"), Cell::Decimal("-0.000000001".into()));
    assert_eq!(by(r, "s"), Cell::Text(String::new()));
    assert_eq!(by(r, "bin"), Cell::Bytes(Vec::new()));
    assert_eq!(by(r, "d"), Cell::Date("0001-01-01".into()));
    assert_eq!(by(r, "ts"), Cell::DateTimeTz("9999-12-31 23:59:59.999999999+00:00".into()));
    assert_eq!(by(r, "j"), Cell::Json("null".into()));
    assert_eq!(by(r, "ai"), Cell::Json("[]".into()));

    // A filter goes to the WHERE; a bad one is an error.
    let got = read(&mut s, ReadSpec { table: table("xfer_types"), columns: Some(vec!["id".into()]), filter: Some("id >= 2".into()) }).await;
    assert_eq!(got.rows.len(), 2);
    // (The emulator's gateway loses the reason: "failed to marshal error message".)
    let e = s
        .read_batches(&ReadSpec { table: table("xfer_types"), columns: None, filter: Some("nope = 1".into()) }, Arc::new(Mutex::new(Collect::default())))
        .await
        .unwrap_err();
    println!("bad filter: {e}");
    assert!(s
        .read_batches(&ReadSpec { table: table("xfer_types"), columns: Some(vec!["zz".into()]), filter: None }, Arc::new(Mutex::new(Collect::default())))
        .await
        .is_err());

    // A read-only connection reads, and never loads.
    let mut ro = open(&url, true).await;
    let got = read(&mut ro, ReadSpec { table: table("xfer_types"), columns: None, filter: None }).await;
    assert_eq!(got.rows.len(), 3);
    assert!(ro.bulk_load(&load_spec("xfer_types", &["id"]), &[], &mut batches(vec![vec![Cell::Int(9)]], 1), &|_| {}).await.is_err());
}

#[tokio::test]
#[ignore]
async fn transfer_50k_rows() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let mut s = open(&url, false).await;
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP INDEX xfer_big_name; DROP TABLE xfer_big", 10, &mut out).await;
    let _ = s.execute("DROP TABLE xfer_big", 10, &mut out).await;
    run(
        &mut s,
        "CREATE TABLE xfer_big (id INT64 NOT NULL, name STRING(100), amount NUMERIC, ts TIMESTAMP, flag BOOL, raw BYTES(64)) PRIMARY KEY (id);
         CREATE INDEX xfer_big_name ON xfer_big (name)",
    )
    .await;
    let row = |i: usize| {
        vec![
            Cell::Int(i as i64),
            if i.is_multiple_of(10) { Cell::Null } else { Cell::Text(format!("fila {i} ñ")) },
            Cell::Decimal(format!("{}.{:02}", i, i % 100)),
            Cell::DateTimeTz(format!("2024-01-01 00:00:{:02}.{:09}+00:00", i % 60, i)),
            Cell::Bool(i.is_multiple_of(2)),
            Cell::Bytes((i as u32).to_be_bytes().to_vec()),
        ]
    };
    let rows: Vec<Vec<Cell>> = (0..ROWS).map(row).collect();
    let commits = Mutex::new(Vec::new());
    let t = Instant::now();
    let n = s
        .bulk_load(&load_spec("xfer_big", &["id", "name", "amount", "ts", "flag", "raw"]), &[], &mut batches(rows.clone(), 1000), &|r| {
            commits.lock().unwrap().push(r)
        })
        .await
        .unwrap();
    let load = t.elapsed();
    assert_eq!(n as usize, ROWS);
    let commits = commits.into_inner().unwrap();
    assert_eq!(commits.last(), Some(&(ROWS as u64)));
    assert!(commits.windows(2).all(|w| w[0] < w[1]));

    let t = Instant::now();
    let got = read(&mut s, ReadSpec { table: table("xfer_big"), columns: None, filter: None }).await;
    let read_time = t.elapsed();
    let mut back = got.rows;
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    let expected: Vec<Vec<Cell>> = rows
        .into_iter()
        .map(|mut r| {
            // Spanner keeps NUMERIC normalized and timestamps in UTC.
            if let Cell::Decimal(d) = &r[2] {
                let t = d.trim_end_matches('0').trim_end_matches('.').to_string();
                r[2] = Cell::Decimal(t);
            }
            if let Cell::DateTimeTz(ts) = &r[3] {
                let (base, frac) = ts.trim_end_matches("+00:00").split_once('.').unwrap();
                let frac = frac.trim_end_matches('0');
                r[3] = Cell::DateTimeTz(if frac.is_empty() { format!("{base}+00:00") } else { format!("{base}.{frac}+00:00") });
            }
            r
        })
        .collect();
    assert_eq!(back.len(), expected.len());
    for (a, b) in back.iter().zip(&expected) {
        assert_eq!(a, b);
    }
    println!(
        "spanner: {} commits; load {ROWS} rows in {:.2?} ({:.0} rows/s); read in {:.2?} ({:.0} rows/s)",
        commits.len(),
        load,
        ROWS as f64 / load.as_secs_f64(),
        read_time,
        ROWS as f64 / read_time.as_secs_f64()
    );
}

/// Values that used to panic, be guessed or be sent beyond NUMERIC's scale.
#[tokio::test]
#[ignore]
async fn transfer_value_edges() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let mut s = open(&url, false).await;
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE xfer_edges", 10, &mut out).await;
    run(&mut s, "CREATE TABLE xfer_edges (id INT64 NOT NULL, bin BYTES(MAX), n NUMERIC, ts TIMESTAMP) PRIMARY KEY (id)").await;
    let cols = ["id", "bin", "n", "ts"];
    let rows = vec![
        // Text that looks like hex is still text; a decimal(38,18) that fits.
        vec![Cell::Int(1), Cell::Text("0xCAFE".into()), Cell::Decimal("1.500000000000000000".into()), Cell::Null],
        vec![Cell::Int(2), Cell::Json("[1]".into()), Cell::Decimal("-0.000000001000".into()), Cell::Null],
    ];
    assert_eq!(s.bulk_load(&load_spec("xfer_edges", &cols), &[], &mut batches(rows, 10), &|_| {}).await.unwrap(), 2);
    let got = read(&mut s, ReadSpec { table: table("xfer_edges"), columns: None, filter: None }).await;
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(rows[0][1], Cell::Bytes(b"0xCAFE".to_vec()));
    assert_eq!(rows[0][2], Cell::Decimal("1.5".into()));
    assert_eq!(rows[1][1], Cell::Bytes(b"[1]".to_vec()));
    assert_eq!(rows[1][2], Cell::Decimal("-0.000000001".into()));

    // A dirty timestamp is an error naming the column, never a panic.
    let bad = vec![vec![Cell::Int(3), Cell::Null, Cell::Null, Cell::Text("2024-01-3ñ 13:45:00".into())]];
    let e = s.bulk_load(&load_spec("xfer_edges", &cols), &[], &mut batches(bad, 10), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("TIMESTAMP"), "{e}");
    // More decimals than NUMERIC keeps: refused before anything is sent.
    let bad = vec![vec![Cell::Int(4), Cell::Null, Cell::Decimal("0.1234567891".into()), Cell::Null]];
    let e = s.bulk_load(&load_spec("xfer_edges", &cols), &[], &mut batches(bad, 10), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("9 decimales"), "{e}");
    // Impossible dates and leap seconds are refused whatever the offset,
    // never moved to another date.
    // So are offsets no zone has, malformed parts and values outside
    // TIMESTAMP's range once in UTC.
    for (id, ts) in [
        (5, "2024-02-30 10:00:00+01:00"),
        (6, "2023-02-29 12:00:00-03:00"),
        (7, "2024-02-30 10:00:00+00:00"),
        (8, "2024-06-30 23:59:60+00:30"),
        (9, "2024-01-01 00:00:00+99:99"),
        (10, "2024-01-01 00:00:00-00:60"),
        (11, "2024-01-01 00:00:00+24:00"),
        (12, "2024-01-01 00:00:00+1999"),
        (13, "2024-1-011 00:00:00"),
        (14, "0000-01-01 00:00:00"),
        (15, "9999-12-31 23:00:00-02:00"),
    ] {
        let bad = vec![vec![Cell::Int(id), Cell::Null, Cell::Null, Cell::DateTimeTz(ts.into())]];
        let e = s.bulk_load(&load_spec("xfer_edges", &cols), &[], &mut batches(bad, 10), &|_| {}).await.unwrap_err();
        assert!(e.to_string().contains("TIMESTAMP"), "{ts}: {e}");
    }
    let got = read(&mut s, ReadSpec { table: table("xfer_edges"), columns: Some(vec!["id".into()]), filter: None }).await;
    assert_eq!(got.rows.len(), 2);
}

/// A duplicate key or an oversized value refuses its window whole: no
/// valid half of it is committed around the bad row.
#[tokio::test]
#[ignore]
async fn transfer_data_errors_commit_nothing_of_the_window() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let mut s = open(&url, false).await;
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE xfer_window", 10, &mut out).await;
    run(&mut s, "CREATE TABLE xfer_window (id INT64 NOT NULL, s STRING(4)) PRIMARY KEY (id)").await;
    run(&mut s, "INSERT INTO xfer_window (id, s) VALUES (50, 'x')").await;
    let cols = ["id", "s"];
    let count = |c: Collect| c.rows.len();
    // Duplicate key in the middle of the window.
    let rows: Vec<Vec<Cell>> = (1..=100).map(|i| vec![Cell::Int(i), Cell::Text("ok".into())]).collect();
    assert!(s.bulk_load(&load_spec("xfer_window", &cols), &[], &mut batches(rows, 10), &|_| {}).await.is_err());
    assert_eq!(count(read(&mut s, ReadSpec { table: table("xfer_window"), columns: None, filter: None }).await), 1);
    // One value over its column's length.
    let rows: Vec<Vec<Cell>> =
        (100..=200).map(|i| vec![Cell::Int(i), Cell::Text(if i == 150 { "too large".into() } else { "ok".into() })]).collect();
    assert!(s.bulk_load(&load_spec("xfer_window", &cols), &[], &mut batches(rows, 10), &|_| {}).await.is_err());
    assert_eq!(count(read(&mut s, ReadSpec { table: table("xfer_window"), columns: None, filter: None }).await), 1);
}

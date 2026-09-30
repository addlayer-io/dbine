//! Bulk transfer against bigquery-emulator:
//!   docker run -d --name dbine-test-bigquery -p 25302:9050 ghcr.io/goccy/bigquery-emulator --project=test --dataset=ds1
//!   DBINE_TEST_BIGQUERY_URL=http://localhost:25302 cargo test -p dbine-driver-bigquery --test transfer -- --ignored transfer --nocapture
//!
//! Emulator quirks (real BigQuery decodes both): it stores a `BYTES` value
//! of a load as its base64 text instead of decoding it, and loads the
//! `"NaN"` float as NULL. The byte comparisons accept the first; NaN isn't
//! loaded here.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

async fn open() -> Option<Box<dyn Session>> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    Some(dbine_driver_bigquery::drivers().pop().unwrap().connect(&c, Some("ds1")).await.unwrap())
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("ds1".into()), name: name.into() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Rows(std::vec::IntoIter<Vec<Cell>>);

#[dbine_driver::async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        let rows: Vec<_> = self.0.by_ref().take(1_000).collect();
        (!rows.is_empty()).then_some(RowBatch { rows, bytes: 0 })
    }
}

async fn read(s: &mut Box<dyn Session>, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

async fn load(s: &mut Box<dyn Session>, name: &str, columns: &[&str], rows: Vec<Vec<Cell>>, commit_rows: u64) -> (u64, Vec<u64>) {
    let spec = LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let seen = Mutex::new(Vec::new());
    let progress = |n: u64| seen.lock().unwrap().push(n);
    let n = s.bulk_load(&spec, &[], &mut Rows(rows.into_iter()), &progress).await.unwrap();
    (n, seen.into_inner().unwrap())
}

fn b64(b: &[u8]) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b).into_bytes()
}

/// Equal, allowing the emulator's base64-stored bytes.
fn same(got: &Cell, want: &Cell) -> bool {
    match (got, want) {
        (Cell::Bytes(g), Cell::Bytes(w)) => g == w || *g == b64(w),
        _ => got == want,
    }
}

#[tokio::test]
#[ignore]
async fn transfer_all_types() {
    let Some(mut s) = open().await else { return };
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_types").await;
    run(
        &mut s,
        "CREATE TABLE ds1.xfer_types (id INT64, big INT64, f FLOAT64, inf FLOAT64, n NUMERIC, bn BIGNUMERIC, ok BOOL, \
         s STRING, b BYTES, d DATE, tm TIME, dt DATETIME, ts TIMESTAMP, j JSON, g GEOGRAPHY, arr ARRAY<INT64>, \
         st STRUCT<x INT64, y STRING>)",
    )
    .await;
    let cols = ["id", "big", "f", "inf", "n", "bn", "ok", "s", "b", "d", "tm", "dt", "ts", "j", "g", "arr", "st"];
    let big: Vec<u8> = (0..1024 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let full = vec![
        Cell::Int(1),
        Cell::Int(i64::MIN),
        Cell::Float(1.5e300),
        Cell::Float(f64::NEG_INFINITY),
        Cell::Decimal("-12345678901234567890.123456789".into()),
        Cell::Decimal("123456789012345678901234567890.5".into()),
        Cell::Bool(true),
        Cell::Text("ñandú \"comillas\"\nsalto".into()),
        Cell::Bytes(vec![0, 1, 2, 255]),
        Cell::Date("2024-01-31".into()),
        Cell::Time("12:34:56.789".into()),
        Cell::DateTime("2024-01-31 13:45:00.5".into()),
        Cell::DateTimeTz("2024-01-31 13:45:00.123456+00:00".into()),
        Cell::Json("{\"k\":[1,\"x\"]}".into()),
        Cell::Text("POINT(1 2)".into()),
        Cell::Json("[1,2,3]".into()),
        Cell::Json("{\"x\":5,\"y\":\"z\"}".into()),
    ];
    let mut nulls = vec![Cell::Null; cols.len()];
    nulls[0] = Cell::Int(2);
    let mut large = nulls.clone();
    large[0] = Cell::Int(3);
    large[8] = Cell::Bytes(big.clone());
    let (n, seen) = load(&mut s, "xfer_types", &cols, vec![full.clone(), nulls.clone(), large.clone()], 100_000).await;
    assert_eq!((n, seen), (3, vec![3]));

    let got = read(&mut s, ReadSpec { table: table("xfer_types"), columns: None, filter: None }).await;
    let types: Vec<_> = got.columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(types[..3], ["INT64", "INT64", "FLOAT64"]);
    assert_eq!(types[15..], ["ARRAY<INT64>", "STRUCT<`x` INT64, `y` STRING>"]);
    let mut rows = got.rows;
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    for (want, got) in [&full, &nulls, &large].into_iter().zip(&rows) {
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            // An empty array reads as [] (BigQuery has no NULL arrays).
            let w = if i == 15 && *w == Cell::Null { &Cell::Json("[]".into()) } else { w };
            // Decimals come back without trailing zeros; JSON compact.
            assert!(same(g, w), "column {}: {g:?} != {w:?}", cols[i]);
        }
    }

    // Only some columns, in the asked order, through the query path.
    let got = read(
        &mut s,
        ReadSpec { table: table("xfer_types"), columns: Some(vec!["s".into(), "ID".into()]), filter: Some("id = 1".into()) },
    )
    .await;
    // (named as the read asked for them: BigQuery names are case-insensitive)
    assert_eq!(got.columns.iter().map(|c| c.name.to_lowercase()).collect::<Vec<_>>(), ["s", "id"]);
    assert_eq!(got.rows, vec![vec![full[7].clone(), Cell::Int(1)]]);
    // And through tabledata.list (no filter).
    let got = read(&mut s, ReadSpec { table: table("xfer_types"), columns: Some(vec!["ok".into(), "id".into()]), filter: None }).await;
    assert_eq!(got.rows.len(), 3);
    assert!(got.rows.contains(&vec![Cell::Bool(true), Cell::Int(1)]));

    // A column the table doesn't have is an error, not a shifted load.
    let spec = LoadSpec {
        table: table("xfer_types"),
        columns: vec!["nope".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 10,
        commit_bytes: 1 << 20,
    };
    assert!(s.bulk_load(&spec, &[], &mut Rows(Vec::new().into_iter()), &|_| {}).await.is_err());
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_types").await;
}

#[tokio::test]
#[ignore]
async fn transfer_50k_rows() {
    let Some(mut s) = open().await else { return };
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_big").await;
    run(&mut s, "CREATE TABLE ds1.xfer_big (id INT64, name STRING, amount NUMERIC, ts TIMESTAMP, flag BOOL, f FLOAT64, b BYTES)").await;
    const N: i64 = 50_000;
    let rows: Vec<Vec<Cell>> = (0..N)
        .map(|i| {
            vec![
                Cell::Int(i),
                if i % 7 == 0 { Cell::Null } else { Cell::Text(format!("fila número {i}")) },
                Cell::Decimal(format!("{}.{:02}", i * 3, i % 100).trim_end_matches('0').trim_end_matches('.').to_string()),
                Cell::DateTimeTz({
                    // Read back without trailing zeros in the fraction.
                    let frac = format!("{:06}", i % 1_000_000);
                    let frac = frac.trim_end_matches('0');
                    let dot = if frac.is_empty() { "" } else { "." };
                    format!("2024-01-01 00:00:{:02}{dot}{frac}+00:00", i % 60)
                }),
                Cell::Bool(i % 2 == 0),
                Cell::Float(i as f64 / 4.0),
                Cell::Bytes(i.to_le_bytes().to_vec()),
            ]
        })
        .collect();
    let cols = ["id", "name", "amount", "ts", "flag", "f", "b"];
    let t = Instant::now();
    let (n, seen) = load(&mut s, "xfer_big", &cols, rows.clone(), 10_000).await;
    let load_s = t.elapsed().as_secs_f64();
    assert_eq!(n, N as u64);
    assert_eq!(seen, vec![10_000, 20_000, 30_000, 40_000, 50_000]);

    let t = Instant::now();
    let got = read(&mut s, ReadSpec { table: table("xfer_big"), columns: None, filter: None }).await;
    let read_s = t.elapsed().as_secs_f64();
    let mut back = got.rows;
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(back.len(), rows.len());
    for (g, w) in back.iter().zip(&rows) {
        for (i, (a, b)) in g.iter().zip(w).enumerate() {
            let b = match b {
                // Decimals come back normalized ("1.50" → "1.5").
                Cell::Decimal(d) if d.contains('.') => Cell::Decimal(d.trim_end_matches('0').trim_end_matches('.').to_string()),
                other => other.clone(),
            };
            assert!(same(a, &b), "row {:?} column {}: {a:?} != {b:?}", g[0], cols[i]);
        }
    }
    println!(
        "bigquery (emulador): carga {N} filas en {load_s:.2}s = {:.0} filas/s; lectura en {read_s:.2}s = {:.0} filas/s",
        N as f64 / load_s,
        N as f64 / read_s
    );
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_big").await;
}

#[tokio::test]
#[ignore]
async fn transfer_filter_and_exact_values() {
    let Some(mut s) = open().await else { return };
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_rules").await;
    run(&mut s, "CREATE TABLE ds1.xfer_rules (id INT64, s STRING, b BYTES, d DATE, dt DATETIME)").await;
    let cols = ["id", "s", "b", "d", "dt"];
    let zoned = Cell::DateTimeTz("2024-01-31 23:30:00-03:00".into());
    let row = vec![Cell::Int(1), Cell::Text("a;b".into()), Cell::Decimal("1234".into()), zoned.clone(), zoned];
    let (n, _) = load(&mut s, "xfer_rules", &cols, vec![row], 10).await;
    assert_eq!(n, 1);
    let got = read(&mut s, ReadSpec { table: table("xfer_rules"), columns: None, filter: None }).await;
    let r = &got.rows[0];
    // The same instant in DATE and DATETIME: its UTC date.
    assert_eq!((&r[3], &r[4]), (&Cell::Date("2024-02-01".into()), &Cell::DateTime("2024-02-01 02:30:00".into())));
    // Text into BYTES is its UTF-8 bytes, never decoded as base64.
    assert!(same(&r[2], &Cell::Bytes(b"1234".to_vec())), "{:?}", r[2]);

    // A filter is one condition: a script would run DML on the source.
    let spec = |f: &str| ReadSpec { table: table("xfer_rules"), columns: None, filter: Some(f.into()) };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let err = s.read_batches(&spec("1=1; DELETE FROM ds1.xfer_rules WHERE true; SELECT 1"), sink).await;
    assert!(err.is_err());
    // A `;` inside a literal is fine.
    let got = read(&mut s, spec("s = 'a;b'")).await;
    assert_eq!(got.rows.len(), 1);
    let got = read(&mut s, ReadSpec { table: table("xfer_rules"), columns: Some(vec!["dt".into(), "id".into()]), filter: None }).await;
    assert_eq!(got.rows, vec![vec![Cell::DateTime("2024-02-01 02:30:00".into()), Cell::Int(1)]]);

    // Bytes that aren't UTF-8 into STRING: an error, nothing loaded.
    let spec = LoadSpec {
        table: table("xfer_rules"),
        columns: vec!["id".into(), "s".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 10,
        commit_bytes: 1 << 20,
    };
    let rows = vec![vec![Cell::Int(2), Cell::Bytes(vec![0xff, 0x00])]];
    assert!(s.bulk_load(&spec, &[], &mut Rows(rows.into_iter()), &|_| {}).await.is_err());
    let got = read(&mut s, ReadSpec { table: table("xfer_rules"), columns: None, filter: None }).await;
    assert_eq!(got.rows.len(), 1);
    run(&mut s, "DROP TABLE IF EXISTS ds1.xfer_rules").await;
}

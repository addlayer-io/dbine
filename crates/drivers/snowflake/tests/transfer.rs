//! Bulk transfer against a real Snowflake account (there's no emulator).
//! It creates the database `DBINE_TEST_TRANSFER` and drops it at the end.
//! Run with the variables of `tests/integration.rs` (a warehouse is needed):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test transfer -- --ignored --nocapture transfer
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn, CHUNK_ROWS};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DB: &str = "DBINE_TEST_TRANSFER";
const ROWS: i64 = 100_000;

fn config() -> ConnectionConfig {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options: std::collections::HashMap<String, String> = [
        ("account".to_string(), var("ACCOUNT").expect("DBINE_TEST_SNOWFLAKE_ACCOUNT")),
        ("token".to_string(), var("TOKEN").expect("DBINE_TEST_SNOWFLAKE_TOKEN")),
        ("schema".to_string(), "PUBLIC".to_string()),
    ]
    .into();
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    ConnectionConfig { driver: "snowflake".into(), username: var("USER"), options: options.into_iter().collect(), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The row `i` exactly as reading it back gives it.
fn row(i: i64) -> Vec<Cell> {
    if i % 10 == 0 {
        let mut r = vec![Cell::Int(i)];
        r.resize(12, Cell::Null);
        return r;
    }
    let bin = if i == 1 { (0..4 * 1024 * 1024u32).map(|x| (x % 251) as u8).collect() } else { i.to_le_bytes().to_vec() };
    vec![
        Cell::Int(i),
        Cell::Decimal(format!("{}.{:04}", i * 1_000_000_000_000, i % 10_000)),
        Cell::Float(i as f64 * 0.5),
        Cell::Bool(i % 2 == 0),
        Cell::Text(format!("fila {i} 'ñ' \"x\" \\ {}", "é".repeat((i % 7) as usize))),
        Cell::Bytes(bin),
        Cell::Date("2024-02-29".into()),
        Cell::Time("13:45:00.123456789".into()),
        Cell::DateTime("2024-01-31 13:45:00.5".into()),
        Cell::DateTimeTz("2024-01-31 10:45:00.25-03:00".into()),
        Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()),
        Cell::Json(format!("{{\"i\":{i},\"a\":[1,\"x\"]}}")),
    ]
}

struct Rows {
    next: i64,
}

#[dbine_driver::async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.next >= ROWS {
            return None;
        }
        let end = (self.next + CHUNK_ROWS as i64).min(ROWS);
        let rows: Vec<Vec<Cell>> = (self.next..end).map(row).collect();
        self.next = end;
        Some(RowBatch { bytes: rows.iter().flatten().map(Cell::size).sum(), rows })
    }
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
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

#[tokio::test]
#[ignore]
async fn transfer_round_trip() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    let cfg = config();
    let mut admin = driver.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}")).await;
    let mut s = driver.connect(&cfg, Some(DB)).await.expect("connect");
    run(
        &mut s,
        "CREATE TABLE PUBLIC.T (ID NUMBER(38,0), AMT NUMBER(38,4), F FLOAT, B BOOLEAN, TXT VARCHAR, BIN BINARY,
         D DATE, TM TIME(9), NTZ TIMESTAMP_NTZ(9), TZ TIMESTAMP_TZ(9), LTZ TIMESTAMP_LTZ(9), V VARIANT)",
    )
    .await;
    let table = ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: "T".into() };
    let names: Vec<String> = ["ID", "AMT", "F", "B", "TXT", "BIN", "D", "TM", "NTZ", "TZ", "LTZ", "V"].map(String::from).to_vec();
    let spec = LoadSpec {
        table: table.clone(),
        columns: names.clone(),
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let last = Arc::new(Mutex::new(0u64));
    let seen = last.clone();
    let progress = move |n: u64| *seen.lock().unwrap() = n;
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Rows { next: 0 }, &progress).await.expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    println!("carga: {loaded} filas en {secs:.1} s ({:.0} filas/s)", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);
    assert_eq!(*last.lock().unwrap(), ROWS as u64);

    let sink = Arc::new(Mutex::new(Collect::default()));
    let start = Instant::now();
    let read = s.read_batches(&ReadSpec { table, columns: Some(names), filter: None }, sink.clone()).await.expect("read_batches");
    let secs = start.elapsed().as_secs_f64();
    println!("lectura: {read} filas en {secs:.1} s ({:.0} filas/s)", read as f64 / secs);
    let mut got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(read, ROWS as u64);
    assert_eq!(got.columns[1].type_name, "NUMBER(38,4)");
    got.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => i64::MAX,
    });
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r, &row(i as i64), "fila {i}");
    }

    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

/// First cell of `sql`'s first row, as the grid gives it.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> serde_json::Value {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.last().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or_default()
}

fn count(v: &serde_json::Value) -> i64 {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(-1)
}

/// `total` rows, with a date that can't be read at row `bad` (if any).
struct Bad {
    next: i64,
    total: i64,
    bad: i64,
}

#[dbine_driver::async_trait]
impl BatchSource for Bad {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.next >= self.total {
            return None;
        }
        let rows: Vec<Vec<Cell>> = (self.next..self.next + 1000)
            .map(|i| vec![Cell::Int(i), if i == self.bad { Cell::Text("no es fecha".into()) } else { Cell::Date("2024-02-29".into()) }])
            .collect();
        self.next += 1000;
        Some(RowBatch { bytes: rows.len() * 40, rows })
    }
}

fn bad_spec(name: &str) -> LoadSpec {
    LoadSpec {
        table: ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: name.into() },
        columns: vec!["ID".into(), "D".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// A failed load leaves the target empty, also a while after it returned
/// (nothing orphaned commits later), and no staging table behind. A
/// cancelled one (its future dropped) too, once its clean-up ran.
#[tokio::test]
#[ignore]
async fn transfer_failed_or_cancelled_load_leaves_nothing() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    let cfg = config();
    let mut admin = driver.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}_FAIL")).await;
    let mut s = driver.connect(&cfg, Some(&format!("{DB}_FAIL"))).await.expect("connect");
    run(&mut s, "CREATE TABLE PUBLIC.F (ID NUMBER(38,0), D DATE)").await;
    run(&mut s, "CREATE TABLE PUBLIC.C (ID NUMBER(38,0), D DATE)").await;

    let err = s.bulk_load(&bad_spec("F"), &[], &mut Bad { next: 0, total: 20_000, bad: 15_000 }, &|_| {}).await;
    assert!(err.is_err(), "{err:?}");
    for _ in 0..3 {
        assert_eq!(count(&scalar(&mut s, "SELECT COUNT(*) FROM PUBLIC.F").await), 0);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }

    let r = tokio::time::timeout(std::time::Duration::from_secs(3), s.bulk_load(&bad_spec("C"), &[], &mut Bad { next: 0, total: 2_000_000, bad: -1 }, &|_| {})).await;
    assert!(r.is_err() || r.as_ref().is_ok_and(|r| r.is_err()), "se esperaba cortarla a mitad");
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    assert_eq!(count(&scalar(&mut s, "SELECT COUNT(*) FROM PUBLIC.C").await), 0);
    let left = scalar(&mut s, "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES WHERE CONTAINS(table_name, '__DBINE_CARGA_')").await;
    assert_eq!(count(&left), 0);

    run(&mut admin, &format!("DROP DATABASE {DB}_FAIL")).await;
}

/// VARIANT numbers come back with every digit; a VECTOR column comes with
/// its whole type and loads back.
#[tokio::test]
#[ignore]
async fn transfer_variant_digits_and_vectors() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    let cfg = config();
    let mut admin = driver.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}_TYPES")).await;
    let mut s = driver.connect(&cfg, Some(&format!("{DB}_TYPES"))).await.expect("connect");
    run(&mut s, "CREATE TABLE PUBLIC.V (J VARIANT, VEC VECTOR(FLOAT, 3))").await;
    run(&mut s, "INSERT INTO PUBLIC.V SELECT PARSE_JSON('{\"id\":123456789012345678901,\"p\":0.12345678901234567890}'), [1.5, 2, 3]::VECTOR(FLOAT, 3)").await;
    let table = ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: "V".into() };
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: table.clone(), columns: None, filter: None }, sink.clone()).await.expect("read_batches");
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns[1].type_name.replace(' ', ""), "VECTOR(FLOAT,3)");
    assert_eq!(got.rows[0][0], Cell::Json("{\"id\":123456789012345678901,\"p\":0.12345678901234567890}".into()));

    struct One(Option<Vec<Cell>>);
    #[dbine_driver::async_trait]
    impl BatchSource for One {
        async fn next(&mut self) -> Option<RowBatch> {
            self.0.take().map(|r| RowBatch { bytes: 64, rows: vec![r] })
        }
    }
    let spec = LoadSpec {
        table,
        columns: vec!["J".into(), "VEC".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s.bulk_load(&spec, &[], &mut One(Some(got.rows[0].clone())), &|_| {}).await.expect("bulk_load");
    assert_eq!(n, 1);
    let same = scalar(&mut s, "SELECT COUNT(DISTINCT J::STRING, VEC::ARRAY::STRING) FROM PUBLIC.V").await;
    assert_eq!(count(&same), 1);

    run(&mut admin, &format!("DROP DATABASE {DB}_TYPES")).await;
}

/// A user whose date / time input formats aren't ISO still loads the
/// right values (the load reads them with explicit formats), and a
/// VARIANT with Snowflake's non-JSON tokens is refused, never changed.
#[tokio::test]
#[ignore]
async fn transfer_input_formats_and_non_json_variants() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    let cfg = config();
    let user = std::env::var("DBINE_TEST_SNOWFLAKE_USER").expect("DBINE_TEST_SNOWFLAKE_USER");
    let mut admin = driver.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}_FMT")).await;
    run(
        &mut admin,
        &format!(
            "ALTER USER {user} SET DATE_INPUT_FORMAT = 'DD/MM/YYYY', TIME_INPUT_FORMAT = 'HH12:MI:SS AM', \
             TIMESTAMP_INPUT_FORMAT = 'DD/MM/YYYY HH24:MI:SS'"
        ),
    )
    .await;
    let mut s = driver.connect(&cfg, Some(&format!("{DB}_FMT"))).await.expect("connect");
    run(&mut s, "CREATE TABLE PUBLIC.D (D DATE, T TIME(9), N TIMESTAMP_NTZ(9), Z TIMESTAMP_TZ(9), L TIMESTAMP_LTZ(9))").await;
    struct One(Option<Vec<Cell>>);
    #[dbine_driver::async_trait]
    impl BatchSource for One {
        async fn next(&mut self) -> Option<RowBatch> {
            self.0.take().map(|r| RowBatch { bytes: 64, rows: vec![r] })
        }
    }
    let row = vec![
        Cell::Date("2024-02-03".into()),
        Cell::Time("13:45:00.123456789".into()),
        Cell::DateTime("2024-02-03 13:45:00.5".into()),
        Cell::DateTimeTz("2024-02-03 10:45:00.25-03:00".into()),
        Cell::DateTimeTz("2024-02-03 13:45:00+00:00".into()),
    ];
    let table = ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: "D".into() };
    let spec = LoadSpec {
        table: table.clone(),
        columns: ["D", "T", "N", "Z", "L"].map(String::from).to_vec(),
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let loaded = s.bulk_load(&spec, &[], &mut One(Some(row.clone())), &|_| {}).await;
    run(&mut admin, &format!("ALTER USER {user} UNSET DATE_INPUT_FORMAT, TIME_INPUT_FORMAT, TIMESTAMP_INPUT_FORMAT")).await;
    assert_eq!(loaded.expect("bulk_load"), 1);
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table, columns: None, filter: None }, sink.clone()).await.expect("read_batches");
    assert_eq!(sink.lock().unwrap().rows, vec![row]);

    run(&mut s, "CREATE TABLE PUBLIC.V (A ARRAY)").await;
    run(&mut s, "INSERT INTO PUBLIC.V SELECT ARRAY_CONSTRUCT(1, NULL, 3)").await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let v = ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: "V".into() };
    let r = s.read_batches(&ReadSpec { table: v, columns: None, filter: None }, sink).await;
    assert!(matches!(&r, Err(dbine_driver::Error::Unsupported(m)) if m.contains("undefined")), "{r:?}");

    run(&mut admin, &format!("DROP DATABASE {DB}_FMT")).await;
}

/// Two loads into the same table at once (as from two DBine instances)
/// each add all their rows: neither touches the other's staging table.
#[tokio::test]
#[ignore]
async fn transfer_two_loads_into_one_table() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    let cfg = config();
    let mut admin = driver.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}_TWO")).await;
    let mut s1 = driver.connect(&cfg, Some(&format!("{DB}_TWO"))).await.expect("connect");
    let mut s2 = driver.connect(&cfg, Some(&format!("{DB}_TWO"))).await.expect("connect");
    run(&mut s1, "CREATE TABLE PUBLIC.P (ID NUMBER(38,0), D DATE)").await;
    let (a, b) = (bad_spec("P"), bad_spec("P"));
    let (mut src1, mut src2) = (Bad { next: 0, total: 30_000, bad: -1 }, Bad { next: 0, total: 30_000, bad: -1 });
    let (r1, r2) = tokio::join!(s1.bulk_load(&a, &[], &mut src1, &|_| {}), s2.bulk_load(&b, &[], &mut src2, &|_| {}));
    assert_eq!((r1.expect("carga 1"), r2.expect("carga 2")), (30_000, 30_000));
    assert_eq!(count(&scalar(&mut s1, "SELECT COUNT(*) FROM PUBLIC.P").await), 60_000);
    assert_eq!(count(&scalar(&mut s1, "SELECT COUNT(DISTINCT ID) FROM PUBLIC.P").await), 30_000);
    let left = scalar(&mut s1, "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES WHERE CONTAINS(table_name, '__DBINE_CARGA_')").await;
    assert_eq!(count(&left), 0);
    run(&mut admin, &format!("DROP DATABASE {DB}_TWO")).await;
}

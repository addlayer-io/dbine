//! Bulk transfer against a real Databricks SQL warehouse (ignored by
//! default; there's no container for it). It creates and drops a table
//! `dbine_transfer_test` in the given catalog and schema:
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… \
//! DBINE_TEST_DATABRICKS_CATALOG=main DBINE_TEST_DATABRICKS_SCHEMA=default \
//!   cargo test -p dbine-driver-databricks -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const TABLE: &str = "dbine_transfer_test";

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

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}")).ok().filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig { driver: "databricks".into(), host: env("HOST")?, database: env("CATALOG")?, ..Default::default() };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    c.options.insert("schema".into(), env("SCHEMA").unwrap_or_else(|| "default".into()));
    Some(c)
}

fn table() -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: env("SCHEMA").or(Some("default".into())), name: TABLE.into() }
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn row(i: usize) -> Vec<Cell> {
    let null = i.is_multiple_of(7);
    let or_null = |c: Cell| if null { Cell::Null } else { c };
    vec![
        Cell::Int(i as i64),
        or_null(Cell::Int(i64::MAX - i as i64)),
        or_null(Cell::Float(i as f64 / 3.0)),
        or_null(Cell::Decimal(format!("{}.{:04}", i * 1_000_003, i % 10_000))),
        or_null(Cell::Bool(i.is_multiple_of(2))),
        or_null(Cell::Text(format!("fila {i} con 'comillas', ${{x}} y \\barra ñ"))),
        or_null(Cell::Bytes(if i == 1 { vec![0xAB; 1024 * 1024] } else { vec![(i % 256) as u8; 16] })),
        or_null(Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1))),
        or_null(Cell::DateTimeTz(format!("2024-01-31 13:45:{:02}.123456+00:00", i % 60))),
        or_null(Cell::DateTime(format!("2024-01-31 08:00:{:02}.5", i % 60))),
        or_null(Cell::Json(format!("[{i},{}]", i + 1))),
        or_null(Cell::Json(format!("{{\"{i}\":\"v{i}\"}}"))),
        or_null(Cell::Text(format!("INTERVAL '{} 02:03:04.5' DAY TO SECOND", i % 100))),
        or_null(Cell::Json(format!("{{\"t\":\"2024-01-31T13:45:{:02}.123456Z\"}}", i % 60))),
    ]
}

#[tokio::test]
#[ignore]
async fn transfer_round_trip() {
    let Some(c) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    exec(
        &mut s,
        &format!(
            "CREATE TABLE {TABLE} (id BIGINT, big BIGINT, f DOUBLE, d DECIMAL(24,4), b BOOLEAN, s STRING, bin BINARY,
             dt DATE, ts TIMESTAMP, ntz TIMESTAMP_NTZ, arr ARRAY<INT>, m MAP<INT,STRING>,
             iv INTERVAL DAY TO SECOND, st STRUCT<t: TIMESTAMP>)"
        ),
    )
    .await;
    let names: Vec<String> = ["id", "big", "f", "d", "b", "s", "bin", "dt", "ts", "ntz", "arr", "m", "iv", "st"].iter().map(|n| n.to_string()).collect();
    let rows: Vec<Vec<Cell>> = (0..ROWS).map(row).collect();
    let batches: Vec<RowBatch> = rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    let spec = LoadSpec {
        table: table(),
        columns: names.clone(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let t = Instant::now();
    let seen = Mutex::new(0u64);
    let loaded = s
        .bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| *seen.lock().unwrap() = n)
        .await
        .expect("bulk_load");
    let secs = t.elapsed().as_secs_f64();
    assert_eq!(loaded, ROWS as u64);
    assert_eq!(*seen.lock().unwrap(), ROWS as u64);
    println!("carga: {ROWS} filas en {secs:.1} s ({:.0} filas/s)", ROWS as f64 / secs);

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s
        .read_batches(&ReadSpec { table: table(), columns: Some(names.clone()), filter: None }, sink.clone())
        .await
        .expect("read_batches");
    let secs = t.elapsed().as_secs_f64();
    println!("lectura: {read} filas en {secs:.1} s ({:.0} filas/s)", read as f64 / secs);
    let mut got = std::mem::take(&mut sink.lock().unwrap().rows);
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got.len(), ROWS);
    for (i, r) in got.iter().enumerate() {
        assert_eq!(r, &row(i), "fila {i}");
    }
    exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
}

/// A load that fails after some committed windows leaves the table as it
/// was (RESTORE to the version before the load).
#[tokio::test]
#[ignore]
async fn transfer_failed_load_leaves_no_rows() {
    let Some(c) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}_fail")).await;
    exec(&mut s, &format!("CREATE TABLE {TABLE}_fail (id BIGINT)")).await;
    let rows: Vec<Vec<Cell>> =
        (0..50).map(|i| vec![if i == 35 { Cell::Text("no es un número".into()) } else { Cell::Int(i) }]).collect();
    let spec = LoadSpec {
        table: ObjectRef { name: format!("{TABLE}_fail"), ..table() },
        columns: vec!["id".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 10,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let seen = Mutex::new(0u64);
    let r = s
        .bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|n| *seen.lock().unwrap() = n)
        .await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(*seen.lock().unwrap(), 30, "three windows were committed before the failure");
    let mut out = QueryOutcome::default();
    s.execute(&format!("SELECT count(*) FROM {TABLE}_fail"), 10, &mut out).await.expect("count");
    println!("{:?}", out.results.first().map(|r| &r.rows));
    let n = out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).map(|v| v.to_string()).unwrap_or_default();
    assert!(n.trim_matches('"') == "0", "filas que quedaron: {n}");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}_fail")).await;
}

/// Maps whose keys `to_json` doesn't write faithfully (binary, dates,
/// timestamps) are refused on the read, and binary keys on the load.
#[tokio::test]
#[ignore]
async fn transfer_refuses_map_keys_json_alters() {
    let Some(c) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let name = format!("{TABLE}_mapkeys");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
    exec(&mut s, &format!("CREATE TABLE {name} (mb MAP<BINARY, INT>, md MAP<DATE, INT>)")).await;
    exec(&mut s, &format!("INSERT INTO {name} VALUES (map(X'FF00', 1), map(DATE'2024-01-31', 2))")).await;
    let t = ObjectRef { name: name.clone(), ..table() };
    for col in ["mb", "md"] {
        let sink = Arc::new(Mutex::new(Collect::default()));
        let r = s.read_batches(&ReadSpec { table: t.clone(), columns: Some(vec![col.into()]), filter: None }, sink).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{col}: {r:?}");
    }
    let spec = LoadSpec {
        table: t,
        columns: vec!["mb".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let rows = vec![vec![Cell::Json("{\"a\":1}".into())]];
    let r = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
}

/// Text heavy in backslashes and control characters (several times its
/// size once escaped in the request) is split into statements that the
/// API accepts, and comes back unchanged.
#[tokio::test]
#[ignore]
async fn transfer_escaped_heavy_text_round_trip() {
    let Some(c) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let name = format!("{TABLE}_escaped");
    exec(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
    exec(&mut s, &format!("CREATE TABLE {name} (id BIGINT, s STRING)")).await;
    let text = |i: usize| format!("{i}{}", "\\\"\u{1}".repeat(700 * 1024));
    let rows: Vec<Vec<Cell>> = (0..8).map(|i| vec![Cell::Int(i as i64), Cell::Text(text(i))]).collect();
    let t = ObjectRef { name: name.clone(), ..table() };
    let spec = LoadSpec {
        table: t.clone(),
        columns: vec!["id".into(), "s".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()), &|_| {})
        .await
        .expect("bulk_load");
    assert_eq!(n, 8);
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: t, columns: None, filter: None }, sink.clone()).await.expect("read_batches");
    let mut got = std::mem::take(&mut sink.lock().unwrap().rows);
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    for (i, r) in got.iter().enumerate() {
        assert_eq!(r[1], Cell::Text(text(i)), "fila {i}");
    }
    exec(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
}

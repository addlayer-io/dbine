//! Bulk transfer against a real Amazon Athena (ignored by default; there is
//! no local emulator). They create Iceberg tables, load 50,000 rows with
//! every type (nested ones with dates and timestamps, NaN / ±Infinity, REAL,
//! instants into a zoneless column, empty strings apart from NULL), read
//! them back in another column order, compare and drop the tables; check
//! the 100-partitions split and that a Hive table is refused:
//!
//! ```sh
//! DBINE_TEST_ATHENA_DATABASE=dbine_test \
//! DBINE_TEST_ATHENA_LOCATION=s3://my-bucket/dbine-test/ \
//! DBINE_TEST_ATHENA_OUTPUT=s3://my-bucket/athena-results/ \
//! AWS_REGION=us-east-1 \
//!   cargo test -p dbine-driver-athena -- --ignored transfer --nocapture
//! ```
//!
//! Credentials come from the default AWS chain.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 50_000;

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
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

const COLS: [&str; 13] = ["id", "b", "n", "f", "r", "d", "s", "bin", "dt", "ts", "tz", "st", "arr"];

fn row(i: usize) -> Vec<Cell> {
    let null = i % 7 == 3;
    let or_null = |c: Cell| if null { Cell::Null } else { c };
    let f = match i % 13 {
        1 => f64::NAN,
        2 => f64::INFINITY,
        4 => f64::NEG_INFINITY,
        _ => i as f64 / 3.0,
    };
    // Empty strings are not NULLs.
    let s = if i % 11 == 5 { String::new() } else { format!("fila {i} · ñandú 'x'") };
    vec![
        Cell::Int(i as i64),
        or_null(Cell::Bool(i.is_multiple_of(2))),
        or_null(Cell::Int(i as i64 * 1_000_003 - 7)),
        or_null(Cell::Float(f)),
        or_null(Cell::Float(f64::from(i as f32 / 7.0))),
        or_null(Cell::Decimal(format!("{}.{:02}", i, i % 100))),
        or_null(Cell::Text(s)),
        or_null(Cell::Bytes((0..(i % 64) as u8).collect())),
        or_null(Cell::Date("2024-02-29".into())),
        or_null(Cell::DateTime(format!("2024-01-02 03:04:{:02}.123456", i % 60))),
        // An instant into a zoneless column: its UTC wall time, microseconds kept.
        or_null(Cell::DateTimeTz(format!("2024-01-02 03:04:{:02}.654321+01:00", i % 60))),
        or_null(Cell::Json(
            serde_json::json!({"d": "2024-02-29", "ts": format!("2024-01-02 03:04:05.{:06}", i % 1_000_000), "n": i, "s": "x'y"}).to_string(),
        )),
        or_null(Cell::Json("[\"2024-01-01\",\"2024-12-31\"]".into())),
    ]
}

/// What reading `row(i)` back gives, column by column.
fn expected(i: usize) -> Vec<Cell> {
    let mut r = row(i);
    if let Cell::DateTimeTz(_) = r[10] {
        r[10] = Cell::DateTime(format!("2024-01-02 02:04:{:02}.654321", i % 60));
    }
    r
}

fn same(a: &Cell, b: &Cell) -> bool {
    match (a, b) {
        (Cell::Float(x), Cell::Float(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (Cell::Json(x), Cell::Json(y)) => {
            serde_json::from_str::<serde_json::Value>(x).ok() == serde_json::from_str::<serde_json::Value>(y).ok()
        }
        _ => a == b,
    }
}

async fn session() -> Option<(Box<dyn Session>, String, String)> {
    let (Some(db), Some(location)) = (env("DBINE_TEST_ATHENA_DATABASE"), env("DBINE_TEST_ATHENA_LOCATION")) else {
        eprintln!("DBINE_TEST_ATHENA_DATABASE / DBINE_TEST_ATHENA_LOCATION sin definir: se omite");
        return None;
    };
    let mut cfg = ConnectionConfig { driver: "athena".into(), database: db.clone(), ..Default::default() };
    if let Some(o) = env("DBINE_TEST_ATHENA_OUTPUT") {
        cfg.options.insert("output_location".into(), o);
    }
    if let Some(r) = env("AWS_REGION") {
        cfg.options.insert("region".into(), r);
    }
    let driver = dbine_driver_athena::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    Some((driver.connect(&cfg, None).await.expect("connect"), db, location))
}

fn spec(db: &str, name: &str, cols: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: ObjectRef { kind: kinds::TABLE.into(), schema: Some(db.into()), name: name.into() },
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

async fn read_all(s: &mut Box<dyn Session>, read: &ReadSpec) -> (Vec<String>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(read, sink.clone()).await.expect("read");
    let mut c = sink.lock().unwrap();
    (c.columns.iter().map(|c| c.name.clone()).collect(), std::mem::take(&mut c.rows))
}

#[tokio::test]
#[ignore]
async fn transfer_load_and_read_back() {
    let Some((mut s, db, location)) = session().await else { return };
    let name = "dbine_transfer_test";
    let _ = s.execute(&format!("DROP TABLE IF EXISTS `{name}`"), 1, &mut QueryOutcome::default()).await;
    let ddl = format!(
        "CREATE TABLE {name} (id bigint, b boolean, n bigint, f double, r float, d decimal(12,2), s string, bin binary, \
         dt date, ts timestamp, tz timestamp, st struct<d:date,ts:timestamp,n:int,s:string>, arr array<date>) \
         LOCATION '{location}{name}/' TBLPROPERTIES ('table_type' = 'ICEBERG')"
    );
    s.execute(&ddl, 1, &mut QueryOutcome::default()).await.expect("create");

    let rows: Vec<Vec<Cell>> = (0..ROWS).map(row).collect();
    // Windows of 5,000 rows: progress never jumps by more than that.
    let spec = spec(&db, name, &COLS, 5_000);
    let mut src = Batches(rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect::<Vec<_>>().into_iter());
    let seen = Mutex::new(vec![0u64]);
    let started = Instant::now();
    let loaded = s
        .bulk_load(&spec, &[], &mut src, &|n| {
            let mut v = seen.lock().unwrap();
            assert!(n - v.last().unwrap() <= 5_000, "ventana de {} filas", n - v.last().unwrap());
            v.push(n);
        })
        .await
        .expect("load");
    let secs = started.elapsed().as_secs_f64();
    eprintln!("carga: {loaded} filas en {secs:.1} s ({:.0} filas/s)", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);

    // Read back with the columns in reverse order.
    let reversed: Vec<String> = COLS.iter().rev().map(|c| c.to_string()).collect();
    let read = ReadSpec { table: spec.table.clone(), columns: Some(reversed.clone()), filter: None };
    let started = Instant::now();
    let (names, mut got) = read_all(&mut s, &read).await;
    eprintln!("lectura: {} filas en {:.1} s", got.len(), started.elapsed().as_secs_f64());
    assert_eq!(names, reversed);
    for r in &mut got {
        r.reverse();
    }
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got.len(), ROWS);
    for (i, r) in got.iter().enumerate() {
        let want = expected(i);
        for (c, (a, b)) in r.iter().zip(&want).enumerate() {
            assert!(same(a, b), "fila {i}, columna {}: {a:?} != {b:?}", COLS[c]);
        }
    }

    // A subset in another order, with a filter.
    let read = ReadSpec { table: spec.table.clone(), columns: Some(vec!["s".into(), "id".into()]), filter: Some("id < 10".into()) };
    let (names, got) = read_all(&mut s, &read).await;
    assert_eq!(names, ["s", "id"]);
    assert_eq!(got.len(), 10);

    s.execute(&format!("DROP TABLE `{name}`"), 1, &mut QueryOutcome::default()).await.expect("drop");
}

/// 250 narrow rows in one statement over 250 partitions: over Athena's
/// 100-partition limit, so the statement is split and every row lands once.
#[tokio::test]
#[ignore]
async fn transfer_splits_over_the_partition_limit() {
    let Some((mut s, db, location)) = session().await else { return };
    let name = "dbine_transfer_parts";
    let _ = s.execute(&format!("DROP TABLE IF EXISTS `{name}`"), 1, &mut QueryOutcome::default()).await;
    let ddl = format!(
        "CREATE TABLE {name} (id bigint, p int) PARTITIONED BY (p) LOCATION '{location}{name}/' TBLPROPERTIES ('table_type' = 'ICEBERG')"
    );
    s.execute(&ddl, 1, &mut QueryOutcome::default()).await.expect("create");
    let spec = spec(&db, name, &["id", "p"], LoadSpec::DEFAULT_COMMIT_ROWS);
    let rows: Vec<Vec<Cell>> = (0..250).map(|i| vec![Cell::Int(i), Cell::Int(i)]).collect();
    let mut src = Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());
    assert_eq!(s.bulk_load(&spec, &[], &mut src, &|_| {}).await.expect("load"), 250);
    let read = ReadSpec { table: spec.table.clone(), columns: Some(vec!["id".into()]), filter: None };
    let (_, mut got) = read_all(&mut s, &read).await;
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got, (0..250).map(|i| vec![Cell::Int(i)]).collect::<Vec<_>>());
    s.execute(&format!("DROP TABLE `{name}`"), 1, &mut QueryOutcome::default()).await.expect("drop");
}

/// A Hive table can keep a failed INSERT's files: loading into it is refused.
#[tokio::test]
#[ignore]
async fn transfer_refuses_hive_tables() {
    let Some((mut s, db, location)) = session().await else { return };
    let name = "dbine_transfer_hive";
    let _ = s.execute(&format!("DROP TABLE IF EXISTS `{name}`"), 1, &mut QueryOutcome::default()).await;
    let ddl = format!("CREATE EXTERNAL TABLE `{name}` (id bigint) STORED AS PARQUET LOCATION '{location}{name}/'");
    s.execute(&ddl, 1, &mut QueryOutcome::default()).await.expect("create");
    let spec = spec(&db, name, &["id"], LoadSpec::DEFAULT_COMMIT_ROWS);
    let mut src = Batches(vec![RowBatch { rows: vec![vec![Cell::Int(1)]], bytes: 0 }].into_iter());
    let r = s.bulk_load(&spec, &[], &mut src, &|_| {}).await;
    assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("Iceberg")), "{r:?}");
    s.execute(&format!("DROP TABLE `{name}`"), 1, &mut QueryOutcome::default()).await.expect("drop");
}

async fn count(s: &mut Box<dyn Session>, db: &str, name: &str) -> u64 {
    let read = ReadSpec {
        table: ObjectRef { kind: kinds::TABLE.into(), schema: Some(db.into()), name: name.into() },
        columns: Some(vec!["id".into()]),
        filter: None,
    };
    read_all(s, &read).await.1.len() as u64
}

/// Nested timestamps as other engines print them (ISO `T`, `Z`) load; a
/// nested nanosecond value and bytes that aren't UTF-8 into a string column
/// are refused before any row lands.
#[tokio::test]
#[ignore]
async fn transfer_nested_times_and_refusals() {
    let Some((mut s, db, location)) = session().await else { return };
    let name = "dbine_transfer_nested";
    let _ = s.execute(&format!("DROP TABLE IF EXISTS `{name}`"), 1, &mut QueryOutcome::default()).await;
    let ddl = format!(
        "CREATE TABLE {name} (id bigint, s string, st struct<ts:timestamp,tz:timestamp with time zone>) \
         LOCATION '{location}{name}/' TBLPROPERTIES ('table_type' = 'ICEBERG')"
    );
    s.execute(&ddl, 1, &mut QueryOutcome::default()).await.expect("create");
    let spec = spec(&db, name, &["id", "s", "st"], LoadSpec::DEFAULT_COMMIT_ROWS);
    let nested = |ts: &str| Cell::Json(serde_json::json!({"ts": ts, "tz": "2024-01-02T03:04:05.123456Z"}).to_string());

    let bad_ts = vec![vec![Cell::Int(1), Cell::Text("a".into()), nested("2024-01-02 03:04:05.123456789")]];
    let r = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: bad_ts, bytes: 0 }].into_iter()), &|_| {}).await;
    assert!(matches!(r, Err(Error::Query(ref m)) if m.contains("decimales")), "{r:?}");
    let bad_utf8 = vec![vec![Cell::Int(1), Cell::Bytes(vec![0xFF, 0x41]), Cell::Null]];
    let r = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: bad_utf8, bytes: 0 }].into_iter()), &|_| {}).await;
    assert!(matches!(r, Err(Error::Query(ref m)) if m.contains("UTF-8")), "{r:?}");
    assert_eq!(count(&mut s, &db, name).await, 0);

    let good = vec![vec![Cell::Int(1), Cell::Text("a".into()), nested("2024-01-02T03:04:05.654321")]];
    let n = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: good, bytes: 0 }].into_iter()), &|_| {}).await;
    assert_eq!(n.expect("load"), 1);
    let read = ReadSpec { table: spec.table.clone(), columns: Some(vec!["st".into()]), filter: None };
    let (_, got) = read_all(&mut s, &read).await;
    let Cell::Json(j) = &got[0][0] else { panic!("{got:?}") };
    let v: serde_json::Value = serde_json::from_str(j).unwrap();
    assert_eq!(v["ts"], "2024-01-02 03:04:05.654321", "{j}");
    s.execute(&format!("DROP TABLE `{name}`"), 1, &mut QueryOutcome::default()).await.expect("drop");
}

/// A cancel in the middle of a load: it stops, and the table holds exactly
/// the rows `progress` reported (none land after it returns).
#[tokio::test]
#[ignore]
async fn transfer_cancel_leaves_what_progress_said() {
    let Some((mut s, db, location)) = session().await else { return };
    let name = "dbine_transfer_cancel";
    let _ = s.execute(&format!("DROP TABLE IF EXISTS `{name}`"), 1, &mut QueryOutcome::default()).await;
    let ddl = format!("CREATE TABLE {name} (id bigint) LOCATION '{location}{name}/' TBLPROPERTIES ('table_type' = 'ICEBERG')");
    s.execute(&ddl, 1, &mut QueryOutcome::default()).await.expect("create");
    let spec = spec(&db, name, &["id"], 100);
    let rows: Vec<Vec<Cell>> = (0..2_000).map(|i| vec![Cell::Int(i)]).collect();
    let mut src = Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());
    let stop = s.interrupter().expect("interrupter");
    let last = Arc::new(Mutex::new(0u64));
    let seen = last.clone();
    let r = s
        .bulk_load(&spec, &[], &mut src, &move |n| {
            *seen.lock().unwrap() = n;
            // Cancel while the third INSERT starts.
            if n == 200 {
                stop();
            }
        })
        .await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    let reported = *last.lock().unwrap();
    // Give a straggler INSERT time to (wrongly) commit, then count.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_eq!(count(&mut s, &db, name).await, reported);
    s.execute(&format!("DROP TABLE `{name}`"), 1, &mut QueryOutcome::default()).await.expect("drop");
}

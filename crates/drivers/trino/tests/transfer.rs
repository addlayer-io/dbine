//! Bulk transfer against a real Trino (ignored by default), into the
//! `memory` catalog:
//!
//! ```sh
//! docker run -d --name dbine-test-trino -p 25180:8080 trinodb/trino
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//!   cargo test -p dbine-driver-trino -- --ignored transfer --nocapture --test-threads=1
//! ```
//!
//! The tests share the `dbine_tr` schema, hence one thread.
//!
//! Presto too, with `DBINE_TEST_PRESTO_URL` (`prestodb/presto`, whose
//! times and timestamps are milliseconds):
//!
//! ```sh
//! docker run -d --name dbine-test-presto -p 25181:8080 prestodb/presto
//! DBINE_TEST_PRESTO_URL=http://localhost:25181 \
//!   cargo test -p dbine-driver-trino -- --ignored transfer --nocapture --test-threads=1
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Rows of the volume test (`DBINE_TEST_TRANSFER_ROWS`, 100,000 by default;
/// less for a small test server: Presto's image runs with 1 GB of heap).
fn volume() -> usize {
    std::env::var("DBINE_TEST_TRANSFER_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000)
}

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

fn cfg(driver: &str, var: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    let v: Vec<RowBatch> = rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Batches(v.into_iter())
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_tr".into()), name: name.into() }
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

async fn read_all(s: &mut Box<dyn Session>, name: &str, filter: Option<&str>) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table(name), columns: None, filter: filter.map(str::to_string) };
    let n = s.read_batches(&spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap();
}

/// Presto has no `DROP SCHEMA … CASCADE` (and its `IF EXISTS` fails
/// without the schema): each drop on its own, errors ignored.
async fn clean(s: &mut Box<dyn Session>) {
    for t in ["all_types", "big", "slow", "nest", "nest2", "late", "odd", "frac", "lone", "lone2", "lone_t", "days"] {
        let _ = s.execute(&format!("DROP TABLE IF EXISTS dbine_tr.{t}"), 1, &mut QueryOutcome::default()).await;
    }
    let _ = s.execute("DROP SCHEMA IF EXISTS dbine_tr", 1, &mut QueryOutcome::default()).await;
}

#[tokio::test]
#[ignore]
async fn transfer_trino() {
    run("trino", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test]
#[ignore]
async fn transfer_presto() {
    run("presto", "DBINE_TEST_PRESTO_URL").await;
}

async fn run(driver: &str, var: &str) {
    let Some(c) = cfg(driver, var) else { return };
    let presto = driver == "presto";
    let rows_n = volume();
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, None).await.unwrap();
    // Presto: no parametric times, no TIME WITH TIME ZONE literal read back as written.
    let (tm, tmz, ts, tsz) = if presto {
        ("time", "varchar", "timestamp", "timestamp with time zone")
    } else {
        ("time(12)", "time(0) with time zone", "timestamp(9)", "timestamp(6) with time zone")
    };
    clean(&mut s).await;
    exec(
        &mut s,
        &format!(
            "CREATE SCHEMA dbine_tr;
             CREATE TABLE dbine_tr.all_types (
               id bigint, b boolean, ti tinyint, si smallint, i integer, r real, dbl double,
               dec decimal(38,9), v varchar, ch char(3), bin varbinary, dt date, tm {tm},
               tmz {tmz}, ts {ts}, tsz {tsz},
               u uuid, j json, a array(integer), m map(varchar, integer), rw row(a integer, b varchar),
               ip ipaddress)"
        ),
    )
    .await;
    let frac = |f: &str| if presto { f[..3].to_string() } else { f.to_string() };

    // Every type, NULLs, and a binary that needs from_base64 to fit.
    let big: Vec<u8> = (0..500_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let t = |s: &str| Cell::Text(s.into());
    let full = vec![
        Cell::Int(1),
        Cell::Bool(true),
        Cell::Int(-5),
        Cell::Int(300),
        Cell::Int(70_000),
        Cell::Float(1.5),
        Cell::Float(0.1),
        Cell::Decimal("12345678901234567890.123456789".into()),
        t("O'Brien ñ\nlínea 'dos'"),
        t("abc"),
        Cell::Bytes(big.clone()),
        Cell::Date("2024-02-29".into()),
        Cell::Time(format!("23:59:59.{}", frac("123456789012"))),
        t("10:00:00+01:00"),
        Cell::DateTime(format!("2024-01-02 03:04:05.{}", frac("123456789"))),
        Cell::DateTimeTz(format!("2024-01-02 03:04:05.{}-03:00", frac("123456"))),
        Cell::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
        Cell::Json("{\"a\":[1,2]}".into()),
        Cell::Json("[1,2,null]".into()),
        Cell::Json("{\"k\":1}".into()),
        Cell::Json("{\"a\":1,\"b\":\"x\"}".into()),
        t("10.0.0.1"),
    ];
    let mut edge = full.clone();
    edge[0] = Cell::Int(2);
    edge[1] = Cell::Bool(false);
    edge[4] = Cell::Int(i32::MIN as i64);
    edge[5] = Cell::Float(f64::NEG_INFINITY);
    edge[6] = Cell::Float(f64::MAX);
    edge[7] = Cell::Decimal("-0.000000001".into());
    edge[10] = Cell::Bytes(vec![0, 255]);
    edge[15] = Cell::DateTimeTz(format!("1970-01-01 00:00:00.{}+00:00", frac("000000")));
    let mut nulls = vec![Cell::Null; full.len()];
    nulls[0] = Cell::Int(3);
    let names = [
        "id", "b", "ti", "si", "i", "r", "dbl", "dec", "v", "ch", "bin", "dt", "tm", "tmz", "ts", "tsz", "u", "j", "a", "m", "rw", "ip",
    ];
    let rows = vec![full, edge, nulls];
    let calls = Mutex::new(Vec::new());
    let n = s
        .bulk_load(&load_spec("all_types", &names), &[], &mut batches(rows.clone()), &|n| calls.lock().unwrap().push(n))
        .await
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(calls.lock().unwrap().last(), Some(&3));

    let mut back = read_all(&mut s, "all_types", None).await;
    assert_eq!(back.columns.len(), names.len());
    back.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    for (want, got) in rows.iter().zip(&back.rows) {
        for (i, (w, g)) in want.iter().zip(got).enumerate() {
            assert_eq!(w, g, "column {}", names[i]);
        }
    }
    // A filter reads only its rows.
    assert_eq!(read_all(&mut s, "all_types", Some("id >= 2")).await.rows.len(), 2);

    // 100k rows: load, read back, compare.
    let ts6 = if presto { "timestamp" } else { "timestamp(6)" };
    exec(&mut s, &format!("CREATE TABLE dbine_tr.big (id bigint, name varchar, amount decimal(12,2), ts {ts6}, flag boolean, x double)")).await;
    let data: Vec<Vec<Cell>> = (0..rows_n as i64)
        .map(|i| {
            vec![
                Cell::Int(i),
                Cell::Text(format!("nombre {i}")),
                Cell::Decimal(format!("{}.{:02}", i, i % 100)),
                Cell::DateTime(format!("2024-01-01 00:00:{:02}.{}", i % 60, frac(&format!("{:06}", i % 1_000_000)))),
                Cell::Bool(i % 2 == 0),
                Cell::Float(i as f64 / 3.0),
            ]
        })
        .collect();
    let cols = ["id", "name", "amount", "ts", "flag", "x"];
    let t0 = Instant::now();
    let n = s.bulk_load(&load_spec("big", &cols), &[], &mut batches(data.clone()), &|_| {}).await.unwrap();
    let load = t0.elapsed();
    assert_eq!(n, rows_n as u64);
    println!("{driver} bulk_load: {rows_n} rows in {load:?} = {:.0} rows/s", rows_n as f64 / load.as_secs_f64());

    let t0 = Instant::now();
    let mut back = read_all(&mut s, "big", None).await;
    let read = t0.elapsed();
    println!("{driver} read_batches: {rows_n} rows in {read:?} = {:.0} rows/s", rows_n as f64 / read.as_secs_f64());
    back.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(back.rows, data);

    // The insert_script path, for comparison (a tenth of the rows; only
    // the columns it can type: its decimals and doubles don't insert).
    exec(&mut s, &format!("CREATE TABLE dbine_tr.slow (id bigint, name varchar, ts {ts6}, flag boolean)")).await;
    let target = table("slow");
    let cols: Vec<String> = ["id", "name", "ts", "flag"].iter().map(|c| c.to_string()).collect();
    let json: Vec<Vec<serde_json::Value>> =
        data[..rows_n / 10].iter().map(|r| [0, 1, 3, 4].iter().map(|&i| r[i].to_json()).collect()).collect();
    let t0 = Instant::now();
    for chunk in json.chunks(500) {
        let script = d.insert_script(&target, &cols, chunk).unwrap();
        exec(&mut s, &script).await;
    }
    let slow = t0.elapsed();
    println!("{driver} insert_script: {} rows in {slow:?} = {:.0} rows/s", rows_n / 10, (rows_n / 10) as f64 / slow.as_secs_f64());

    clean(&mut s).await;
}

async fn count(s: &mut Box<dyn Session>, sql: &str) -> u64 {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    let v = &out.results[0].rows[0][0];
    v.as_u64().or_else(|| v.as_str().and_then(|x| x.parse().ok())).unwrap()
}

/// INSERTs of this run's `late` table the server still hasn't ended.
async fn live_inserts(s: &mut Box<dyn Session>) -> u64 {
    count(
        s,
        "SELECT count(*) FROM system.runtime.queries WHERE state NOT IN ('FINISHED', 'FAILED') \
         AND query LIKE 'INSERT INTO \"dbine_tr\".\"late\"%'",
    )
    .await
}

/// Rows for the `late` table: `n` chunks of 2,000 rows (a few statements
/// each), wide enough that two statements are in flight together, with a
/// value the server rejects in chunk `bad` (if any).
fn late_rows(statements: usize, bad: Option<usize>) -> Vec<Vec<Cell>> {
    let per = 2_000;
    (0..statements * per)
        .map(|i| {
            let id = if bad == Some(i / per) && i % per == per / 2 { Cell::Text("no es un número".into()) } else { Cell::Int(i as i64) };
            vec![id, Cell::Text(format!("{i:0>200}"))]
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn transfer_fixes_trino() {
    fixes("trino", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test]
#[ignore]
async fn transfer_fixes_presto() {
    fixes("presto", "DBINE_TEST_PRESTO_URL").await;
}

/// Regressions of the review's problems, against a real server.
async fn fixes(driver: &str, var: &str) {
    let Some(c) = cfg(driver, var) else { return };
    let presto = driver == "presto";
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    clean(&mut s).await;
    exec(&mut s, "CREATE SCHEMA dbine_tr").await;

    // 1. A failed load leaves nothing running that could commit later.
    exec(&mut s, "CREATE TABLE dbine_tr.late (id bigint, pad varchar)").await;
    let r = s.bulk_load(&load_spec("late", &["id", "pad"]), &[], &mut batches(late_rows(12, Some(6))), &|_| {}).await;
    assert!(r.is_err(), "the bad value must fail the load");
    assert_eq!(live_inserts(&mut s).await, 0, "an INSERT is still running after bulk_load returned");
    let after = count(&mut s, "SELECT count(*) FROM dbine_tr.late").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_tr.late").await, after, "rows committed after the failed load returned");
    println!("{driver}: failed load left {after} rows, none after it returned");

    // …and a cancelled one (the interrupter fires on the first commit).
    exec(&mut s, "DROP TABLE dbine_tr.late").await;
    exec(&mut s, "CREATE TABLE dbine_tr.late (id bigint, pad varchar)").await;
    let stop = s.interrupter().unwrap();
    let r = s.bulk_load(&load_spec("late", &["id", "pad"]), &[], &mut batches(late_rows(12, None)), &|_| stop()).await;
    assert!(r.is_err(), "the load must stop");
    assert_eq!(live_inserts(&mut s).await, 0, "an INSERT is still running after the cancel returned");
    let after = count(&mut s, "SELECT count(*) FROM dbine_tr.late").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_tr.late").await, after, "rows committed after the cancel returned");
    println!("{driver}: cancelled load left {after} rows ({r:?})");

    // 6. Inside an explicit transaction nothing would be committed: refused.
    exec(&mut s, "START TRANSACTION").await;
    let r = s.bulk_load(&load_spec("late", &["id", "pad"]), &[], &mut batches(late_rows(1, None)), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    exec(&mut s, "ROLLBACK").await;

    // 7, 8, 9, 10: no rounding, zones as UTC, texts stay strings, years past 9999.
    let (ts, tsz) = if presto { ("timestamp", "timestamp with time zone") } else { ("timestamp(3)", "timestamp(3) with time zone") };
    exec(&mut s, &format!("CREATE TABLE dbine_tr.odd (id bigint, dec decimal(10,2), ts {ts}, j json, tsz {tsz})")).await;
    let cols = ["id", "dec", "ts", "j", "tsz"];
    let r = s
        .bulk_load(&load_spec("odd", &cols), &[], &mut batches(vec![vec![Cell::Int(0), Cell::Decimal("1.239".into()), Cell::Null, Cell::Null, Cell::Null]]), &|_| {})
        .await;
    assert!(r.is_err(), "1.239 into decimal(10,2) must not be rounded");
    let far = if presto { None } else { Some(Cell::DateTimeTz("12024-01-02 03:04:05.000+00:00".into())) };
    let row = vec![
        Cell::Int(1),
        Cell::Decimal("1.230".into()),
        Cell::Text("2024-01-01 00:00:00+01:00".into()),
        Cell::Text("123".into()),
        far.clone().unwrap_or(Cell::Null),
    ];
    s.bulk_load(&load_spec("odd", &cols), &[], &mut batches(vec![row]), &|_| {}).await.unwrap();
    let back = read_all(&mut s, "odd", None).await.rows;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0][1], Cell::Decimal("1.23".into()));
    assert_eq!(back[0][2], Cell::DateTime("2023-12-31 23:00:00.000".into()));
    assert_eq!(back[0][3], Cell::Json("\"123\"".into()));
    assert_eq!(back[0][4], far.unwrap_or(Cell::Null));

    // 2, 3. Nested values JSON can't take: read, loaded back, read again.
    if !presto {
        let def = "(id bigint, ad array(date), ats array(timestamp(6)), au array(uuid), atm array(time(3)), \
                   mb map(varchar, varbinary), rw row(a uuid, b date, c timestamp(3) with time zone), md map(date, integer), \
                   dec array(decimal(38,9)), ach array(char(3)), nn array(array(date)))";
        exec(&mut s, &format!("CREATE TABLE dbine_tr.nest {def}")).await;
        exec(&mut s, &format!("CREATE TABLE dbine_tr.nest2 {def}")).await;
        exec(
            &mut s,
            "INSERT INTO dbine_tr.nest VALUES
             (1, ARRAY[DATE '2024-01-01', NULL], ARRAY[TIMESTAMP '2024-01-01 00:00:00.123456'],
              ARRAY[UUID 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'], ARRAY[TIME '10:00:00.500'],
              MAP(ARRAY['k'], ARRAY[X'CAFE']), CAST(ROW(UUID 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', DATE '2024-02-29', TIMESTAMP '2024-01-01 00:00:00.000 -03:00') AS row(a uuid, b date, c timestamp(3) with time zone)),
              MAP(ARRAY[DATE '2024-01-01'], ARRAY[7]), ARRAY[DECIMAL '12345678901234567890.123456789'], ARRAY[CHAR 'ab'],
              ARRAY[ARRAY[DATE '2024-01-01'], ARRAY[]]),
             (2, ARRAY[], NULL, ARRAY[], NULL, MAP(), NULL, NULL, NULL, NULL, NULL)",
        )
        .await;
        let first = read_all(&mut s, "nest", None).await;
        let names: Vec<String> = first.columns.iter().map(|c| c.name.clone()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let n = s.bulk_load(&load_spec("nest2", &names), &first.columns, &mut batches(first.rows.clone()), &|_| {}).await.unwrap();
        assert_eq!(n, 2);
        let mut a = first.rows;
        let mut b = read_all(&mut s, "nest2", None).await.rows;
        for v in [&mut a, &mut b] {
            v.sort_by_key(|r| match r[0] {
                Cell::Int(i) => i,
                _ => 0,
            });
        }
        assert_eq!(a, b);
        let same = count(
            &mut s,
            // Maps aren't comparable: their keys and values are.
            "SELECT count(*) FROM dbine_tr.nest x JOIN dbine_tr.nest2 y ON x.id = y.id
             WHERE x.ad IS NOT DISTINCT FROM y.ad AND x.au IS NOT DISTINCT FROM y.au AND x.atm IS NOT DISTINCT FROM y.atm
               AND map_keys(x.mb) IS NOT DISTINCT FROM map_keys(y.mb) AND map_values(x.mb) IS NOT DISTINCT FROM map_values(y.mb)
               AND map_keys(x.md) IS NOT DISTINCT FROM map_keys(y.md) AND map_values(x.md) IS NOT DISTINCT FROM map_values(y.md)
               AND x.rw IS NOT DISTINCT FROM y.rw AND x.dec IS NOT DISTINCT FROM y.dec AND x.ach IS NOT DISTINCT FROM y.ach
               AND x.nn IS NOT DISTINCT FROM y.nn AND x.ats IS NOT DISTINCT FROM y.ats",
        )
        .await;
        assert_eq!(same, 2, "both rows must come back identical");
        println!("{driver}: nested row read back: {:?}", a[0]);
    }

    clean(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn transfer_times_trino() {
    times("trino", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test]
#[ignore]
async fn transfer_times_presto() {
    times("presto", "DBINE_TEST_PRESTO_URL").await;
}

/// More fraction-of-second digits than the column keeps is an error (the
/// server rounded `23:59:59.9999` into `time(3)` to midnight); trailing
/// zeros are not. Light on its own, apart from [`fixes`]' heavier loads.
async fn times(driver: &str, var: &str) {
    let Some(c) = cfg(driver, var) else { return };
    let presto = driver == "presto";
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    clean(&mut s).await;
    exec(&mut s, "CREATE SCHEMA dbine_tr").await;
    let (ts, tsz) = if presto { ("timestamp", "timestamp with time zone") } else { ("timestamp(3)", "timestamp(3) with time zone") };
    let (t3, tz3) = if presto { ("time", "time with time zone") } else { ("time(3)", "time(3) with time zone") };
    exec(&mut s, &format!("CREATE TABLE dbine_tr.frac (id bigint, t {t3}, ts {ts}, tsz {tsz}, ttz {tz3})")).await;
    let fcols = ["id", "t", "ts", "tsz", "ttz"];
    let lossy = [
        (1, Cell::Time("23:59:59.9999".into())),
        (2, Cell::DateTime("2024-06-01 12:00:00.123999".into())),
        (3, Cell::DateTimeTz("2024-06-01 12:00:00.1234+01:00".into())),
        (4, Cell::Time("10:00:00.1234+01:00".into())),
    ];
    for (col, cell) in lossy {
        let mut row = vec![Cell::Int(col as i64), Cell::Null, Cell::Null, Cell::Null, Cell::Null];
        row[col] = cell;
        let r = s.bulk_load(&load_spec("frac", &fcols), &[], &mut batches(vec![row]), &|_| {}).await;
        assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("no se redondea")), "column {col}: {r:?}");
    }
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_tr.frac").await, 0);
    let row = vec![
        Cell::Int(9),
        Cell::Time("23:59:59.999000".into()),
        Cell::DateTime("2024-06-01 12:00:00.123000".into()),
        Cell::DateTimeTz("2024-06-01 12:00:00.500+00:00".into()),
        Cell::Time("10:00:00.250+00:00".into()),
    ];
    s.bulk_load(&load_spec("frac", &fcols), &[], &mut batches(vec![row]), &|_| {}).await.unwrap();
    let back = read_all(&mut s, "frac", None).await.rows;
    assert_eq!(back[0][1], Cell::Time("23:59:59.999".into()));
    assert_eq!(back[0][2], Cell::DateTime("2024-06-01 12:00:00.123".into()));

    clean(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn transfer_lone_rows_and_dates_trino() {
    lone_rows_and_dates("trino", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test]
#[ignore]
async fn transfer_lone_rows_and_dates_presto() {
    lone_rows_and_dates("presto", "DBINE_TEST_PRESTO_URL").await;
}

/// A table whose only column is a ROW loads (VALUES used to spread the
/// value over its fields), and a timestamp into a DATE never loses its
/// time of day silently. Light: a few rows.
async fn lone_rows_and_dates(driver: &str, var: &str) {
    let Some(c) = cfg(driver, var) else { return };
    let presto = driver == "presto";
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    clean(&mut s).await;
    exec(&mut s, "CREATE SCHEMA dbine_tr").await;

    exec(&mut s, "CREATE TABLE dbine_tr.lone (r row(a integer, b varchar))").await;
    exec(&mut s, "CREATE TABLE dbine_tr.lone2 (r row(a integer, b varchar))").await;
    exec(&mut s, "INSERT INTO dbine_tr.lone VALUES ROW(CAST(ROW(1, 'x') AS row(a integer, b varchar))), ROW(NULL), ROW(CAST(ROW(2, NULL) AS row(a integer, b varchar)))").await;
    let first = read_all(&mut s, "lone", None).await;
    let n = s.bulk_load(&load_spec("lone2", &["r"]), &first.columns, &mut batches(first.rows.clone()), &|_| {}).await.unwrap();
    assert_eq!(n, 3);
    let key = |mut v: Vec<Vec<Cell>>| {
        v.sort_by_key(|r| format!("{r:?}"));
        v
    };
    assert_eq!(key(read_all(&mut s, "lone2", None).await.rows), key(first.rows), "the ROW values must come back identical");
    // One row alone, too.
    let n = s.bulk_load(&load_spec("lone2", &["r"]), &[], &mut batches(vec![vec![Cell::Json("{\"a\":3,\"b\":\"z\"}".into())]]), &|_| {}).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_tr.lone2 WHERE r.a = 3 AND r.b = 'z'").await, 1);
    if !presto {
        exec(&mut s, "CREATE TABLE dbine_tr.lone_t (r row(t time(3)))").await;
        let n = s.bulk_load(&load_spec("lone_t", &["r"]), &[], &mut batches(vec![vec![Cell::Json("{\"t\":\"10:00:00.250\"}".into())]]), &|_| {}).await.unwrap();
        assert_eq!(n, 1);
    }

    exec(&mut s, "CREATE TABLE dbine_tr.days (id bigint, d date)").await;
    for (i, cell) in [
        Cell::DateTime("2024-01-01 12:00:00".into()),
        Cell::Text("2024-01-01 12:00:00".into()),
        Cell::DateTimeTz("2024-01-01 00:00:00+01:00".into()),
    ]
    .into_iter()
    .enumerate()
    {
        let r = s.bulk_load(&load_spec("days", &["id", "d"]), &[], &mut batches(vec![vec![Cell::Int(i as i64), cell]]), &|_| {}).await;
        assert!(matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("se perdería la hora")), "{r:?}");
    }
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_tr.days").await, 0);
    let rows = vec![
        vec![Cell::Int(1), Cell::DateTime("2024-01-01 00:00:00".into())],
        vec![Cell::Int(2), Cell::DateTimeTz("2024-01-01 21:00:00-03:00".into())],
    ];
    s.bulk_load(&load_spec("days", &["id", "d"]), &[], &mut batches(rows), &|_| {}).await.unwrap();
    let mut back = read_all(&mut s, "days", None).await.rows;
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(back[0][1], Cell::Date("2024-01-01".into()));
    assert_eq!(back[1][1], Cell::Date("2024-01-02".into()));

    clean(&mut s).await;
}

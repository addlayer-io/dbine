//! Bulk transfer against a real server:
//!
//! ```sh
//! docker start dbine-test-clickhouse   # -p 25123:8123, user dbine/dbine
//! cargo test --release -p dbine-driver-clickhouse -- --ignored transfer --nocapture
//! ```
//!
//! `DBINE_TEST_CLICKHOUSE_URL` overrides the server (default
//! `http://dbine:dbine@localhost:25123`).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn cfg() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_CLICKHOUSE_URL").unwrap_or_else(|_| "http://dbine:dbine@localhost:25123".into());
    let url = reqwest::Url::parse(&url).expect("URL");
    ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap()
}

async fn session() -> Box<dyn Session> {
    driver().connect(&cfg(), Some("dbine_transfer")).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_transfer".into()), name: name.into() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out.results.pop().map(|r| r.rows).unwrap_or_default()
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
    /// Only count (benchmarks).
    count_only: bool,
    rows: u64,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.rows += b.len() as u64;
        if !self.count_only {
            self.batches.push(b);
        }
        Ok(())
    }
}

struct Source(VecDeque<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read(s: &mut Box<dyn Session>, name: &str, count_only: bool) -> Collect {
    let sink = Arc::new(Mutex::new(Collect { count_only, ..Default::default() }));
    let spec = ReadSpec { table: table(name), columns: None, filter: None };
    let n = s.read_batches(&spec, sink.clone()).await.expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n, c.rows);
    c
}

fn load_spec(name: &str, columns: &[TransferColumn], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

const ALL_TYPES: &str = "(
    id UInt32, i8 Nullable(Int8), i64 Int64, u64 UInt64, i128 Int128, u256 UInt256, f32 Float32, f64 Nullable(Float64),
    b Bool, d32 Decimal(9, 4), d256 Nullable(Decimal(76, 20)), s Nullable(String), bin String, fs FixedString(4),
    lc LowCardinality(Nullable(String)), dt Date, d32e Date32, ts DateTime, ts_mad DateTime('Europe/Madrid'),
    ts64 Nullable(DateTime64(6, 'UTC')), u UUID, ip4 IPv4, ip6 IPv6, e Enum8('a''b' = 1, 'z' = -3),
    arr Array(Nullable(Int32)), m Map(String, Array(UInt8)), t Tuple(p UInt8, q String), j JSON, pt Point
) ENGINE = MergeTree ORDER BY id";

#[tokio::test]
#[ignore]
async fn transfer_all_types_round_trip() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    for t in ["src", "dst", "dst_native"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} {ALL_TYPES}")).await;
    }
    run(
        &mut s,
        r#"INSERT INTO src VALUES
        (1, -128, -9223372036854775808, 18446744073709551615, -170141183460469231731687303715884105728,
         115792089237316195423570985008687907853269984665640564039457584007913129639935, 1.5, 1e300,
         true, -12345.6789, 12345678901234567890123456789012345678901234567890.12345678901234567890,
         'héllo ''quoted''', unhex('FF00FE'), 'ab', 'low', '2149-06-06', '1900-01-01', '2024-02-29 23:59:59',
         '2024-07-01 10:00:00', '2024-01-01 00:00:00.123456', '61f0c404-5cb3-11e7-907b-a6006ad3dba0',
         '116.106.34.242', '2001:db8::ff00:42:8329', 'a''b', [1, NULL, 3], {'k': [1, 2]}, (7, 'x'),
         '{"a": 1, "b": {"c": [1, 2]}}', (1.5, -2.5)),
        (2, NULL, 0, 0, 0, 0, 0, NULL, false, 0, NULL, NULL, '', '', NULL, '1970-01-01', '1970-01-01',
         '1970-01-01 00:00:00', '2024-01-01 00:00:00', NULL, '00000000-0000-0000-0000-000000000000', '0.0.0.0', '::',
         'z', [], {}, (0, ''), '{}', (0, 0))"#,
    )
    .await;

    let got = read(&mut s, "src", false).await;
    assert_eq!(got.columns.len(), 29);
    let rows: Vec<Vec<Cell>> = got.batches.iter().flat_map(|b| b.rows.clone()).collect();
    assert_eq!(rows.len(), 2);
    let r = rows.iter().find(|r| r[0] == Cell::Int(1)).unwrap();
    assert_eq!(r[1], Cell::Int(-128));
    assert_eq!(r[3], Cell::UInt(u64::MAX));
    assert_eq!(r[4], Cell::Decimal("-170141183460469231731687303715884105728".into()));
    assert_eq!(r[9], Cell::Decimal("-12345.6789".into()));
    assert_eq!(r[10], Cell::Decimal("12345678901234567890123456789012345678901234567890.12345678901234567890".into()));
    assert_eq!(r[11], Cell::Text("héllo 'quoted'".into()));
    assert_eq!(r[12], Cell::Bytes(vec![0xff, 0x00, 0xfe]));
    assert_eq!(r[13], Cell::Text("ab".into()));
    assert_eq!(r[17], Cell::DateTimeTz("2024-02-29 23:59:59+00:00".into()));
    assert_eq!(r[18], Cell::DateTimeTz("2024-07-01 08:00:00+00:00".into()), "Madrid is UTC+2 in July");
    assert_eq!(r[19], Cell::DateTimeTz("2024-01-01 00:00:00.123456+00:00".into()));
    assert_eq!(r[20], Cell::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into()));
    assert_eq!(r[21], Cell::Text("116.106.34.242".into()));
    assert_eq!(r[23], Cell::Text("a'b".into()));
    assert_eq!(r[24], Cell::Json("[1,null,3]".into()));
    assert_eq!(r[26], Cell::Json(r#"{"p":7,"q":"x"}"#.into()));
    assert!(matches!(&r[27], Cell::Json(j) if j.contains("\"c\"")), "{:?}", r[27]);
    let n = rows.iter().find(|r| r[0] == Cell::Int(2)).unwrap();
    assert_eq!(n[1], Cell::Null);
    assert_eq!(n[11], Cell::Null);
    assert_eq!(n[19], Cell::Null);

    // Bulk load what was read, then compare the tables value by value.
    let spec = load_spec("dst", &got.columns, 1);
    let mut commits = Vec::new();
    let progress = |n: u64| commits.push(n);
    let progress = Mutex::new(progress);
    let loaded = s
        .bulk_load(&spec, &got.columns, &mut Source(got.batches.clone().into()), &|n| (progress.lock().unwrap())(n))
        .await
        .expect("bulk_load");
    assert_eq!(loaded, 2);
    drop(progress);
    assert_eq!(commits, vec![1, 2], "one commit per window");
    let all = "SELECT * EXCEPT j, toString(j) FROM {} ORDER BY id";
    assert_eq!(run(&mut s, &all.replace("{}", "src")).await, run(&mut s, &all.replace("{}", "dst")).await);

    // Native copy.
    let d = driver();
    let mut target = session().await;
    let spec = CopySpec {
        source: ReadSpec { table: table("src"), columns: None, filter: None },
        target: load_spec("dst_native", &got.columns, LoadSpec::DEFAULT_COMMIT_ROWS),
    };
    let copied = d.copy_native(&mut *s, &mut *target, &spec, &|_| {}).await.expect("copy_native");
    assert_eq!(copied, 2);
    assert_eq!(run(&mut s, &all.replace("{}", "src")).await, run(&mut s, &all.replace("{}", "dst_native")).await);

    // Different types: no native copy (the orchestrator falls back).
    run(&mut s, "DROP TABLE IF EXISTS other").await;
    run(&mut s, "CREATE TABLE other (id UInt64) ENGINE = MergeTree ORDER BY id").await;
    let spec = CopySpec {
        source: ReadSpec { table: table("src"), columns: Some(vec!["id".into()]), filter: None },
        target: LoadSpec { columns: vec!["id".into()], ..load_spec("other", &[], 10) },
    };
    assert!(matches!(d.copy_native(&mut *s, &mut *target, &spec, &|_| {}).await, Err(dbine_driver::Error::Unsupported(_))));

    // A filtered read.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table("src"), columns: Some(vec!["id".into(), "s".into()]), filter: Some("id = 2".into()) };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 1);
    assert_eq!(sink.lock().unwrap().batches[0].rows[0], vec![Cell::Int(2), Cell::Null]);

    // A naive time into a Madrid column lands at that wall-clock time.
    run(&mut s, "TRUNCATE TABLE dst").await;
    let mut row = rows[0].clone();
    row[18] = Cell::DateTime("2024-07-01 10:00:00".into());
    let spec = load_spec("dst", &got.columns, 100);
    s.bulk_load(&spec, &got.columns, &mut Source(vec![RowBatch { rows: vec![row], bytes: 0 }].into()), &|_| {}).await.unwrap();
    assert_eq!(run(&mut s, "SELECT toString(ts_mad) FROM dst").await, vec![vec![Value::String("2024-07-01 10:00:00".into())]]);

    // A value that doesn't fit fails the load and commits nothing of its window.
    run(&mut s, "TRUNCATE TABLE dst").await;
    let mut bad = rows[0].clone();
    bad[1] = Cell::Int(1000);
    let batch = RowBatch { rows: vec![rows[1].clone(), bad], bytes: 0 };
    let e = s.bulk_load(&spec, &got.columns, &mut Source(vec![batch].into()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("i8"), "{e}");
    assert_eq!(run(&mut s, "SELECT count() FROM dst").await, vec![vec![serde_json::json!(0)]]);

    // An error the server raises mid-stream is reported, not read as rows.
    let sink = Arc::new(Mutex::new(Collect { count_only: true, ..Default::default() }));
    run(&mut s, "DROP VIEW IF EXISTS failing").await;
    run(&mut s, "CREATE VIEW failing AS SELECT number, throwIf(number = 300000) AS x FROM numbers(1000000)").await;
    let spec = ReadSpec { table: table("failing"), columns: None, filter: None };
    let e = s.read_batches(&spec, sink).await.unwrap_err();
    assert!(e.to_string().contains("Code: 395"), "{e}");
}

#[tokio::test]
#[ignore]
async fn transfer_benchmark_1m_rows() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    let ddl = "(id UInt64, name String, amount Decimal(18, 4), ratio Float64, created DateTime64(3), flag Bool,
                note Nullable(String), code LowCardinality(String), uid UUID) ENGINE = MergeTree ORDER BY id";
    for t in ["bench_src", "bench_dst", "bench_native"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} {ddl}")).await;
    }
    run(
        &mut s,
        "INSERT INTO bench_src SELECT number, concat('name-', toString(number)), number / 7, number * 0.37,
         toDateTime64('2020-01-01 00:00:00', 3) + number, number % 2 = 0,
         if(number % 5 = 0, NULL, repeat('x', number % 40)), ['a', 'b', 'c'][number % 3 + 1], generateUUIDv4()
         FROM numbers(1000000)",
    )
    .await;
    let checksum = "SELECT count(), sum(cityHash64(id, name, amount, ratio, created, flag, note, code, uid)) FROM {}";

    let t = Instant::now();
    let got = read(&mut s, "bench_src", false).await;
    let read_s = t.elapsed().as_secs_f64();
    assert_eq!(got.rows, 1_000_000);

    let spec = load_spec("bench_dst", &got.columns, LoadSpec::DEFAULT_COMMIT_ROWS);
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &got.columns, &mut Source(got.batches.into()), &|_| {}).await.expect("bulk_load");
    let load_s = t.elapsed().as_secs_f64();
    assert_eq!(loaded, 1_000_000);
    assert_eq!(run(&mut s, &checksum.replace("{}", "bench_src")).await, run(&mut s, &checksum.replace("{}", "bench_dst")).await);

    let mut target = session().await;
    let spec = CopySpec {
        source: ReadSpec { table: table("bench_src"), columns: None, filter: None },
        target: load_spec("bench_native", &got.columns, LoadSpec::DEFAULT_COMMIT_ROWS),
    };
    let t = Instant::now();
    let copied = driver().copy_native(&mut *s, &mut *target, &spec, &|_| {}).await.expect("copy_native");
    let native_s = t.elapsed().as_secs_f64();
    assert_eq!(copied, 1_000_000);
    assert_eq!(run(&mut s, &checksum.replace("{}", "bench_src")).await, run(&mut s, &checksum.replace("{}", "bench_native")).await);

    println!(
        "clickhouse 1M rows: read {:.0} rows/s ({read_s:.2} s), bulk load {:.0} rows/s ({load_s:.2} s), native copy {:.0} rows/s ({native_s:.2} s)",
        1e6 / read_s,
        1e6 / load_s,
        1e6 / native_s
    );
}

/// Timeplus Proton (same crate): streams read through `table()`.
/// `DBINE_TEST_TIMEPLUS_URL` (default `http://localhost:25119`, container
/// `dbine-test-proton`).
#[tokio::test]
#[ignore]
async fn transfer_timeplus_streams() {
    let url = std::env::var("DBINE_TEST_TIMEPLUS_URL").unwrap_or_else(|_| "http://localhost:25119".into());
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig { driver: "timeplus".into(), host: url.host_str().unwrap().into(), port: url.port().unwrap_or(0), ..Default::default() };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "timeplus").unwrap();
    let mut s = d.connect(&c, None).await.expect("connect");
    let stream = |n: &str| ObjectRef { kind: kinds::STREAM.into(), schema: None, name: n.into() };
    for t in ["tp_src", "tp_dst", "tp_native"] {
        run(&mut s, &format!("DROP STREAM IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE STREAM {t} (id int32, name nullable(string), amount decimal(10, 2), at datetime64(3), tags array(string))")).await;
    }
    run(&mut s, "INSERT INTO tp_src (id, name, amount, at, tags) SELECT number, if(number % 3 = 0, NULL, to_string(number)), number / 4, to_datetime64('2024-01-01 00:00:00', 3) + number, ['a', 'b'] FROM numbers(5000)").await;
    let cols: Vec<String> = ["id", "name", "amount", "at", "tags"].iter().map(|s| s.to_string()).collect();
    let count = |t: &str| format!("SELECT count(), sum(id), count(name), sum(amount) FROM table({t})");
    // Wait until the rows are visible in the stream's historical store.
    for _ in 0..50 {
        if run(&mut s, &count("tp_src")).await[0][0] == serde_json::json!(5000) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: stream("tp_src"), columns: Some(cols.clone()), filter: None };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.expect("read"), 5000);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let load = LoadSpec { table: stream("tp_dst"), columns: cols.clone(), table_lock: false, keep_identity: false, commit_rows: 2000, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES };
    assert_eq!(s.bulk_load(&load, &got.columns, &mut Source(got.batches.into()), &|_| {}).await.expect("bulk_load"), 5000);
    let mut target = d.connect(&c, None).await.unwrap();
    let spec = CopySpec { source: spec, target: LoadSpec { table: stream("tp_native"), ..load } };
    assert_eq!(d.copy_native(&mut *s, &mut *target, &spec, &|_| {}).await.expect("copy_native"), 5000);
    let expected = run(&mut s, &count("tp_src")).await;
    for t in ["tp_dst", "tp_native"] {
        let mut got = Vec::new();
        for _ in 0..50 {
            got = run(&mut s, &count(t)).await;
            if got == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert_eq!(got, expected, "{t}");
    }
}

/// Yields `batches` batches of `rows` rows `(id, name)` with a pause
/// before each; `bad_at` makes one row's id not fit a UInt32.
struct Slow {
    left: u32,
    rows: u64,
    next_id: u64,
    pause: std::time::Duration,
    bad_at: Option<u64>,
}

#[dbine_driver::async_trait]
impl BatchSource for Slow {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        tokio::time::sleep(self.pause).await;
        let rows = (0..self.rows)
            .map(|_| {
                self.next_id += 1;
                let id = if Some(self.next_id) == self.bad_at { Cell::Int(-1) } else { Cell::UInt(self.next_id) };
                vec![id, Cell::Text(format!("row {}", self.next_id))]
            })
            .collect();
        Some(RowBatch { rows, bytes: 0 })
    }
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> Value {
    run(s, &format!("SELECT count() FROM {t}")).await[0][0].clone()
}

/// A cancelled or failed load commits nothing after it returns: the open
/// window's body is cut, never ended cleanly.
#[tokio::test]
#[ignore]
async fn transfer_cancelled_load_commits_nothing_late() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    run(&mut s, "DROP TABLE IF EXISTS cancel_dst").await;
    run(&mut s, "CREATE TABLE cancel_dst (id UInt32, name String) ENGINE = MergeTree ORDER BY id").await;
    let cols = vec![
        TransferColumn { name: "id".into(), type_name: "UInt32".into(), nullable: false },
        TransferColumn { name: "name".into(), type_name: "String".into(), nullable: false },
    ];
    let spec = load_spec("cancel_dst", &cols, 100_000);
    for i in 0..12 {
        let mut src = Slow { left: 20, rows: 5000, next_id: 0, pause: std::time::Duration::from_millis(100), bad_at: None };
        let cut = std::time::Duration::from_millis(500 + 50 * (i % 6));
        let r = tokio::time::timeout(cut, s.bulk_load(&spec, &cols, &mut src, &|_| {})).await;
        assert!(r.is_err(), "the load should still be running");
    }
    // A failed load (a value that doesn't fit, mid-window).
    let mut src = Slow { left: 20, rows: 5000, next_id: 0, pause: std::time::Duration::from_millis(20), bad_at: Some(42_000) };
    assert!(s.bulk_load(&spec, &cols, &mut src, &|_| {}).await.is_err());
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let mut s = session().await;
    assert_eq!(count(&mut s, "cancel_dst").await, serde_json::json!(0));
}

/// NULLs into non-nullable columns loaded through `input()` are errors;
/// zoned DateTimes nested in Array/Map/Tuple load; identifiers with `\`
/// and backticks are quoted; Maps keep repeated keys; nested non-UTF-8
/// bytes are refused; a failed read's error carries no row data.
#[tokio::test]
#[ignore]
async fn transfer_review_round1() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    let col = |n: &str, t: &str| TransferColumn { name: n.into(), type_name: t.into(), nullable: false };

    // 2. NULL into non-nullable DateTime('Asia/Tokyo') and JSON.
    run(&mut s, "DROP TABLE IF EXISTS nn").await;
    run(&mut s, "CREATE TABLE nn (id UInt8, d DateTime('Asia/Tokyo'), j JSON) ENGINE = MergeTree ORDER BY id").await;
    let cols = vec![col("id", "UInt8"), col("d", "DateTime"), col("j", "JSON")];
    for row in [vec![Cell::Int(1), Cell::Null, Cell::Json("{}".into())], vec![Cell::Int(1), Cell::DateTime("2024-01-01 00:00:00".into()), Cell::Null]] {
        let batch = RowBatch { rows: vec![row], bytes: 0 };
        let e = s.bulk_load(&load_spec("nn", &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.unwrap_err();
        assert!(e.to_string().contains("no admite nulos"), "{e}");
    }
    assert_eq!(count(&mut s, "nn").await, serde_json::json!(0));

    // 3. Zoned DateTimes nested in Array, Map and Tuple.
    run(&mut s, "DROP TABLE IF EXISTS nested_tz").await;
    run(
        &mut s,
        "CREATE TABLE nested_tz (id UInt8, a Array(DateTime('Asia/Tokyo')), m Map(String, DateTime64(3, 'Asia/Tokyo')), \
         t Tuple(x UInt8, y Array(Nullable(DateTime('Asia/Tokyo'))))) ENGINE = MergeTree ORDER BY id",
    )
    .await;
    let cols = vec![col("id", "UInt8"), col("a", "Array"), col("m", "Map"), col("t", "Tuple")];
    let row = vec![
        Cell::Int(1),
        Cell::Json(r#"["2024-01-01 10:00:00+00:00","2024-01-01 10:00:00"]"#.into()),
        Cell::Json(r#"{"k":"2024-01-01 10:00:00.123+02:00","k":"2024-01-02 10:00:00"}"#.into()),
        Cell::Json(r#"{"x":7,"y":[null,"2024-06-01T00:00:00Z"]}"#.into()),
    ];
    let loaded = s
        .bulk_load(&load_spec("nested_tz", &cols, 100), &cols, &mut Source(vec![RowBatch { rows: vec![row], bytes: 0 }].into()), &|_| {})
        .await
        .expect("nested zoned load");
    assert_eq!(loaded, 1);
    assert_eq!(
        run(&mut s, "SELECT toString(a), toString(m), toString(t) FROM nested_tz").await,
        vec![vec![
            Value::String("['2024-01-01 19:00:00','2024-01-01 10:00:00']".into()),
            Value::String("{'k':'2024-01-01 17:00:00.123','k':'2024-01-02 10:00:00.000'}".into()),
            Value::String("(7,[NULL,'2024-06-01 09:00:00'])".into()),
        ]]
    );
    // Garbage is an error, not a default.
    let row = vec![Cell::Int(2), Cell::Json(r#"["garbage"]"#.into()), Cell::Json("{}".into()), Cell::Json(r#"{"x":1,"y":[]}"#.into())];
    assert!(s
        .bulk_load(&load_spec("nested_tz", &cols, 100), &cols, &mut Source(vec![RowBatch { rows: vec![row], bytes: 0 }].into()), &|_| {})
        .await
        .is_err());

    // 4. Identifiers with a backslash and a backtick.
    let weird = ["a\\", "x\\`) SELECT * FROM system.one"];
    run(&mut s, "DROP TABLE IF EXISTS `we\\\\ird`").await;
    run(&mut s, "CREATE TABLE `we\\\\ird` (`a\\\\` UInt8, `x\\\\\\`) SELECT * FROM system.one` String) ENGINE = MergeTree ORDER BY tuple()").await;
    let names: Vec<String> = run(&mut s, "SELECT name FROM system.columns WHERE database = 'dbine_transfer' AND table = 'we\\\\ird' ORDER BY position")
        .await
        .into_iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, weird);
    let cols = vec![col(weird[0], "UInt8"), col(weird[1], "String")];
    let batch = RowBatch { rows: vec![vec![Cell::Int(5), Cell::Text("v".into())]], bytes: 0 };
    assert_eq!(s.bulk_load(&load_spec("we\\ird", &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.expect("load weird"), 1);
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table("we\\ird"), columns: Some(weird.iter().map(|s| s.to_string()).collect()), filter: None };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.expect("read weird"), 1);
    assert_eq!(sink.lock().unwrap().batches[0].rows[0], vec![Cell::Int(5), Cell::Text("v".into())]);

    // 5. A Map keeps repeated keys and their order; nested bytes are refused.
    run(&mut s, "DROP TABLE IF EXISTS maps").await;
    run(&mut s, "CREATE TABLE maps (id UInt8, m Map(String, UInt8)) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO maps VALUES (1, map('b', 1, 'a', 2, 'b', 3))").await;
    let got = read(&mut s, "maps", false).await;
    assert_eq!(got.batches[0].rows[0][1], Cell::Json(r#"{"b":1,"a":2,"b":3}"#.into()));
    run(&mut s, "TRUNCATE TABLE maps").await;
    s.bulk_load(&load_spec("maps", &got.columns, 100), &got.columns, &mut Source(got.batches.into()), &|_| {}).await.unwrap();
    assert_eq!(run(&mut s, "SELECT toString(m) FROM maps").await, vec![vec![Value::String("{'b':1,'a':2,'b':3}".into())]]);
    run(&mut s, "DROP TABLE IF EXISTS nested_bin").await;
    run(&mut s, "CREATE TABLE nested_bin (id UInt8, a Array(String)) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO nested_bin VALUES (1, [unhex('FF00FE')])").await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table("nested_bin"), columns: None, filter: None };
    assert!(matches!(s.read_batches(&spec, sink).await, Err(dbine_driver::Error::Unsupported(_))));

    // 6. A read that fails after the server buffered rows.
    run(&mut s, "DROP TABLE IF EXISTS many").await;
    run(&mut s, "CREATE TABLE many (id UInt64, s String) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO many SELECT number, repeat('x', 20) FROM numbers(100000)").await;
    run(&mut s, "DROP TABLE IF EXISTS many_dst").await;
    run(&mut s, "CREATE TABLE many_dst AS many").await;
    let mut target = session().await;
    let cols = vec![col("id", "UInt64"), col("s", "String")];
    for _ in 0..3 {
        let spec = CopySpec {
            source: ReadSpec { table: table("many"), columns: None, filter: Some("throwIf(id = 50000) = 0".into()) },
            target: load_spec("many_dst", &cols, 1_000_000),
        };
        let e = driver().copy_native(&mut *s, &mut *target, &spec, &|_| {}).await.unwrap_err().to_string();
        assert!(e.contains("Code: 395") && e.len() < 8192, "{} bytes: {}", e.len(), &e[..e.len().min(300)]);
        let sink = Arc::new(Mutex::new(Collect { count_only: true, ..Default::default() }));
        let e = s.read_batches(&spec.source, sink).await.unwrap_err().to_string();
        assert!(e.contains("Code: 395") && e.len() < 8192, "{} bytes", e.len());
    }
    assert_eq!(count(&mut s, "many_dst").await, serde_json::json!(0));
}

/// Round 2: a nullable zoned DateTime with NULLs and values in one window
/// loads (and garbage in it is still an error); Dynamic and Variant are
/// Unsupported in every path instead of coming back as Strings.
#[tokio::test]
#[ignore]
async fn transfer_review_round2() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    let col = |n: &str, t: &str| TransferColumn { name: n.into(), type_name: t.into(), nullable: false };

    // 1. Nullable(DateTime('Asia/Tokyo')) and Nullable(DateTime64(3, …)).
    run(&mut s, "DROP TABLE IF EXISTS ntz").await;
    run(&mut s, "DROP TABLE IF EXISTS ntz_dst").await;
    run(&mut s, "CREATE TABLE ntz (id UInt8, d Nullable(DateTime('Asia/Tokyo')), e Nullable(DateTime64(3, 'Asia/Tokyo'))) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "CREATE TABLE ntz_dst AS ntz").await;
    run(&mut s, "INSERT INTO ntz VALUES (1, NULL, '2024-01-01 00:00:00.250'), (2, '2024-01-01 00:00:00', NULL), (3, NULL, NULL)").await;
    let got = read(&mut s, "ntz", false).await;
    assert_eq!(got.batches[0].rows[0][1], Cell::Null);
    let n = s.bulk_load(&load_spec("ntz_dst", &got.columns, 100), &got.columns, &mut Source(got.batches.into()), &|_| {}).await.expect("load ntz");
    assert_eq!(n, 3);
    let q = |t: &str| format!("SELECT id, toString(d), toString(e) FROM {t} ORDER BY id");
    assert_eq!(run(&mut s, &q("ntz_dst")).await, run(&mut s, &q("ntz")).await);
    let cols = vec![col("id", "UInt8"), col("d", "DateTime"), col("e", "DateTime64")];
    let bad = vec![vec![Cell::Int(9), Cell::Null, Cell::Null], vec![Cell::Int(10), Cell::DateTimeTz("garbage".into()), Cell::Null]];
    assert!(s.bulk_load(&load_spec("ntz_dst", &cols, 100), &cols, &mut Source(vec![RowBatch { rows: bad, bytes: 0 }].into()), &|_| {}).await.is_err());
    assert_eq!(count(&mut s, "ntz_dst").await, serde_json::json!(3));

    // 2. Dynamic and Variant.
    for (name, ty) in [("dyn", "Dynamic"), ("var", "Variant(String, UInt64)")] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
        run(&mut s, &format!("DROP TABLE IF EXISTS {name}_dst")).await;
        run(&mut s, &format!("CREATE TABLE {name} (id UInt8, d {ty}) ENGINE = MergeTree ORDER BY id SETTINGS allow_experimental_dynamic_type = 1, allow_experimental_variant_type = 1")).await;
        run(&mut s, &format!("CREATE TABLE {name}_dst AS {name}")).await;
        run(&mut s, &format!("INSERT INTO {name} VALUES (1, NULL), (2, 42), (3, 'x')")).await;
        let sink = Arc::new(Mutex::new(Collect::default()));
        let spec = ReadSpec { table: table(name), columns: None, filter: None };
        let e = s.read_batches(&spec, sink).await.unwrap_err();
        assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e}");
        let cols = vec![col("id", "UInt8"), col("d", ty)];
        let batch = RowBatch { rows: vec![vec![Cell::Int(1), Cell::Null]], bytes: 0 };
        let e = s.bulk_load(&load_spec(&format!("{name}_dst"), &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.unwrap_err();
        assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e}");
        let mut target = session().await;
        let spec = CopySpec { source: ReadSpec { table: table(name), columns: None, filter: None }, target: load_spec(&format!("{name}_dst"), &cols, 100) };
        let e = driver().copy_native(&mut *s, &mut *target, &spec, &|_| {}).await.unwrap_err();
        assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e}");
        assert_eq!(count(&mut s, &format!("{name}_dst")).await, serde_json::json!(0));
    }
}

/// Round 3: impossible dates and times are errors in every path (UTC,
/// zoned, nested zoned), never another instant nor NULL; a tuple element
/// named with a `(` doesn't hide a Variant; a load dropped at any moment
/// has committed exactly what it reported when the drop returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn transfer_review_round3() {
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    let col = |n: &str, t: &str| TransferColumn { name: n.into(), type_name: t.into(), nullable: false };

    // A/B. Feb 31 and 99:99:99.
    for (name, ty) in [
        ("b_utc", "DateTime('UTC')"),
        ("b_utc64", "DateTime64(3, 'UTC')"),
        ("b_tok", "Nullable(DateTime('Asia/Tokyo'))"),
        ("a_arr", "Array(Nullable(DateTime('Asia/Tokyo')))"),
    ] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
        run(&mut s, &format!("CREATE TABLE {name} (id UInt8, d {ty}) ENGINE = MergeTree ORDER BY id")).await;
        let cols = vec![col("id", "UInt8"), col("d", ty)];
        for bad in ["2024-02-31 00:00:00+00:00", "2024-01-01 99:99:99+00:00"] {
            let cell = if ty.starts_with("Array") { Cell::Json(format!("[\"{bad}\", null]")) } else { Cell::DateTimeTz(bad.into()) };
            let batch = RowBatch { rows: vec![vec![Cell::Int(1), cell]], bytes: 0 };
            let r = s.bulk_load(&load_spec(name, &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await;
            assert!(r.is_err(), "{name} {bad}: {r:?}");
        }
        assert_eq!(count(&mut s, name).await, serde_json::json!(0), "{name}");
        // A real leap day loads.
        let cell = if ty.starts_with("Array") { Cell::Json("[\"2024-02-29 12:00:00+00:00\", null]".into()) } else { Cell::DateTimeTz("2024-02-29 12:00:00+00:00".into()) };
        let batch = RowBatch { rows: vec![vec![Cell::Int(1), cell]], bytes: 0 };
        s.bulk_load(&load_spec(name, &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.expect(name);
        let v = if ty.starts_with("Array") { "assumeNotNull(d[1])" } else { "assumeNotNull(d)" };
        let got = run(&mut s, &format!("SELECT toString(toUnixTimestamp({v})) FROM {name}")).await;
        assert_eq!(got[0][0], serde_json::json!("1709208000"), "{name}");
    }

    // C. A tuple element named `a(`.
    run(&mut s, "DROP TABLE IF EXISTS c_var").await;
    run(&mut s, "CREATE TABLE c_var (id UInt8, t Tuple(`a(` Variant(String, UInt64))) ENGINE = MergeTree ORDER BY id SETTINGS allow_experimental_variant_type = 1").await;
    run(&mut s, "INSERT INTO c_var VALUES (1, tuple(NULL)), (2, tuple(42::UInt64))").await;
    let e = s.read_batches(&ReadSpec { table: table("c_var"), columns: None, filter: None }, Arc::new(Mutex::new(Collect::default()))).await.unwrap_err();
    assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e}");

    // 3. Dropped at any moment: what's in the table when the drop returns
    // is what was reported, and nothing comes later.
    run(&mut s, "DROP TABLE IF EXISTS cancel3").await;
    run(&mut s, "CREATE TABLE cancel3 (id UInt32, name String) ENGINE = MergeTree ORDER BY id").await;
    let cols = vec![col("id", "UInt32"), col("name", "String")];
    let spec = load_spec("cancel3", &cols, 1_000_000);
    let start = Instant::now();
    let mut src = Slow { left: 4, rows: 5000, next_id: 0, pause: std::time::Duration::ZERO, bad_at: None };
    s.bulk_load(&spec, &cols, &mut src, &|_| {}).await.expect("full load");
    let full = start.elapsed();
    let mut committed_some = false;
    for i in 0..16u32 {
        run(&mut s, "TRUNCATE TABLE cancel3").await;
        let reported = AtomicU64::new(0);
        let progress = |n: u64| reported.store(n, SeqCst);
        let mut src = Slow { left: 4, rows: 5000, next_id: 0, pause: std::time::Duration::ZERO, bad_at: None };
        let cut = full * (40 + 5 * i) / 100;
        let _ = tokio::time::timeout(cut, s.bulk_load(&spec, &cols, &mut src, &progress)).await;
        let now = count(&mut s, "cancel3").await;
        assert_eq!(now, serde_json::json!(reported.load(SeqCst)), "cut at {cut:?}");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(count(&mut s, "cancel3").await, now, "cut at {cut:?}: rows committed after the load returned");
        committed_some |= reported.load(SeqCst) > 0;
    }
    eprintln!("full load {full:?}; some cut loads committed: {committed_some}");
}

/// A part the column can't hold (a time of day into a Date, fractions into
/// a DateTime, digits beyond a DateTime64's scale) makes the load fail and
/// commit nothing; the same values with that part at zero load.
#[tokio::test]
#[ignore]
async fn transfer_date_time_parts_are_never_dropped() {
    let mut s = driver().connect(&cfg(), None).await.expect("connect");
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_transfer").await;
    let mut s = session().await;
    let col = |n: &str, t: &str| TransferColumn { name: n.into(), type_name: t.into(), nullable: false };
    for (name, ty, bad, ok, shown) in [
        ("p_date", "Date", "2024-05-06 10:30:00", "2024-05-06 00:00:00", "2024-05-06"),
        ("p_date32", "Date32", "1950-05-06T00:00:00.5Z", "1950-05-06T00:00:00.000Z", "1950-05-06"),
        ("p_dt", "DateTime('UTC')", "2024-01-01 12:00:00.25", "2024-01-01 12:00:00.000", "2024-01-01 12:00:00"),
        ("p_dt64", "DateTime64(3, 'UTC')", "2024-01-01 12:00:00.1234", "2024-01-01 12:00:00.123000", "2024-01-01 12:00:00.123"),
        ("p_tok64", "Nullable(DateTime64(3, 'Asia/Tokyo'))", "2024-01-01 12:00:00.1234+00:00", "2024-01-01 12:00:00.1230+00:00", "2024-01-01 21:00:00.123"),
        ("p_arr", "Array(Date)", r#"["2024-01-01", "2024-01-02 00:00:01"]"#, r#"["2024-01-01", "2024-01-02 00:00:00"]"#, "['2024-01-01','2024-01-02']"),
    ] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {name}")).await;
        run(&mut s, &format!("CREATE TABLE {name} (id UInt8, d {ty}) ENGINE = MergeTree ORDER BY id")).await;
        let cols = vec![col("id", "UInt8"), col("d", ty)];
        let cell = |v: &str| if ty.starts_with("Array") { Cell::Json(v.into()) } else { Cell::DateTimeTz(v.into()) };
        // The bad row comes after a good one: nothing of the load stays.
        let batch = RowBatch { rows: vec![vec![Cell::Int(1), cell(ok)], vec![Cell::Int(2), cell(bad)]], bytes: 0 };
        let e = s.bulk_load(&load_spec(name, &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.unwrap_err();
        assert!(matches!(e, dbine_driver::Error::Query(_)) && e.to_string().contains("perder"), "{name}: {e}");
        assert_eq!(count(&mut s, name).await, serde_json::json!(0), "{name}");
        let batch = RowBatch { rows: vec![vec![Cell::Int(1), cell(ok)]], bytes: 0 };
        s.bulk_load(&load_spec(name, &cols, 100), &cols, &mut Source(vec![batch].into()), &|_| {}).await.expect(name);
        assert_eq!(run(&mut s, &format!("SELECT toString(d) FROM {name}")).await, vec![vec![Value::String(shown.into())]], "{name}");
        run(&mut s, &format!("DROP TABLE {name}")).await;
    }
}

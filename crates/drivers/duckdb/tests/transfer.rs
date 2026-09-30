//! Bulk transfer end to end on temporary database files: typed reads of
//! every DuckDB type, Appender loads with commit windows, the native copy
//! inside one instance and a load benchmark (1M rows in release;
//! `cargo test --release -p dbine-driver-duckdb --test transfer -- --nocapture`
//! prints the rates).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn temp_db(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("dbine-duck-xfer-{tag}-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}.wal", p.display()));
    p.to_string_lossy().to_string()
}

fn remove(p: &str) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(format!("{p}.wal"));
}

async fn open(path: &str, read_only: bool) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "duckdb".into(), host: path.into(), read_only, ..Default::default() };
    dbine_driver_duckdb::drivers().remove(0).connect(&cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("main".into()), name: name.into() }
}

fn all(name: &str) -> ReadSpec {
    ReadSpec { table: table(name), columns: None, filter: None }
}

#[derive(Default)]
struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.cols = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

async fn read_all(s: &mut Box<dyn Session>, spec: &ReadSpec) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    (c.cols, c.rows)
}

struct VecSource(std::vec::IntoIter<RowBatch>);

impl VecSource {
    fn new(rows: Vec<Vec<Cell>>, per: usize) -> Self {
        VecSource(rows.chunks(per).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect::<Vec<_>>().into_iter())
    }
}

#[dbine_driver::async_trait]
impl BatchSource for VecSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn load_spec(name: &str, cols: &[String], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: cols.to_vec(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

const DDL: &str = "CREATE TYPE mood AS ENUM ('ok', 'sad');
CREATE TABLE t (
    id INTEGER NOT NULL, b BOOLEAN, ti TINYINT, ub UBIGINT, h HUGEINT, uh UHUGEINT, f FLOAT, d DOUBLE,
    dec DECIMAL(38,10), dsmall DECIMAL(5,2), s VARCHAR, bl BLOB, dt DATE, tm TIME, ts TIMESTAMP, tsns TIMESTAMP_NS,
    tz TIMESTAMPTZ, u UUID, j JSON, l INTEGER[], st STRUCT(a INTEGER, b VARCHAR), m MAP(VARCHAR, INTEGER),
    iv INTERVAL, e mood, arr DOUBLE[2]
)";

const ROW: &str = "INSERT INTO t VALUES (
    1, true, -128, 18446744073709551615, -170141183460469231731687303715884105728, 340282366920938463463374607431768211455,
    1.5, -2.25e300, 1234567890123456789012345678.0123456789, -999.99, 'ñandú ''q''', '\\x00\\xFF'::BLOB,
    DATE '2024-02-29', TIME '13:45:00.123456', TIMESTAMP '2024-01-31 13:45:00.5', TIMESTAMP_NS '2024-01-31 13:45:00.123456789',
    TIMESTAMPTZ '2024-01-31 13:45:00.25+02:00', '123e4567-e89b-12d3-a456-426614174000', '{\"k\": [1, 2]}', [1, NULL, 3],
    {'a': 7, 'b': 'x'}, MAP {'k': 1}, INTERVAL '1 month 2 days 3 seconds', 'sad', [1.5, 2.5]
)";

#[tokio::test]
async fn read_is_typed() {
    let path = temp_db("read");
    let mut s = open(&path, false).await;
    run(&mut s, DDL).await;
    run(&mut s, ROW).await;
    run(&mut s, "INSERT INTO t (id) VALUES (2)").await;
    let (cols, rows) = read_all(&mut s, &all("t")).await;
    assert_eq!(cols.len(), 25);
    assert!(!cols[0].nullable && cols[1].nullable);
    assert_eq!(cols[8].type_name, "DECIMAL(38,10)");
    let r = &rows[0];
    let want = vec![
        Cell::Int(1),
        Cell::Bool(true),
        Cell::Int(-128),
        Cell::UInt(u64::MAX),
        Cell::Decimal(i128::MIN.to_string()),
        Cell::Decimal(u128::MAX.to_string()),
        Cell::Float(1.5),
        Cell::Float(-2.25e300),
        Cell::Decimal("1234567890123456789012345678.0123456789".into()),
        Cell::Decimal("-999.99".into()),
        Cell::Text("ñandú 'q'".into()),
        Cell::Bytes(vec![0, 255]),
        Cell::Date("2024-02-29".into()),
        Cell::Time("13:45:00.123456".into()),
        Cell::DateTime("2024-01-31 13:45:00.5".into()),
        Cell::DateTime("2024-01-31 13:45:00.123456789".into()),
        Cell::DateTimeTz("2024-01-31 11:45:00.25+00:00".into()),
        Cell::Uuid("123e4567-e89b-12d3-a456-426614174000".into()),
        Cell::Json("{\"k\": [1, 2]}".into()),
        Cell::Json("[1,null,3]".into()),
        Cell::Json("{\"a\":7,\"b\":\"x\"}".into()),
        Cell::Json("{\"k\":1}".into()),
        Cell::Text("1 month 2 days 00:00:03".into()),
        Cell::Text("sad".into()),
        Cell::Json("[1.5,2.5]".into()),
    ];
    for (i, (got, want)) in r.iter().zip(&want).enumerate() {
        assert_eq!(got, want, "column {}", cols[i].name);
    }
    assert!(rows[1][1..].iter().all(|c| *c == Cell::Null), "{:?}", rows[1]);

    // Subset and filter.
    let spec = ReadSpec { table: table("t"), columns: Some(vec!["s".into(), "id".into()]), filter: Some("id = 2".into()) };
    let (_, rows) = read_all(&mut s, &spec).await;
    assert_eq!(rows, vec![vec![Cell::Null, Cell::Int(2)]]);
    drop(s);
    remove(&path);
}

#[tokio::test]
async fn load_round_trip_every_type() {
    let (a, b) = (temp_db("rt-a"), temp_db("rt-b"));
    let mut src = open(&a, false).await;
    run(&mut src, DDL).await;
    run(&mut src, ROW).await;
    run(&mut src, "INSERT INTO t (id) VALUES (2)").await;
    let (cols, mut rows) = read_all(&mut src, &all("t")).await;
    // A 5 MB blob, whole.
    let big: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut third = rows[1].clone();
    third[0] = Cell::Int(3);
    third[11] = Cell::Bytes(big.clone());
    rows.push(third);
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();

    let mut dst = open(&b, false).await;
    run(&mut dst, DDL).await;
    let committed = Arc::new(Mutex::new(Vec::new()));
    let c2 = committed.clone();
    let mut source = VecSource::new(rows.clone(), 2);
    let n = dst.bulk_load(&load_spec("t", &names, 2), &cols, &mut source, &move |n| c2.lock().unwrap().push(n)).await.unwrap();
    assert_eq!(n, 3);
    assert_eq!(*committed.lock().unwrap(), vec![2, 3]);
    let (_, back) = read_all(&mut dst, &ReadSpec { table: table("t"), columns: None, filter: None }).await;
    for (i, (got, want)) in back.iter().zip(&rows).enumerate() {
        for (j, (g, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(g, w, "row {i}, column {}", names[j]);
        }
    }
    assert_eq!(back[2][11], Cell::Bytes(big));

    // A failing window rolls back; the committed one stays.
    run(&mut dst, "DELETE FROM t").await;
    let bad = vec![
        vec![Cell::Int(10)],
        vec![Cell::Null], // id is NOT NULL
    ];
    let mut source = VecSource::new(bad, 1);
    let e = dst.bulk_load(&load_spec("t", &["id".to_string()], 1), &[], &mut source, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("NOT NULL")), "{e:?}");
    let (_, back) = read_all(&mut dst, &ReadSpec { table: table("t"), columns: Some(vec!["id".into(), "s".into()]), filter: None }).await;
    assert_eq!(back, vec![vec![Cell::Int(10), Cell::Null]]);
    run(&mut dst, "INSERT INTO t (id) VALUES (11)").await; // no transaction left open

    // Text into typed columns is cast exactly.
    run(&mut dst, "DELETE FROM t").await;
    let cols2: Vec<String> = ["id", "dec", "ub", "dt", "tz", "u"].iter().map(|s| s.to_string()).collect();
    let row = vec![
        Cell::Text("5".into()),
        Cell::Decimal("0.0000000001".into()),
        Cell::Text("18446744073709551615".into()),
        Cell::Text("2000-01-01".into()),
        Cell::DateTimeTz("2024-06-01 00:00:00-03:00".into()),
        Cell::Uuid("00000000-0000-0000-0000-000000000001".into()),
    ];
    let mut source = VecSource::new(vec![row], 1);
    dst.bulk_load(&load_spec("t", &cols2, 100), &[], &mut source, &|_| {}).await.unwrap();
    let (_, back) = read_all(&mut dst, &ReadSpec { table: table("t"), columns: Some(cols2.clone()), filter: None }).await;
    assert_eq!(
        back[0],
        vec![
            Cell::Int(5),
            Cell::Decimal("0.0000000001".into()),
            Cell::UInt(u64::MAX),
            Cell::Date("2000-01-01".into()),
            Cell::DateTimeTz("2024-06-01 03:00:00+00:00".into()),
            Cell::Uuid("00000000-0000-0000-0000-000000000001".into())
        ]
    );
    drop((src, dst));
    remove(&a);
    remove(&b);
}

#[tokio::test]
async fn native_copy_inside_one_instance() {
    let (a, b) = (temp_db("nat-a"), temp_db("nat-b"));
    let d = dbine_driver_duckdb::drivers().remove(0);
    assert!(d.supports_bulk_load() && d.supports_native_copy("duckdb"));
    let mut s1 = open(&a, false).await;
    run(&mut s1, DDL).await;
    run(&mut s1, ROW).await;
    run(&mut s1, "CREATE TABLE t2 AS SELECT * FROM t LIMIT 0").await;
    let mut s2 = open(&a, false).await; // same file: same instance
    let names: Vec<String> = read_all(&mut s1, &all("t")).await.0.into_iter().map(|c| c.name).collect();
    let spec = CopySpec { source: all("t"), target: load_spec("t2", &names, 100) };
    let done = Arc::new(Mutex::new(0u64));
    let d2 = done.clone();
    assert_eq!(d.copy_native(&mut *s1, &mut *s2, &spec, &move |n| *d2.lock().unwrap() = n).await.unwrap(), 1);
    assert_eq!(*done.lock().unwrap(), 1);
    assert_eq!(read_all(&mut s1, &all("t")).await.1, read_all(&mut s2, &all("t2")).await.1);

    // Another file is another instance: the migration reads and appends.
    let mut other = open(&b, false).await;
    run(&mut other, DDL).await;
    let spec = CopySpec { source: all("t"), target: load_spec("t", &names, 100) };
    assert!(matches!(d.copy_native(&mut *s1, &mut *other, &spec, &|_| {}).await, Err(Error::Unsupported(_))));
    drop((s1, s2, other));
    remove(&a);
    remove(&b);
}

/// 1M rows (100k in debug builds): the Appender's rate, the read's and the
/// native copy's.
#[tokio::test]
async fn load_benchmark() {
    let n: i64 = if cfg!(debug_assertions) { 100_000 } else { 1_000_000 };
    let rows: Vec<Vec<Cell>> = (0..n)
        .map(|i| {
            vec![
                Cell::Int(i),
                Cell::Int(i * 7),
                Cell::Float(i as f64 / 3.0),
                Cell::Text(format!("row {i:08}")),
                Cell::Decimal(format!("{}.{:02}", i, i % 100)),
                Cell::DateTime("2024-01-31 13:45:00".into()),
            ]
        })
        .collect();
    let ddl = "CREATE TABLE big (id BIGINT, a BIGINT, f DOUBLE, s VARCHAR, dec DECIMAL(18,2), ts TIMESTAMP)";
    let cols: Vec<String> = ["id", "a", "f", "s", "dec", "ts"].iter().map(|s| s.to_string()).collect();
    let path = temp_db("bench");
    let mut s = open(&path, false).await;
    run(&mut s, &format!("{ddl}; {}", ddl.replace("big", "big2"))).await;
    let mut src = VecSource::new(rows, 1_000);
    let t = Instant::now();
    let loaded = s.bulk_load(&load_spec("big", &cols, LoadSpec::DEFAULT_COMMIT_ROWS), &[], &mut src, &|_| {}).await.unwrap();
    let load = t.elapsed();
    assert_eq!(loaded, n as u64);

    let t = Instant::now();
    let (_, back) = read_all(&mut s, &all("big")).await;
    let read = t.elapsed();
    assert_eq!(back.len(), n as usize);
    let hit = back.iter().find(|r| r[0] == Cell::Int(12_345)).unwrap();
    assert_eq!(hit[3], Cell::Text("row 00012345".into()));
    assert_eq!(hit[4], Cell::Decimal("12345.45".into()));
    drop(back);

    let mut s2 = open(&path, false).await;
    let d = dbine_driver_duckdb::drivers().remove(0);
    let spec = CopySpec { source: all("big"), target: load_spec("big2", &cols, 0) };
    let t = Instant::now();
    assert_eq!(d.copy_native(&mut *s, &mut *s2, &spec, &|_| {}).await.unwrap(), n as u64);
    let native = t.elapsed();
    let rate = |d: std::time::Duration| (n as f64 / d.as_secs_f64()) as u64;
    println!(
        "duckdb {n} rows: bulk_load {load:?} ({} rows/s), read_batches {read:?} ({} rows/s), copy_native {native:?} ({} rows/s)",
        rate(load),
        rate(read),
        rate(native)
    );
    drop((s, s2));
    remove(&path);
}

/// The first statement's rows, every cell as the grid gives it.
async fn grid(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<serde_json::Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, usize::MAX, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.remove(0).rows
}

async fn count(s: &mut Box<dyn Session>, table: &str) -> u64 {
    grid(s, &format!("SELECT CAST(count(*) AS VARCHAR) FROM {table}")).await[0][0].as_str().unwrap().parse().unwrap()
}

/// Hands its batches and then never ends (a source that hangs).
struct Hang(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Hang {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.0.next() {
            Some(b) => Some(b),
            None => std::future::pending().await,
        }
    }
}

fn numbered(n: i64) -> Vec<Vec<Cell>> {
    (0..n).map(|i| vec![Cell::Int(i), Cell::Text(format!("row {i}"))]).collect()
}

/// A dropped load (the orchestrator drops it on a failed read or a cancel)
/// never commits once the call is gone: what is in the table when the drop
/// returns is what stays.
#[tokio::test]
async fn dropped_load_never_commits_later() {
    let path = temp_db("late-load");
    let mut s = open(&path, false).await;
    run(&mut s, "CREATE TABLE t (id BIGINT, s VARCHAR)").await;
    let mut watch = open(&path, false).await; // same instance, another connection
    let cols = vec!["id".to_string(), "s".to_string()];
    for delay in [0u64, 1, 2, 5, 20] {
        run(&mut s, "DELETE FROM t").await;
        let mut source = Hang(VecSource::new(numbered(8_000), 1_000).0);
        let spec = load_spec("t", &cols, 1_000);
        let r = tokio::time::timeout(std::time::Duration::from_millis(delay), s.bulk_load(&spec, &[], &mut source, &|_| {})).await;
        assert!(r.is_err(), "the load can't end: its source hangs");
        let at_drop = count(&mut watch, "t").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(count(&mut watch, "t").await, at_drop, "rows committed after the drop ({delay} ms)");
        assert_eq!(at_drop % 1_000, 0);
    }
    // The session is left usable, with no transaction open.
    run(&mut s, "INSERT INTO t VALUES (-1, 'x')").await;
    assert_eq!(grid(&mut watch, "SELECT CAST(count(*) AS VARCHAR) FROM t WHERE id = -1").await[0][0], "1");
    drop((s, watch));
    remove(&path);
}

/// A dropped native copy is interrupted and rolled back, never committed
/// after the call is gone.
#[tokio::test]
async fn dropped_native_copy_never_commits_later() {
    let path = temp_db("late-copy");
    let mut s1 = open(&path, false).await;
    run(&mut s1, "CREATE TABLE src AS SELECT range AS id, 'row ' || range AS s FROM range(3000000)").await;
    run(&mut s1, "CREATE TABLE dst AS SELECT * FROM src LIMIT 0").await;
    let mut s2 = open(&path, false).await;
    let mut watch = open(&path, false).await;
    let d = dbine_driver_duckdb::drivers().remove(0);
    let cols = vec!["id".to_string(), "s".to_string()];
    // A source no copy gets through (10^12 rows): dropped at 50 ms, it is
    // always still running, however fast the machine.
    run(&mut s1, "CREATE VIEW huge AS SELECT range AS id, 'x' AS s FROM range(1000000000000)").await;
    let endless = CopySpec { source: all("huge"), target: load_spec("dst", &cols, 0) };
    let r = tokio::time::timeout(std::time::Duration::from_millis(50), d.copy_native(&mut *s1, &mut *s2, &endless, &|_| {})).await;
    assert!(r.is_err(), "the copy finished before the drop: {r:?}");
    let at_drop = count(&mut watch, "dst").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(count(&mut watch, "dst").await, at_drop, "rows committed after the drop");
    assert_eq!(at_drop, 0);
    // The target session is usable afterwards, in autocommit: the `INSERT`
    // was interrupted, not left running in the background.
    tokio::time::timeout(std::time::Duration::from_secs(10), run(&mut s2, "INSERT INTO dst VALUES (1, 'a')"))
        .await
        .expect("the dropped copy still holds the target session");
    assert_eq!(count(&mut watch, "dst").await, 1);
    // And a copy to the end still commits.
    run(&mut s2, "DELETE FROM dst").await;
    let spec = CopySpec { source: all("src"), target: load_spec("dst", &cols, 0) };
    assert_eq!(d.copy_native(&mut *s1, &mut *s2, &spec, &|_| {}).await.unwrap(), 3_000_000);
    assert_eq!(count(&mut watch, "dst").await, 3_000_000);
    drop((s1, s2, watch));
    remove(&path);
}

/// A filter is one condition: it can't carry other statements, nor write
/// through a function, whatever wraps the session.
#[tokio::test]
async fn filter_cannot_write_the_source() {
    let path = temp_db("filter");
    let mut owner = open(&path, false).await;
    run(&mut owner, "CREATE SEQUENCE sq; CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1), (2), (3); CREATE TABLE t2 (id INTEGER)").await;
    let mut src: Box<dyn Session> = Box::new(dbine_driver::read_only::ReadOnlySession::new(open(&path, false).await));
    let attacks = [
        "true; DELETE FROM main.t",
        "true; DELETE FROM main.t;",
        "true) ; DELETE FROM main.t; SELECT (1",
        "true; SELECT 1",
        "nextval('sq') > 0",
        "id IN (SELECT id FROM query('DELETE FROM main.t RETURNING id'))",
    ];
    for f in attacks {
        let spec = ReadSpec { table: table("t"), columns: None, filter: Some(f.into()) };
        let sink = Arc::new(Mutex::new(Collect::default()));
        let r = src.read_batches(&spec, sink).await;
        assert!(r.is_err(), "{f}: {r:?}");
        assert_eq!(count(&mut owner, "t").await, 3, "{f}");
    }
    // The sequence never moved, and the session isn't left in a transaction.
    assert_eq!(grid(&mut owner, "SELECT CAST(nextval('sq') AS VARCHAR)").await[0][0], "1");
    let spec = ReadSpec { table: table("t"), columns: None, filter: Some("id > 1 -- trailing comment".into()) };
    assert_eq!(read_all(&mut src, &spec).await.1, vec![vec![Cell::Int(2)], vec![Cell::Int(3)]]);

    // The native copy leaves a filtered read to the read-only path.
    let d = dbine_driver_duckdb::drivers().remove(0);
    let mut dst = open(&path, false).await;
    let spec = CopySpec { source: ReadSpec { table: table("t"), columns: None, filter: Some("true; DELETE FROM main.t".into()) }, target: load_spec("t2", &["id".to_string()], 0) };
    assert!(matches!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await, Err(Error::Unsupported(_))));
    assert_eq!(count(&mut owner, "t").await, 3);
    assert_eq!(count(&mut owner, "t2").await, 0);
    drop((owner, src, dst));
    remove(&path);
}

const NESTED: &str = r#"CREATE TABLE n (
    id INTEGER, d38 DECIMAL(38,10)[], d18 DECIMAL(18,3)[], sd STRUCT(d DECIMAL(38,10)), bl BLOB[], sb STRUCT(b BLOB),
    mk MAP(VARCHAR[], INTEGER), vi VARINT[], hu HUGEINT[], un UNION(n DECIMAL(38,10), s VARCHAR), tz TIMESTAMPTZ[],
    mb MAP(INTEGER, BLOB), fx DECIMAL(5,2)[2], nm STRUCT("my f" INTEGER, "q""x" VARCHAR[]), en ENUM('a,b', 'c''d')[],
    js JSON[], iv INTERVAL[], sl STRUCT(l DECIMAL(10,2)[], u UUID)[], ms MAP(VARCHAR, STRUCT(b BLOB))
)"#;

const NESTED_ROWS: &str = r#"INSERT INTO n VALUES (
    1, [1234567890123456789012345678.0123456789], [123456789012345.678], {'d': 1234567890123456789012345678.0123456789},
    ['\x00\xFF'::BLOB, 'a\x5Cb'::BLOB], {'b': '\x00'::BLOB}, MAP {['x']: 1, ['y,z', 'q''r']: 2},
    [123456789012345678901234567890123456789012345678901234567890::VARINT], ['-170141183460469231731687303715884105728'::HUGEINT],
    union_value(n := 12345678901234567890.0123456789::DECIMAL(38,10)),
    ['1850-01-01 00:00:00+00'::TIMESTAMPTZ, '-0043-03-15 00:00:00+00'::TIMESTAMPTZ, '2024-01-31 13:45:00.25+02'::TIMESTAMPTZ, 'infinity'::TIMESTAMPTZ],
    MAP {1: '\x00\x01'::BLOB}, [1.5, -999.99], {'my f': 7, 'q"x': ['a', NULL]}, ['a,b', 'c''d'],
    ['{"k":[1,2]}'::JSON, NULL], [INTERVAL '1 month 2 days 3 seconds'], [{'l': [0.01, NULL], 'u': '123e4567-e89b-12d3-a456-426614174000'}],
    MAP {'k': {'b': '\xDE\xAD'::BLOB}}
), (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
(3, [NULL], [], {'d': NULL}, [NULL], {'b': NULL}, MAP {}, [], [NULL], union_value(s := 'x'), [NULL], MAP {}, NULL,
    {'my f': NULL, 'q"x': NULL}, [], [], [], [NULL], MAP {'k': NULL})"#;

/// Every value of the table as DuckDB's own text, row by row.
async fn as_text(s: &mut Box<dyn Session>, table: &str) -> Vec<Vec<serde_json::Value>> {
    grid(s, &format!("SELECT CAST(COLUMNS(*) AS VARCHAR) FROM {table} ORDER BY 1")).await
}

/// Nested values keep every digit and byte, and load back into the same
/// types, between two instances (where the native copy doesn't apply).
#[tokio::test]
async fn nested_values_round_trip_exactly() {
    let (a, b) = (temp_db("nest-a"), temp_db("nest-b"));
    let mut src = open(&a, false).await;
    run(&mut src, NESTED).await;
    run(&mut src, NESTED_ROWS).await;
    let (cols, rows) = read_all(&mut src, &all("n")).await;
    let json = |c: &Cell| match c {
        Cell::Json(s) => serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|e| panic!("{s}: {e}")),
        other => panic!("{other:?}"),
    };
    // Exact digits and bytes, also for the other engines that parse it.
    assert_eq!(json(&rows[0][1]), serde_json::json!(["1234567890123456789012345678.0123456789"]));
    assert_eq!(json(&rows[0][3]), serde_json::json!({"d": "1234567890123456789012345678.0123456789"}));
    assert_eq!(json(&rows[0][4]), serde_json::json!(["\\x00\\xFF", "a\\x5Cb"]));
    assert_eq!(json(&rows[0][6]), serde_json::json!({"[x]": 1, "['y,z', 'q\\'r']": 2}));
    assert_eq!(json(&rows[0][10]), serde_json::json!(["1850-01-01 00:00:00+00:00", "0044-03-15 (BC) 00:00:00+00:00", "2024-01-31 11:45:00.25+00:00", "infinity"]));
    assert_eq!(rows[1][1..], vec![Cell::Null; 18][..]);
    // The same read, filtered (the whole statement is checked as one SELECT).
    let spec = ReadSpec { table: table("n"), columns: None, filter: Some("id = 1".into()) };
    assert_eq!(read_all(&mut src, &spec).await.1, vec![rows[0].clone()]);

    let mut dst = open(&b, false).await;
    run(&mut dst, NESTED).await;
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let mut source = VecSource::new(rows, 2);
    assert_eq!(dst.bulk_load(&load_spec("n", &names, 2), &cols, &mut source, &|_| {}).await.unwrap(), 3);
    let (want, got) = (as_text(&mut src, "n").await, as_text(&mut dst, "n").await);
    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
        for (j, (w, g)) in w.iter().zip(g).enumerate() {
            assert_eq!(g, w, "row {i}, column {}", names[j]);
        }
    }
    assert_eq!(want.len(), got.len());
    // Instants compared as instants (the session's zone text drops historic
    // offsets' seconds).
    let instants = "SELECT CAST(list_transform(tz, lambda x: CASE WHEN isfinite(x) THEN CAST(epoch_us(x) AS VARCHAR) ELSE CAST(x AS VARCHAR) END) AS VARCHAR) FROM n ORDER BY id";
    assert_eq!(grid(&mut src, instants).await, grid(&mut dst, instants).await);
    drop((src, dst));
    remove(&a);
    remove(&b);
}

/// A `JSON` nested in a `STRUCT` or list keeps its exact text (not
/// re-encoded by `to_json`, whose numbers go through `double`), and NaN and
/// ±infinity inside nested floats travel as valid JSON (the strings
/// `"NaN"`, `"Infinity"`, `"-Infinity"`) that loads back into the same float.
#[tokio::test]
async fn nested_json_and_nonfinite_floats_round_trip() {
    let (a, b) = (temp_db("nf-a"), temp_db("nf-b"));
    let mut src = open(&a, false).await;
    let ddl = "CREATE TABLE f (id INTEGER, d DOUBLE[], r FLOAT[], s STRUCT(d DOUBLE, f FLOAT), m MAP(VARCHAR, DOUBLE), j STRUCT(j JSON), l JSON[])";
    run(&mut src, ddl).await;
    run(
        &mut src,
        r#"INSERT INTO f VALUES
        (1, ['nan'::DOUBLE, 'inf', '-inf', -0.0, 5e-324, 1.7976931348623157e308, NULL], ['nan'::FLOAT, 'inf', '-inf', 3.4028235e38, 0.1],
            {'d': 'nan', 'f': '-inf'}, MAP {'a': 'inf'::DOUBLE, 'b': 1.5},
            {'j': '{"a":1.00000000000000000001,"b":12345678901234567890123}'}, ['{"x":0.10000000000000000001}', '[1e400]', NULL]),
        (2, NULL, NULL, NULL, NULL, NULL, NULL)"#,
    )
    .await;
    let (cols, rows) = read_all(&mut src, &all("f")).await;
    let json = |c: &Cell| match c {
        Cell::Json(s) => serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|e| panic!("not JSON: {s}: {e}")),
        other => panic!("{other:?}"),
    };
    for c in &rows[0][1..] {
        json(c);
    }
    assert_eq!(json(&rows[0][1]).as_array().unwrap()[..3], [serde_json::json!("NaN"), serde_json::json!("Infinity"), serde_json::json!("-Infinity")]);
    assert_eq!(json(&rows[0][3]), serde_json::json!({"d": "NaN", "f": "-Infinity"}));
    assert_eq!(json(&rows[0][4]), serde_json::json!({"a": "Infinity", "b": 1.5}));
    assert_eq!(rows[0][5], Cell::Json(r#"{"j":"{\"a\":1.00000000000000000001,\"b\":12345678901234567890123}"}"#.into()));

    let mut dst = open(&b, false).await;
    run(&mut dst, ddl).await;
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let mut source = VecSource::new(rows, 1);
    assert_eq!(dst.bulk_load(&load_spec("f", &names, 1), &cols, &mut source, &|_| {}).await.unwrap(), 2);
    let (want, got) = (as_text(&mut src, "f").await, as_text(&mut dst, "f").await);
    assert_eq!(got, want);
    // Bit for bit (the text of -0.0 and of the extremes included).
    let bits = "SELECT list_transform(d, lambda x: CAST(x AS VARCHAR)), CAST(list_transform(r, lambda x: CAST(x AS VARCHAR)) AS VARCHAR) FROM f ORDER BY id";
    assert_eq!(grid(&mut src, bits).await, grid(&mut dst, bits).await);
    // What another engine sends: the embedded document as JSON (not its
    // text), and plain numbers.
    let other = vec![vec![Cell::Int(3), Cell::Json(r#"[1.5,"NaN"]"#.into()), Cell::Json(r#"{"j":{"a":[1,"x"]}}"#.into())]];
    let two = vec!["id".to_string(), "d".to_string(), "j".to_string()];
    let mut source = VecSource::new(other, 1);
    assert_eq!(dst.bulk_load(&load_spec("f", &two, 1), &cols, &mut source, &|_| {}).await.unwrap(), 1);
    let q = "SELECT CAST(d AS VARCHAR), CAST(j.j AS VARCHAR) FROM f WHERE id = 3";
    assert_eq!(grid(&mut dst, q).await, vec![vec![serde_json::json!("[1.5, nan]"), serde_json::json!(r#"{"a":[1,"x"]}"#)]]);
    drop((src, dst));
    remove(&a);
    remove(&b);
}

/// A `MAP` keyed by floats keeps NaN, ±infinity and -0.0 as keys (keys go
/// as their own text, not as the JSON leaf of a nested float value). And a
/// plain text where a nested `JSON` goes fails the load with the reason,
/// leaving nothing.
#[tokio::test]
async fn float_map_keys_and_json_leaf_texts() {
    let (a, b) = (temp_db("fk-a"), temp_db("fk-b"));
    let mut src = open(&a, false).await;
    let ddl = "CREATE TABLE k (id INTEGER, m MAP(DOUBLE, DOUBLE), f MAP(FLOAT, INTEGER)[], j STRUCT(j JSON), l JSON[])";
    run(&mut src, ddl).await;
    run(
        &mut src,
        "INSERT INTO k VALUES (1, MAP {'nan'::DOUBLE: '-inf'::DOUBLE, -0.0: 0.1, 'inf': 'nan', 5e-324: 1}, [MAP {'-inf'::FLOAT: 1, 0.1: 2}], NULL, NULL)",
    )
    .await;
    let (cols, rows) = read_all(&mut src, &all("k")).await;
    let mut dst = open(&b, false).await;
    run(&mut dst, ddl).await;
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let mut source = VecSource::new(rows, 1);
    assert_eq!(dst.bulk_load(&load_spec("k", &names, 1), &cols, &mut source, &|_| {}).await.unwrap(), 1);
    assert_eq!(as_text(&mut dst, "k").await, as_text(&mut src, "k").await);
    let keys = "SELECT CAST(list_transform(map_keys(m), lambda x: CAST(x AS VARCHAR)) AS VARCHAR) FROM k";
    let want = grid(&mut src, keys).await;
    assert!(want[0][0].as_str().unwrap().starts_with("[nan, "), "{want:?}");
    assert_eq!(grid(&mut dst, keys).await, want);

    for bad in [r#"{"j":"text"}"#, r#"{"j":"[1,"}"#] {
        let row = vec![vec![Cell::Int(2), Cell::Json(bad.into()), Cell::Null]];
        let two = vec!["id".to_string(), "j".to_string(), "l".to_string()];
        let mut source = VecSource::new(row, 1);
        let e = dst.bulk_load(&load_spec("k", &two, 1), &cols, &mut source, &|_| {}).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("texto del documento")), "{e:?}");
    }
    let row = vec![vec![Cell::Int(3), Cell::Null, Cell::Json(r#"[{"a":1},2,"x",null]"#.into())]];
    let two = vec!["id".to_string(), "j".to_string(), "l".to_string()];
    let mut source = VecSource::new(row, 1);
    let e = dst.bulk_load(&load_spec("k", &two, 1), &cols, &mut source, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("texto del documento")), "{e:?}");
    assert_eq!(count(&mut dst, "k").await, 1);
    drop((src, dst));
    remove(&a);
    remove(&b);
}

/// `TIMESTAMPTZ` over DuckDB's whole range: UTC in DuckDB's own text,
/// read back into the same instants.
#[tokio::test]
async fn timestamptz_whole_range_round_trips() {
    let (a, b) = (temp_db("tz-a"), temp_db("tz-b"));
    let mut src = open(&a, false).await;
    let ddl = "CREATE TABLE z (id INTEGER, x TIMESTAMPTZ)";
    run(&mut src, ddl).await;
    run(
        &mut src,
        "INSERT INTO z VALUES (1, '294246-12-31 23:59:59.999999+00'), (2, '12000-01-01 00:00:00+00'), (3, '-0043-03-15 00:00:00+00'),
         (4, '1850-01-01 00:00:00+00'), (5, 'infinity'), (6, '-infinity'), (7, '290309-12-22 (BC) 00:00:00+00'),
         (8, '2024-01-31 13:45:00.25+02'), (9, NULL)",
    )
    .await;
    let (cols, rows) = read_all(&mut src, &all("z")).await;
    let texts: Vec<Cell> = rows.iter().map(|r| r[1].clone()).collect();
    let tz = |s: &str| Cell::DateTimeTz(s.into());
    assert_eq!(
        texts,
        vec![
            tz("294246-12-31 23:59:59.999999+00:00"),
            tz("12000-01-01 00:00:00+00:00"),
            tz("0044-03-15 (BC) 00:00:00+00:00"),
            tz("1850-01-01 00:00:00+00:00"),
            tz("infinity"),
            tz("-infinity"),
            tz("290309-12-22 (BC) 00:00:00+00:00"),
            tz("2024-01-31 11:45:00.25+00:00"),
            Cell::Null,
        ]
    );
    let mut dst = open(&b, false).await;
    run(&mut dst, ddl).await;
    let names = vec!["id".to_string(), "x".to_string()];
    let mut source = VecSource::new(rows, 4);
    assert_eq!(dst.bulk_load(&load_spec("z", &names, 4), &cols, &mut source, &|_| {}).await.unwrap(), 9);
    let q = "SELECT id, CASE WHEN isfinite(x) THEN CAST(epoch_us(x) AS VARCHAR) ELSE CAST(x AS VARCHAR) END FROM z ORDER BY id";
    assert_eq!(grid(&mut src, q).await, grid(&mut dst, q).await);
    drop((src, dst));
    remove(&a);
    remove(&b);
}

/// Column names match as DuckDB's identifiers do: without regard to case.
#[tokio::test]
async fn columns_match_without_case() {
    let path = temp_db("case");
    let mut s = open(&path, false).await;
    run(&mut s, r#"CREATE TABLE m ("Mixed" INTEGER, other VARCHAR, l INTEGER[])"#).await;
    run(&mut s, "INSERT INTO m VALUES (1, 'a', [1])").await;
    let spec = ReadSpec { table: table("m"), columns: Some(vec!["mixed".into(), "OTHER".into()]), filter: None };
    let (cols, rows) = read_all(&mut s, &spec).await;
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Mixed", "other"]);
    assert_eq!(rows, vec![vec![Cell::Int(1), Cell::Text("a".into())]]);
    let load: Vec<String> = ["MIXED", "Other", "L"].iter().map(|s| s.to_string()).collect();
    let mut source = VecSource::new(vec![vec![Cell::Int(2), Cell::Text("b".into()), Cell::Json("[2]".into())]], 1);
    s.bulk_load(&load_spec("m", &load, 10), &[], &mut source, &|_| {}).await.unwrap();
    assert_eq!(count(&mut s, "m").await, 2);
    let spec = ReadSpec { table: table("m"), columns: Some(vec!["nope".into()]), filter: None };
    assert!(s.read_batches(&spec, Arc::new(Mutex::new(Collect::default()))).await.is_err());
    drop(s);
    remove(&path);
}

/// Checks, on each `next()`, that the load already took (and here, with a
/// window per batch, committed) every batch handed before.
struct Paced {
    batches: std::vec::IntoIter<RowBatch>,
    handed: u64,
    committed: Arc<Mutex<u64>>,
    ahead: Arc<Mutex<Vec<(u64, u64)>>>,
}

#[dbine_driver::async_trait]
impl BatchSource for Paced {
    async fn next(&mut self) -> Option<RowBatch> {
        let done = *self.committed.lock().unwrap();
        if done != self.handed * 1_000 {
            self.ahead.lock().unwrap().push((self.handed, done));
        }
        let b = self.batches.next()?;
        self.handed += 1;
        Some(b)
    }
}

/// The load holds no batches of its own beyond the one it is appending:
/// the next one is asked for only once the last one was taken.
#[tokio::test]
async fn load_asks_for_a_batch_only_after_taking_the_last() {
    let path = temp_db("paced");
    let mut s = open(&path, false).await;
    run(&mut s, "CREATE TABLE t (id BIGINT, s VARCHAR)").await;
    let committed = Arc::new(Mutex::new(0u64));
    let ahead = Arc::new(Mutex::new(Vec::new()));
    let mut source = Paced { batches: VecSource::new(numbered(6_000), 1_000).0, handed: 0, committed: committed.clone(), ahead: ahead.clone() };
    let c2 = committed.clone();
    let cols = vec!["id".to_string(), "s".to_string()];
    assert_eq!(s.bulk_load(&load_spec("t", &cols, 1_000), &[], &mut source, &move |n| *c2.lock().unwrap() = n).await.unwrap(), 6_000);
    assert!(ahead.lock().unwrap().is_empty(), "asked ahead (handed, committed): {:?}", ahead.lock().unwrap());
    drop(s);
    remove(&path);
}

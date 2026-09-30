//! Bulk transfer against a real server:
//!
//! ```sh
//! docker start dbine-test-firebird   # -p 25602:3050, user dbine/dbine, test.fdb
//! cargo test --release -p dbine-driver-firebird -- --ignored transfer --nocapture
//! ```
//!
//! `DBINE_TEST_FIREBIRD_URL` overrides the server (default
//! `firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb`).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL")
        .unwrap_or_else(|_| "firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb".into());
    let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn session() -> Box<dyn Session> {
    let driver = dbine_driver_firebird::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    driver.connect(&config(), None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1_000_000, &mut out).await {
        panic!("{sql}: {e}");
    }
    out.results.pop().map(|r| r.rows).unwrap_or_default()
}

async fn drop_table(s: &mut Box<dyn Session>, t: &str) {
    let _ = s.execute(&format!("DROP TABLE {t}"), 10, &mut QueryOutcome::default()).await;
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: name.into() }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
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

async fn read(s: &mut Box<dyn Session>, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.batches.iter().map(RowBatch::len).sum::<usize>());
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

const ALL_TYPES: &str = "(ID INTEGER NOT NULL PRIMARY KEY, SI SMALLINT, BI BIGINT, I128 INT128, N NUMERIC(18,4),
    D DECIMAL(9,2), N38 NUMERIC(38,10), F FLOAT, DP DOUBLE PRECISION, DF DECFLOAT(34), B BOOLEAN, C CHAR(5),
    VC VARCHAR(100), OCT CHAR(16) CHARACTER SET OCTETS, VB VARCHAR(10) CHARACTER SET OCTETS, DT DATE, TM TIME,
    TS TIMESTAMP, TSZ TIMESTAMP WITH TIME ZONE, TMZ TIME WITH TIME ZONE, BT BLOB SUB_TYPE TEXT, BB BLOB SUB_TYPE BINARY,
    CALC COMPUTED BY (ID * 2))";

#[tokio::test]
#[ignore]
async fn transfer_all_types_round_trip() {
    let mut s = session().await;
    for t in ["DBINE_XFER_SRC", "DBINE_XFER_DST"] {
        drop_table(&mut s, t).await;
        run(&mut s, &format!("CREATE TABLE {t} {ALL_TYPES}")).await;
    }
    run(
        &mut s,
        "INSERT INTO DBINE_XFER_SRC (ID, SI, BI, I128, N, D, N38, F, DP, DF, B, C, VC, OCT, VB, DT, TM, TS, TSZ, TMZ, BT, BB)
         VALUES (1, -32768, -9223372036854775808, -170141183460469231731687303715884105728, -12345678901234.5678,
         -0.5, 1234567890123456789012345678.0123456789, 1.5, 1e300, 1.5E+300, TRUE, 'ab', 'héllo ''q''',
         x'61F0C4045CB311E7907BA6006AD3DBA0', x'00FF', '2024-02-29', '13:45:00.1234', '2024-02-29 23:59:59.9999',
         '2024-07-01 10:00:00 +02:00', '10:00:00 +02:00', 'long text ' || LPAD(CAST('' AS BLOB SUB_TYPE TEXT), 40000, 'x'), x'DEADBEEF')",
    )
    .await;
    run(&mut s, "INSERT INTO DBINE_XFER_SRC (ID) VALUES (2)").await;
    run(&mut s, "INSERT INTO DBINE_XFER_SRC (ID, TSZ) VALUES (3, '2024-01-01 10:00:00 America/Sao_Paulo')").await;
    run(&mut s, "COMMIT").await;

    let got = read(&mut s, ReadSpec { table: table("DBINE_XFER_SRC"), columns: None, filter: None }).await;
    assert_eq!(got.columns.len(), 22, "the computed column isn't read");
    assert_eq!(got.columns[4].type_name, "NUMERIC(18,4)");
    let rows: Vec<Vec<Cell>> = got.batches.iter().flat_map(|b| b.rows.clone()).collect();
    let r = rows.iter().find(|r| r[0] == Cell::Int(1)).unwrap();
    assert_eq!(r[1], Cell::Int(-32768));
    assert_eq!(r[3], Cell::Decimal("-170141183460469231731687303715884105728".into()));
    assert_eq!(r[4], Cell::Decimal("-12345678901234.5678".into()));
    assert_eq!(r[5], Cell::Decimal("-0.50".into()));
    assert_eq!(r[6], Cell::Decimal("1234567890123456789012345678.0123456789".into()));
    assert_eq!(r[8], Cell::Float(1e300));
    assert_eq!(r[10], Cell::Bool(true));
    assert_eq!(r[11], Cell::Text("ab   ".into()));
    assert_eq!(r[12], Cell::Text("héllo 'q'".into()));
    assert_eq!(r[13], Cell::Bytes(vec![0x61, 0xF0, 0xC4, 0x04, 0x5C, 0xB3, 0x11, 0xE7, 0x90, 0x7B, 0xA6, 0x00, 0x6A, 0xD3, 0xDB, 0xA0]));
    assert_eq!(r[14], Cell::Bytes(vec![0x00, 0xFF]));
    assert_eq!(r[15], Cell::Date("2024-02-29".into()));
    assert_eq!(r[16], Cell::Time("13:45:00.1234".into()));
    assert_eq!(r[17], Cell::DateTime("2024-02-29 23:59:59.9999".into()));
    // The zone is kept: an offset as such, a region as the server's text.
    assert_eq!(r[18], Cell::DateTimeTz("2024-07-01 10:00:00.0000+02:00".into()));
    let sp = rows.iter().find(|r| r[0] == Cell::Int(3)).unwrap();
    assert_eq!(sp[18], Cell::Text("2024-01-01 10:00:00.0000 America/Sao_Paulo".into()));
    assert!(matches!(&r[20], Cell::Text(t) if t.len() == 40_010));
    assert_eq!(r[21], Cell::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let n = rows.iter().find(|r| r[0] == Cell::Int(2)).unwrap();
    assert!(n[1..].iter().all(|c| *c == Cell::Null), "{n:?}");

    // Load what was read and compare value by value.
    let spec = load_spec("DBINE_XFER_DST", &got.columns, 1);
    let commits = Mutex::new(Vec::new());
    let loaded = s
        .bulk_load(&spec, &got.columns, &mut Source(got.batches.clone().into()), &|n| commits.lock().unwrap().push(n))
        .await
        .expect("bulk_load");
    assert_eq!(loaded, 3);
    assert_eq!(*commits.lock().unwrap(), vec![1, 2, 3]);
    // Every value, the time zones' own zone included.
    let all = "SELECT t.*, CAST(TSZ AS VARCHAR(64)), EXTRACT(TIMEZONE_HOUR FROM TSZ) FROM {} t ORDER BY ID";
    let src = run(&mut s, &all.replace("{}", "DBINE_XFER_SRC")).await;
    assert_eq!(src[2][23], Value::String("2024-01-01 10:00:00.0000 America/Sao_Paulo".into()));
    assert_eq!(src, run(&mut s, &all.replace("{}", "DBINE_XFER_DST")).await);

    // Values from other engines' spellings.
    run(&mut s, "DELETE FROM DBINE_XFER_DST").await;
    run(&mut s, "COMMIT").await;
    let cols: Vec<String> = ["ID", "N", "OCT", "TS", "TSZ", "B"].iter().map(|s| s.to_string()).collect();
    let row = vec![
        Cell::UInt(7),
        Cell::Float(2.5),
        Cell::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into()),
        Cell::DateTimeTz("2024-01-01 01:00:00.123456+02:00".into()),
        Cell::DateTime("2024-01-01T10:00:00".into()),
        Cell::Int(1),
    ];
    let spec = LoadSpec { columns: cols.clone(), ..load_spec("DBINE_XFER_DST", &[], 100) };
    s.bulk_load(&spec, &[], &mut Source(vec![RowBatch { rows: vec![row], bytes: 0 }].into()), &|_| {}).await.unwrap();
    let back = read(&mut s, ReadSpec { table: table("DBINE_XFER_DST"), columns: Some(cols), filter: Some("ID = 7".into()) }).await;
    assert_eq!(
        back.batches[0].rows[0][..4],
        [
            Cell::Int(7),
            Cell::Decimal("2.5000".into()),
            Cell::Bytes(vec![0x61, 0xF0, 0xC4, 0x04, 0x5C, 0xB3, 0x11, 0xE7, 0x90, 0x7B, 0xA6, 0x00, 0x6A, 0xD3, 0xDB, 0xA0]),
            Cell::DateTime("2023-12-31 23:00:00.1234".into()),
        ]
    );

    // A bad value fails the load and rolls its window back.
    run(&mut s, "DELETE FROM DBINE_XFER_DST").await;
    run(&mut s, "COMMIT").await;
    let mut bad = rows[0].clone();
    bad[1] = Cell::Int(100_000);
    let batch = RowBatch { rows: vec![rows[1].clone(), bad], bytes: 0 };
    let spec = load_spec("DBINE_XFER_DST", &got.columns, 100);
    let e = s.bulk_load(&spec, &got.columns, &mut Source(vec![batch].into()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("overflow") || e.to_string().contains("out of range"), "{e}");
    assert_eq!(run(&mut s, "SELECT COUNT(*) FROM DBINE_XFER_DST").await, vec![vec![serde_json::json!(0)]]);
}

#[tokio::test]
#[ignore]
async fn transfer_benchmark_100k_rows() {
    let mut s = session().await;
    drop_table(&mut s, "DBINE_XFER_BENCH").await;
    run(
        &mut s,
        "CREATE TABLE DBINE_XFER_BENCH (ID INTEGER NOT NULL, NAME VARCHAR(50), AMOUNT NUMERIC(18,4), RATIO DOUBLE PRECISION,
         CREATED TIMESTAMP, FLAG BOOLEAN, NOTE VARCHAR(40))",
    )
    .await;
    let n = 100_000i64;
    let rows: Vec<Vec<Cell>> = (0..n)
        .map(|i| {
            vec![
                Cell::Int(i),
                Cell::Text(format!("name-{i}")),
                Cell::Decimal(format!("{}.{:04}", i / 7, i % 10_000)),
                Cell::Float(i as f64 * 0.37),
                Cell::DateTime(format!("2020-01-{:02} {:02}:{:02}:{:02}.0000", i % 28 + 1, i % 24, i % 60, i % 60)),
                Cell::Bool(i % 2 == 0),
                if i % 5 == 0 { Cell::Null } else { Cell::Text("x".repeat((i % 40) as usize)) },
            ]
        })
        .collect();
    let batches: VecDeque<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    let spec = LoadSpec {
        columns: ["ID", "NAME", "AMOUNT", "RATIO", "CREATED", "FLAG", "NOTE"].iter().map(|s| s.to_string()).collect(),
        ..load_spec("DBINE_XFER_BENCH", &[], LoadSpec::DEFAULT_COMMIT_ROWS)
    };
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Source(batches), &|_| {}).await.expect("bulk_load");
    let load_s = t.elapsed().as_secs_f64();
    assert_eq!(loaded, n as u64);

    let t = Instant::now();
    let got = read(&mut s, ReadSpec { table: table("DBINE_XFER_BENCH"), columns: None, filter: None }).await;
    let read_s = t.elapsed().as_secs_f64();
    let mut back: Vec<Vec<Cell>> = got.batches.into_iter().flat_map(|b| b.rows).collect();
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(back.len(), rows.len());
    for (a, b) in rows.iter().zip(&back) {
        assert_eq!(a, b);
    }
    println!(
        "firebird 100k rows: bulk load {:.0} rows/s ({load_s:.2} s), read {:.0} rows/s ({read_s:.2} s)",
        n as f64 / load_s,
        n as f64 / read_s
    );
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> i64 {
    run(s, &format!("SELECT COUNT(*) FROM {t}")).await[0][0].as_i64().unwrap()
}

/// Gives its batches, then waits forever (a reader that stalls).
struct Stalls(VecDeque<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Stalls {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.0.pop_front() {
            Some(b) => Some(b),
            None => std::future::pending().await,
        }
    }
}

fn ints(from: i64, n: i64) -> RowBatch {
    RowBatch { rows: (from..from + n).map(|i| vec![Cell::Int(i), Cell::Text(format!("row {i}"))]).collect(), bytes: 0 }
}

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// A load dropped mid-batch (cancelled, or its source failed) commits
/// nothing afterwards, and leaves nothing open for the next load.
#[tokio::test]
#[ignore]
async fn transfer_dropped_load_commits_nothing_later() {
    let mut s = session().await;
    drop_table(&mut s, "DBINE_XFER_DROP").await;
    run(&mut s, "CREATE TABLE DBINE_XFER_DROP (ID INTEGER NOT NULL, V VARCHAR(40))").await;
    let spec = LoadSpec { columns: cols(&["ID", "V"]), ..load_spec("DBINE_XFER_DROP", &[], 2_000) };

    // Dropped while a step runs: its window closing inside the batch must not commit.
    let commits = Mutex::new(Vec::new());
    let r = tokio::time::timeout(
        Duration::from_millis(30),
        s.bulk_load(&spec, &[], &mut Source(vec![ints(0, 3_000)].into()), &|n| commits.lock().unwrap().push(n)),
    )
    .await;
    assert!(r.is_err(), "the load finished in 30 ms: nothing was cancelled");
    assert_eq!(count(&mut s, "DBINE_XFER_DROP").await, 0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count(&mut s, "DBINE_XFER_DROP").await, 0, "a commit landed after the load returned");
    assert!(commits.lock().unwrap().is_empty());

    // Dropped between batches, with a window open: rolled back before the
    // next load uses the connection (which then commits only its own rows).
    let r = tokio::time::timeout(Duration::from_millis(1_500), s.bulk_load(&spec, &[], &mut Stalls(vec![ints(0, 500)].into()), &|_| {})).await;
    assert!(r.is_err());
    let loaded = s.bulk_load(&spec, &[], &mut Source(vec![ints(1_000, 10)].into()), &|_| {}).await.unwrap();
    assert_eq!(loaded, 10);
    assert_eq!(count(&mut s, "DBINE_XFER_DROP").await, 10);
    drop_table(&mut s, "DBINE_XFER_DROP").await;
}

/// Rows committed before a failure in the same batch are reported, and a
/// failing block names its own rows.
#[tokio::test]
#[ignore]
async fn transfer_failures_report_committed_rows_and_the_right_rows() {
    let mut s = session().await;
    drop_table(&mut s, "DBINE_XFER_FAIL").await;
    run(&mut s, "CREATE TABLE DBINE_XFER_FAIL (ID INTEGER NOT NULL, V SMALLINT NOT NULL)").await;
    let spec = LoadSpec { columns: cols(&["ID", "V"]), ..load_spec("DBINE_XFER_FAIL", &[], 10) };
    let batch = |n: i64, bad: i64, cell: Cell| RowBatch {
        rows: (1..=n).map(|i| vec![Cell::Int(i), if i == bad { cell.clone() } else { Cell::Int(1) }]).collect(),
        bytes: 0,
    };

    let commits = Mutex::new(Vec::new());
    let e = s
        .bulk_load(&spec, &[], &mut Source(vec![batch(30, 26, Cell::Int(100_000))].into()), &|n| commits.lock().unwrap().push(n))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("fila 26"), "{e}");
    assert_eq!(*commits.lock().unwrap(), vec![10, 20]);
    assert_eq!(count(&mut s, "DBINE_XFER_FAIL").await, 20);

    // Blocks of 256 rows (two integers a row): row 501 is in the second.
    run(&mut s, "DELETE FROM DBINE_XFER_FAIL").await;
    run(&mut s, "COMMIT").await;
    let spec = LoadSpec { columns: cols(&["ID", "V"]), ..load_spec("DBINE_XFER_FAIL", &[], LoadSpec::DEFAULT_COMMIT_ROWS) };
    let e = s.bulk_load(&spec, &[], &mut Source(vec![batch(1_000, 501, Cell::Null)].into()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("filas 257–512"), "{e}");
    assert_eq!(count(&mut s, "DBINE_XFER_FAIL").await, 0);
    drop_table(&mut s, "DBINE_XFER_FAIL").await;
}

/// CHARACTER SET NONE keeps its bytes, and octets wider than a hex VARCHAR load.
#[tokio::test]
#[ignore]
async fn transfer_charset_none_and_wide_octets() {
    let mut s = session().await;
    let def = "(ID INTEGER NOT NULL, V VARCHAR(10) CHARACTER SET NONE, C CHAR(6) CHARACTER SET NONE,
        T BLOB SUB_TYPE TEXT CHARACTER SET NONE, W VARCHAR(20000) CHARACTER SET OCTETS,
        WN VARCHAR(20000) CHARACTER SET NONE)";
    for t in ["DBINE_XFER_NONE_SRC", "DBINE_XFER_NONE_DST"] {
        drop_table(&mut s, t).await;
        run(&mut s, &format!("CREATE TABLE {t} {def}")).await;
    }
    run(
        &mut s,
        "INSERT INTO DBINE_XFER_NONE_SRC VALUES (1, x'E96C6576', x'E96C', CAST(x'E96C6576' AS BLOB SUB_TYPE TEXT CHARACTER SET NONE),
         x'00FF01', LPAD(x'E9', 18000, x'E9'))",
    )
    .await;
    run(&mut s, "INSERT INTO DBINE_XFER_NONE_SRC (ID, V, C, T, W, WN) VALUES (2, 'plain', 'ab', 'text', LPAD(x'00', 20000, x'FF'), 'x')").await;
    run(&mut s, "COMMIT").await;

    let got = read(&mut s, ReadSpec { table: table("DBINE_XFER_NONE_SRC"), columns: None, filter: None }).await;
    let rows: Vec<Vec<Cell>> = got.batches.iter().flat_map(|b| b.rows.clone()).collect();
    let r = rows.iter().find(|r| r[0] == Cell::Int(1)).unwrap();
    assert_eq!(r[1], Cell::Bytes(vec![0xE9, 0x6C, 0x65, 0x76]));
    assert_eq!(r[2], Cell::Bytes(vec![0xE9, 0x6C, 0x20, 0x20, 0x20, 0x20]));
    assert_eq!(r[3], Cell::Bytes(vec![0xE9, 0x6C, 0x65, 0x76]));
    assert_eq!(r[4], Cell::Bytes(vec![0x00, 0xFF, 0x01]));
    let r2 = rows.iter().find(|r| r[0] == Cell::Int(2)).unwrap();
    assert_eq!(r2[1], Cell::Text("plain".into()));

    let spec = load_spec("DBINE_XFER_NONE_DST", &got.columns, 1_000);
    let n = s.bulk_load(&spec, &got.columns, &mut Source(got.batches.clone().into()), &|_| {}).await.expect("bulk_load");
    assert_eq!(n, 2);
    let q = "SELECT ID, HEX_ENCODE(V), HEX_ENCODE(C), HEX_ENCODE(CAST(T AS VARCHAR(100) CHARACTER SET NONE)), OCTET_LENGTH(W),
             HEX_ENCODE(CRYPT_HASH(W USING SHA256)), OCTET_LENGTH(WN), HEX_ENCODE(CRYPT_HASH(WN USING SHA256)) FROM {} ORDER BY ID";
    let src = run(&mut s, &q.replace("{}", "DBINE_XFER_NONE_SRC")).await;
    assert_eq!(src[0][1], Value::String("E96C6576".into()));
    assert_eq!(src, run(&mut s, &q.replace("{}", "DBINE_XFER_NONE_DST")).await);

    // The reported case: three bytes into VARCHAR(20000) OCTETS.
    run(&mut s, "DELETE FROM DBINE_XFER_NONE_DST").await;
    run(&mut s, "COMMIT").await;
    let spec = LoadSpec { columns: cols(&["ID", "W"]), ..load_spec("DBINE_XFER_NONE_DST", &[], 100) };
    let row = vec![Cell::Int(1), Cell::Bytes(vec![0, 255, 1])];
    s.bulk_load(&spec, &[], &mut Source(vec![RowBatch { rows: vec![row], bytes: 0 }].into()), &|_| {}).await.expect("wide octets");
    assert_eq!(run(&mut s, "SELECT HEX_ENCODE(W) FROM DBINE_XFER_NONE_DST").await, vec![vec![Value::String("00FF01".into())]]);
    for t in ["DBINE_XFER_NONE_SRC", "DBINE_XFER_NONE_DST"] {
        drop_table(&mut s, t).await;
    }
}

/// `keep_identity`: GENERATED ALWAYS takes the values (OVERRIDING SYSTEM
/// VALUE); without it Firebird generates them.
#[tokio::test]
#[ignore]
async fn transfer_keep_identity() {
    let mut s = session().await;
    drop_table(&mut s, "DBINE_XFER_IDENT").await;
    run(&mut s, "CREATE TABLE DBINE_XFER_IDENT (ID INT GENERATED ALWAYS AS IDENTITY, V VARCHAR(40))").await;
    let spec = LoadSpec { columns: cols(&["ID", "V"]), keep_identity: true, ..load_spec("DBINE_XFER_IDENT", &[], 100) };
    s.bulk_load(&spec, &[], &mut Source(vec![ints(100, 20)].into()), &|_| {}).await.expect("keep_identity");
    assert_eq!(run(&mut s, "SELECT MIN(ID), MAX(ID) FROM DBINE_XFER_IDENT").await, vec![vec![serde_json::json!(100), serde_json::json!(119)]]);

    run(&mut s, "DELETE FROM DBINE_XFER_IDENT").await;
    run(&mut s, "COMMIT").await;
    let spec = LoadSpec { keep_identity: false, ..spec };
    s.bulk_load(&spec, &[], &mut Source(vec![ints(100, 20)].into()), &|_| {}).await.expect("without keep_identity");
    let got = run(&mut s, "SELECT COUNT(*), MAX(ID) FROM DBINE_XFER_IDENT WHERE ID >= 100").await;
    assert_eq!(got[0][0], serde_json::json!(0), "the given values were written: {got:?}");
    assert_eq!(count(&mut s, "DBINE_XFER_IDENT").await, 20);
    drop_table(&mut s, "DBINE_XFER_IDENT").await;
}

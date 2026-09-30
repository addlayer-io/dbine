//! Bulk transfer against a real server:
//!
//! ```sh
//! docker start dbine-test-oracle   # -p 25601:1521, user dbine/Dbine123, FREEPDB1
//! cargo test --release -p dbine-driver-oracle -- --ignored transfer --nocapture
//! ```
//!
//! `DBINE_TEST_ORACLE_URL` overrides the server (default
//! `oracle://dbine:Dbine123@localhost:25601/FREEPDB1`).

use dbine_driver::async_trait;
use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").unwrap_or_else(|_| "oracle://dbine:Dbine123@localhost:25601/FREEPDB1".into());
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn session() -> Box<dyn Session> {
    let driver = dbine_driver_oracle::drivers().remove(0);
    assert!(driver.supports_bulk_load());
    driver.connect(&config(), None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1000, &mut out).await {
        panic!("{sql}: {e}");
    }
    out
}

async fn drop_table(s: &mut Box<dyn Session>, name: &str) {
    let _ = s.execute(&format!("DROP TABLE {name} PURGE"), 1, &mut QueryOutcome::default()).await;
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
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.batches.push(batch);
        Ok(())
    }
}

struct Batches(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read(s: &mut Box<dyn Session>, name: &str) -> (Vec<TransferColumn>, Vec<RowBatch>, u64) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&ReadSpec { table: table(name), columns: None, filter: None }, sink.clone()).await.expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    (c.columns, c.batches, n)
}

fn load_spec(name: &str, columns: &[TransferColumn], table_lock: bool, keep_identity: bool) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock,
        keep_identity,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

async fn load(s: &mut Box<dyn Session>, spec: &LoadSpec, columns: &[TransferColumn], batches: Vec<RowBatch>) -> (u64, Vec<u64>) {
    let mut source = Batches(batches.into());
    let seen = Mutex::new(Vec::new());
    let n = s.bulk_load(spec, columns, &mut source, &|n| seen.lock().unwrap().push(n)).await.expect("bulk_load");
    (n, seen.into_inner().unwrap())
}

/// Rows ordered by their first cell (an integer id).
fn sorted(batches: &[RowBatch]) -> Vec<Vec<Cell>> {
    let mut rows: Vec<Vec<Cell>> = batches.iter().flat_map(|b| b.rows.clone()).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => i64::MAX,
    });
    rows
}

const TYPES: &str = "(
  id NUMBER(10) PRIMARY KEY,
  n NUMBER(38,10),
  big NUMBER,
  huge NUMBER(38),
  bf BINARY_FLOAT,
  bd BINARY_DOUBLE,
  vc VARCHAR2(100),
  nv NVARCHAR2(50),
  ch CHAR(5),
  cl CLOB,
  ncl NCLOB,
  rw RAW(16),
  bl BLOB,
  d DATE,
  ts TIMESTAMP(9),
  tstz TIMESTAMP(6) WITH TIME ZONE,
  tsltz TIMESTAMP(6) WITH LOCAL TIME ZONE,
  ids INTERVAL DAY(3) TO SECOND(6),
  iym INTERVAL YEAR(3) TO MONTH,
  js JSON,
  flag BOOLEAN
)";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_all_types_round_trip() {
    let mut s = session().await;
    for t in ["dbine_tr_src", "dbine_tr_dst", "dbine_tr_dst2"] {
        drop_table(&mut s, t).await;
        run(&mut s, &format!("CREATE TABLE {t} {TYPES}")).await;
    }
    run(
        &mut s,
        "INSERT INTO dbine_tr_src VALUES (1, 12345678901234567890.0123456789, 3.14159265358979323846264338327950288,
           99999999999999999999999999999999999999, 1.5, 2.718281828459045, 'hola ñandú 😀', N'ünï', 'ab',
           'texto', N'ñ', HEXTORAW('00FF10'), HEXTORAW('DEADBEEF'),
           TO_DATE('2024-02-29 23:59:58', 'YYYY-MM-DD HH24:MI:SS'),
           TO_TIMESTAMP('2024-01-31 13:45:07.123456789', 'YYYY-MM-DD HH24:MI:SS.FF9'),
           TO_TIMESTAMP_TZ('2024-01-31 13:45:07.5 -03:00', 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'),
           TO_TIMESTAMP_TZ('2024-06-01 10:00:00 +02:00', 'YYYY-MM-DD HH24:MI:SS TZH:TZM'),
           INTERVAL '-5 04:03:02.5' DAY TO SECOND, INTERVAL '2-7' YEAR TO MONTH,
           JSON('{\"a\":[1,2.5,\"x\"],\"b\":{\"c\":null,\"d\":true}}'), TRUE)",
    )
    .await;
    run(&mut s, "INSERT INTO dbine_tr_src (id) VALUES (2)").await;
    run(&mut s, "INSERT INTO dbine_tr_src (id, n, big, bf, bd, flag) VALUES (3, -0.5, -12, -1e30, 1e-300, FALSE)").await;
    // Large LOBs: a ~5 MB BLOB, a 100k-character CLOB with multibyte
    // characters, and a 20k-character one (bound inline).
    run(
        &mut s,
        "DECLARE b BLOB; c CLOB; m CLOB;
         BEGIN
           DBMS_LOB.CREATETEMPORARY(b, TRUE); DBMS_LOB.CREATETEMPORARY(c, TRUE); DBMS_LOB.CREATETEMPORARY(m, TRUE);
           FOR i IN 1..164 LOOP
             DBMS_LOB.WRITEAPPEND(b, 32000, UTL_RAW.CAST_TO_RAW(RPAD(TO_CHAR(i), 32000, CHR(65 + MOD(i, 26)))));
           END LOOP;
           FOR i IN 1..10 LOOP
             DBMS_LOB.WRITEAPPEND(c, 10000, RPAD('ñ' || i, 10000, 'é'));
           END LOOP;
           DBMS_LOB.WRITEAPPEND(m, 20000, RPAD('m', 20000, 'x'));
           INSERT INTO dbine_tr_src (id, bl, cl, ncl) VALUES (4, b, c, m);
           INSERT INTO dbine_tr_src (id, cl) VALUES (5, m);
         END;",
    )
    .await;
    // Many fetch round trips with JSON and mid-sized CLOBs (bound inline).
    run(
        &mut s,
        "INSERT INTO dbine_tr_src (id, n, vc, js, tstz, cl, rw)
         SELECT 9 + level, level / 3, 'v' || level, JSON('{\"k\":' || level || ',\"s\":\"x\"}'), SYSTIMESTAMP,
                RPAD('c', MOD(level, 5000) + 1, 'c'), HEXTORAW(LPAD(TO_CHAR(level, 'FMXXXX'), 8, '0'))
         FROM dual CONNECT BY level <= 3000",
    )
    .await;
    run(&mut s, "COMMIT").await;

    let t0 = Instant::now();
    let (cols, batches, n) = read(&mut s, "DBINE_TR_SRC").await;
    println!("read {n} rows in {:?}", t0.elapsed());
    assert_eq!(n, 3005);
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "ID");
    let src = sorted(&batches);
    let r1 = &src[0];
    let at = |name: &str| names.iter().position(|n| *n == name).unwrap();
    assert_eq!(r1[at("ID")], Cell::Int(1));
    assert_eq!(r1[at("N")], Cell::Decimal("12345678901234567890.0123456789".into()));
    assert_eq!(r1[at("BIG")], Cell::Decimal("3.14159265358979323846264338327950288".into()));
    assert_eq!(r1[at("HUGE")], Cell::Decimal("99999999999999999999999999999999999999".into()));
    assert_eq!(r1[at("BF")], Cell::Float(1.5));
    assert_eq!(r1[at("BD")], Cell::Float(std::f64::consts::E));
    assert_eq!(r1[at("VC")], Cell::Text("hola ñandú 😀".into()));
    assert_eq!(r1[at("CH")], Cell::Text("ab   ".into()));
    assert_eq!(r1[at("RW")], Cell::Bytes(vec![0, 0xFF, 0x10]));
    assert_eq!(r1[at("D")], Cell::DateTime("2024-02-29 23:59:58".into()));
    assert_eq!(r1[at("TS")], Cell::DateTime("2024-01-31 13:45:07.123456789".into()));
    assert_eq!(r1[at("TSTZ")], Cell::DateTimeTz("2024-01-31 13:45:07.5-03:00".into()));
    assert_eq!(r1[at("TSLTZ")], Cell::DateTimeTz("2024-06-01 08:00:00+00:00".into()));
    assert_eq!(r1[at("IDS")], Cell::Text("-5 04:03:02.500000000".into()));
    assert_eq!(r1[at("IYM")], Cell::Text("+2-7".into()));
    assert!(matches!(&r1[at("JS")], Cell::Json(j) if j.contains("\"a\":[1,2.5,\"x\"]")), "{:?}", r1[at("JS")]);
    assert_eq!(r1[at("FLAG")], Cell::Bool(true));
    assert!(src[1][1..].iter().all(|c| *c == Cell::Null), "{:?}", src[1]);
    assert_eq!(src[2][at("BF")], Cell::Float(-1e30f32 as f64));
    match &src[3][at("BL")] {
        Cell::Bytes(b) => assert_eq!(b.len(), 164 * 32000),
        other => panic!("{other:?}"),
    }
    match &src[3][at("CL")] {
        Cell::Text(t) => assert_eq!(t.chars().count(), 100_000),
        other => panic!("{other:?}"),
    }

    for (dst, direct) in [("DBINE_TR_DST", false), ("DBINE_TR_DST2", true)] {
        let t0 = Instant::now();
        let (loaded, progress) = load(&mut s, &load_spec(dst, &cols, direct, true), &cols, batches.clone()).await;
        println!("{dst}: loaded {loaded} rows in {:?} (direct path: {direct})", t0.elapsed());
        assert_eq!(loaded, 3005);
        assert_eq!(progress.last(), Some(&3005));
        let (_, back, _) = read(&mut s, dst).await;
        let back = sorted(&back);
        for (a, b) in src.iter().zip(&back) {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert!(x == y, "{dst} row {:?} column {}: {} vs {}", a[0], names[i], short(x), short(y));
            }
        }
    }
}

fn short(c: &Cell) -> String {
    let s = format!("{c:?}");
    if s.len() > 200 {
        format!("{}… ({} bytes)", &s[..200], s.len())
    } else {
        s
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_identity_columns() {
    let mut s = session().await;
    for t in ["dbine_tr_always", "dbine_tr_default"] {
        drop_table(&mut s, t).await;
    }
    run(&mut s, "CREATE TABLE dbine_tr_always (id NUMBER GENERATED ALWAYS AS IDENTITY, v VARCHAR2(10))").await;
    run(&mut s, "CREATE TABLE dbine_tr_default (id NUMBER GENERATED BY DEFAULT AS IDENTITY, v VARCHAR2(10))").await;
    let cols = vec![
        TransferColumn { name: "ID".into(), type_name: "NUMBER".into(), nullable: false },
        TransferColumn { name: "V".into(), type_name: "VARCHAR2(10)".into(), nullable: true },
    ];
    let batch = || vec![RowBatch { rows: vec![vec![Cell::Int(100), Cell::Text("a".into())], vec![Cell::Int(200), Cell::Null]], bytes: 0 }];

    // ALWAYS + keep_identity: refused, the table is not altered.
    let mut source = Batches(batch().into());
    let e = s.bulk_load(&load_spec("DBINE_TR_ALWAYS", &cols, false, true), &cols, &mut source, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("GENERATED ALWAYS"), "{e}");
    // Without keep_identity the column is generated.
    let (n, _) = load(&mut s, &load_spec("DBINE_TR_ALWAYS", &cols, false, false), &cols, batch()).await;
    assert_eq!(n, 2);
    let out = run(&mut s, "SELECT COUNT(*) FROM dbine_tr_always WHERE id < 100").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(2));

    // BY DEFAULT keeps the given values.
    let (n, _) = load(&mut s, &load_spec("DBINE_TR_DEFAULT", &cols, true, true), &cols, batch()).await;
    assert_eq!(n, 2);
    let out = run(&mut s, "SELECT SUM(id) FROM dbine_tr_default").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(300));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_benchmark_200k() {
    const ROWS: u64 = 200_000;
    let mut s = session().await;
    for t in ["dbine_tr_bench", "dbine_tr_bench_a", "dbine_tr_bench_b"] {
        drop_table(&mut s, t).await;
    }
    let ddl = "(id NUMBER(10) PRIMARY KEY, amount NUMBER(12,2), name VARCHAR2(50), d DATE, ts TIMESTAMP(6), x BINARY_DOUBLE, note VARCHAR2(200))";
    for t in ["dbine_tr_bench", "dbine_tr_bench_a", "dbine_tr_bench_b"] {
        run(&mut s, &format!("CREATE TABLE {t} {ddl}")).await;
    }
    run(
        &mut s,
        &format!(
            "INSERT /*+ APPEND */ INTO dbine_tr_bench
             SELECT level, ROUND(DBMS_RANDOM.VALUE(-1e6, 1e6), 2), 'nombre ' || level, DATE '2020-01-01' + MOD(level, 2000),
                    SYSTIMESTAMP - NUMTODSINTERVAL(level, 'SECOND'), level / 7,
                    CASE WHEN MOD(level, 10) = 0 THEN NULL ELSE RPAD('n', MOD(level, 150), 'z') END
             FROM dual CONNECT BY level <= {ROWS}"
        ),
    )
    .await;
    run(&mut s, "COMMIT").await;

    let t0 = Instant::now();
    let (cols, batches, n) = read(&mut s, "DBINE_TR_BENCH").await;
    let secs = t0.elapsed().as_secs_f64();
    assert_eq!(n, ROWS);
    println!("read: {ROWS} rows in {secs:.2} s = {:.0} rows/s", ROWS as f64 / secs);

    for (dst, direct) in [("DBINE_TR_BENCH_A", false), ("DBINE_TR_BENCH_B", true)] {
        let t0 = Instant::now();
        let (loaded, progress) = load(&mut s, &load_spec(dst, &cols, direct, false), &cols, batches.clone()).await;
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(loaded, ROWS);
        assert_eq!(progress.last(), Some(&ROWS));
        println!("load {dst} (direct path: {direct}): {ROWS} rows in {secs:.2} s = {:.0} rows/s", ROWS as f64 / secs);
        let out = run(
            &mut s,
            &format!("SELECT COUNT(*) FROM (SELECT * FROM dbine_tr_bench MINUS SELECT * FROM {dst})"),
        )
        .await;
        assert_eq!(out.results[0].rows[0][0], serde_json::json!(0), "{dst} differs");
    }
    for t in ["dbine_tr_bench", "dbine_tr_bench_a", "dbine_tr_bench_b"] {
        drop_table(&mut s, t).await;
    }
}

async fn count(s: &mut Box<dyn Session>, table: &str) -> i64 {
    let out = run(s, &format!("SELECT COUNT(*) FROM {table}")).await;
    out.results[0].rows[0][0].as_i64().unwrap()
}

/// Gives its batches, then either ends or never answers again.
struct ThenHang(VecDeque<RowBatch>, bool);

#[async_trait]
impl BatchSource for ThenHang {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.0.pop_front() {
            Some(b) => Some(b),
            None if self.1 => std::future::pending().await,
            None => None,
        }
    }
}

fn int_rows(from: i64, n: i64) -> RowBatch {
    RowBatch { rows: (from..from + n).map(|i| vec![Cell::Int(i), Cell::Text(format!("fila {i}"))]).collect(), bytes: 0 }
}

/// A load dropped midway (a cancel, or the orchestrator's failed read)
/// commits nothing after the drop, whether the source hangs or had ended.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dropped_load_commits_nothing_later() {
    let mut s = session().await;
    let mut other = session().await;
    let cols = vec![
        TransferColumn { name: "ID".into(), type_name: "NUMBER".into(), nullable: false },
        TransferColumn { name: "V".into(), type_name: "VARCHAR2(20)".into(), nullable: true },
    ];
    for (name, hang, wait_ms) in [("DBINE_TR_DROP_A", true, 1), ("DBINE_TR_DROP_A2", true, 300), ("DBINE_TR_DROP_B", false, 1), ("DBINE_TR_DROP_B2", false, 30)] {
        drop_table(&mut s, name).await;
        run(&mut s, &format!("CREATE TABLE {name} (id NUMBER PRIMARY KEY, v VARCHAR2(20))")).await;
        let batches: VecDeque<RowBatch> = (0..10).map(|i| int_rows(i * 600, 600)).collect();
        let mut source = ThenHang(batches, hang);
        let mut spec = load_spec(name, &cols, false, false);
        spec.commit_rows = if hang { 600 } else { 100_000 };
        let seen = Mutex::new(Vec::new());
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(wait_ms),
            s.bulk_load(&spec, &cols, &mut source, &|n| seen.lock().unwrap().push(n)),
        )
        .await;
        if r.is_ok() {
            // Finished before the timeout: nothing to check for this case.
            println!("{name}: finished before being dropped");
            continue;
        }
        let right_after = count(&mut other, name).await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let later = count(&mut other, name).await;
        println!("{name}: {right_after} rows right after the drop, {later} 3 s later, progress {:?}", seen.lock().unwrap());
        assert_eq!(right_after, later, "{name}: rows committed after the drop");
        // The session is usable again (the worker rolled back and let go).
        assert_eq!(count(&mut s, name).await, later);
        drop_table(&mut s, name).await;
    }
}

/// Values that used to be lost or refused on the way in.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edge_values() {
    let mut s = session().await;
    let name = "DBINE_TR_EDGE";
    drop_table(&mut s, name).await;
    run(&mut s, &format!("CREATE TABLE {name} (id NUMBER PRIMARY KEY, cl CLOB, ncl NCLOB, bl BLOB, rw RAW(16), oid RAW(12), vc VARCHAR2(10), n NUMBER)")).await;
    let emoji = "😀".repeat(9000); // 36000 bytes, 18000 UTF-16 code units
    let mixed = format!("{}{}", "a".repeat(32000), "😀".repeat(100));
    let cols: Vec<TransferColumn> = [("ID", "NUMBER"), ("CL", "text"), ("NCL", "text"), ("BL", "bytea"), ("RW", "varchar(10)"), ("OID", "objectId"), ("VC", "text"), ("N", "numeric")]
        .iter()
        .map(|(n, t)| TransferColumn { name: n.to_string(), type_name: t.to_string(), nullable: true })
        .collect();
    let row = |id: i64, cl: Cell, ncl: Cell, bl: Cell| vec![id.into_cell(), cl, ncl, bl, Cell::Text("cafe".into()), Cell::Text("65a1b2c3d4e5f60718293a4b".into()), Cell::Text("x".into()), Cell::Int(590400)];
    let batch = RowBatch {
        rows: vec![
            row(1, Cell::Text(emoji.clone()), Cell::Text(emoji.clone()), Cell::Bytes(vec![7; 40_000])),
            row(2, Cell::Text(mixed.clone()), Cell::Text(String::new()), Cell::Bytes(vec![])),
            row(3, Cell::Text(String::new()), Cell::Null, Cell::Null),
        ],
        bytes: 0,
    };
    let (n, _) = load(&mut s, &load_spec(name, &cols, false, false), &cols, vec![batch]).await;
    assert_eq!(n, 3);
    let out = run(
        &mut s,
        &format!(
            "SELECT id, DBMS_LOB.GETLENGTH(cl), DBMS_LOB.GETLENGTH(ncl), DBMS_LOB.GETLENGTH(bl), RAWTOHEX(rw), RAWTOHEX(oid), n,
                    CASE WHEN ncl IS NULL THEN 'null' ELSE 'lob' END, CASE WHEN bl IS NULL THEN 'null' ELSE 'lob' END
             FROM {name} ORDER BY id"
        ),
    )
    .await;
    let rows = &out.results[0].rows;
    use serde_json::json;
    assert_eq!(rows[0][1], json!(18000), "emoji CLOB length in UTF-16 units");
    assert_eq!(rows[0][2], json!(18000));
    assert_eq!(rows[0][3], json!(40000));
    assert_eq!(rows[0][4], json!("63616665"), "text into RAW is its bytes");
    assert_eq!(rows[0][5], json!("65A1B2C3D4E5F60718293A4B"), "an ObjectId into RAW(12)");
    assert_eq!(rows[0][6], json!(590400));
    assert_eq!(rows[1][2], json!(0), "an empty NCLOB is kept (not NULL)");
    assert_eq!(rows[1][7], json!("lob"));
    assert_eq!(rows[1][8], json!("lob"));
    assert_eq!(rows[2][1], json!(0));
    // Read back whole.
    let (_, back, _) = read(&mut s, name).await;
    let back = sorted(&back);
    assert_eq!(back[0][1], Cell::Text(emoji.clone()));
    assert_eq!(back[0][2], Cell::Text(emoji));
    assert_eq!(back[1][1], Cell::Text(mixed));
    assert_eq!(back[1][2], Cell::Text(String::new()));
    assert_eq!(back[1][3], Cell::Bytes(vec![]));

    // An empty VARCHAR2 would be NULL: refused, and nothing stays.
    run(&mut s, &format!("DELETE FROM {name}")).await;
    run(&mut s, "COMMIT").await;
    let mut source = Batches(vec![RowBatch { rows: vec![row(9, Cell::Null, Cell::Null, Cell::Null), { let mut r = row(10, Cell::Null, Cell::Null, Cell::Null); r[6] = Cell::Text(String::new()); r }], bytes: 0 }].into());
    let e = s.bulk_load(&load_spec(name, &cols, false, false), &cols, &mut source, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, dbine_driver::Error::Unsupported(m) if m.contains("VC")), "{e}");
    assert_eq!(count(&mut s, name).await, 0);

    // A huge exponent: an error for that value, the session still fine.
    let mut source = Batches(vec![RowBatch { rows: vec![{ let mut r = row(11, Cell::Null, Cell::Null, Cell::Null); r[7] = Cell::Text("1e2147483647".into()); r }], bytes: 0 }].into());
    let e = s.bulk_load(&load_spec(name, &cols, false, false), &cols, &mut source, &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("Columna N"), "{e}");
    let out = run(&mut s, "SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_NUMERIC_CHARACTERS'").await;
    assert!(out.results[0].rows[0][0].is_string());
    assert_eq!(count(&mut s, name).await, 0);
    drop_table(&mut s, name).await;
}

trait IntoCell {
    fn into_cell(self) -> Cell;
}

impl IntoCell for i64 {
    fn into_cell(self) -> Cell {
        Cell::Int(self)
    }
}

/// BC dates, time zone regions and JSON: read → load keeps BC dates and
/// exact JSON numbers; the native copy also keeps regions and extended
/// JSON scalars.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dates_zones_and_json() {
    let driver = dbine_driver_oracle::drivers().remove(0);
    assert!(driver.supports_native_copy("oracle"));
    let mut s = session().await;
    let mut t = session().await;
    let ddl = "(id NUMBER PRIMARY KEY, d DATE, ts TIMESTAMP(9), tz TIMESTAMP(9) WITH TIME ZONE, js JSON)";
    for name in ["dbine_tr_zsrc", "dbine_tr_zdst", "dbine_tr_zdst2"] {
        drop_table(&mut s, name).await;
        run(&mut s, &format!("CREATE TABLE {name} {ddl}")).await;
    }
    run(
        &mut s,
        "INSERT INTO dbine_tr_zsrc VALUES (1, TO_DATE('-0044-03-15 12:00:00', 'SYYYY-MM-DD HH24:MI:SS'),
           TIMESTAMP '-0044-03-15 12:00:00.5', TIMESTAMP '-0044-03-15 12:00:00 +01:00',
           JSON('{\"big\":12345678901234567890123.456789,\"t\":{\"$oracleTimestamp\":\"2024-01-31T13:45:07\"},\"r\":{\"$binary\":\"3q2+7w==\"}}' EXTENDED))",
    )
    .await;
    run(
        &mut s,
        "INSERT INTO dbine_tr_zsrc (id, tz) VALUES (2, TIMESTAMP '2024-07-01 10:00:00.123456789 America/Argentina/Buenos_Aires')",
    )
    .await;
    run(&mut s, "INSERT INTO dbine_tr_zsrc (id, tz) VALUES (3, TIMESTAMP '2024-11-03 01:30:00 America/New_York EST')").await;
    run(&mut s, "COMMIT").await;

    let (cols, batches, _) = read(&mut s, "DBINE_TR_ZSRC").await;
    let src = sorted(&batches);
    assert_eq!(src[0][1], Cell::Text("0044-03-15 12:00:00 BC".into()));
    assert_eq!(src[0][2], Cell::Text("0044-03-15 12:00:00.5 BC".into()));
    assert_eq!(src[0][3], Cell::Text("0044-03-15 12:00:00+01:00 BC".into()));
    assert!(matches!(&src[0][4], Cell::Json(j) if j.contains("12345678901234567890123.456789")), "{:?}", src[0][4]);
    assert_eq!(src[1][3], Cell::DateTimeTz("2024-07-01 10:00:00.123456789-03:00".into()));

    // Read → load (what another engine's source would give): BC dates and
    // the JSON's digits survive; regions become offsets (same instant).
    load(&mut s, &load_spec("DBINE_TR_ZDST", &cols, false, false), &cols, batches).await;
    let out = run(
        &mut s,
        "SELECT COUNT(*) FROM dbine_tr_zsrc a JOIN dbine_tr_zdst b ON a.id = b.id
         WHERE DECODE(a.d, b.d, 1, 0) = 1 AND DECODE(a.ts, b.ts, 1, 0) = 1 AND DECODE(a.tz, b.tz, 1, 0) = 1",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3), "same dates and instants");
    let out = run(&mut s, "SELECT JSON_SERIALIZE(js RETURNING VARCHAR2(4000)) FROM dbine_tr_zdst WHERE id = 1").await;
    assert!(out.results[0].rows[0][0].as_str().unwrap().contains("12345678901234567890123.456789"), "{:?}", out.results[0].rows[0][0]);

    // Native copy: regions and extended scalars too.
    let spec = dbine_driver::transfer::CopySpec {
        source: ReadSpec { table: table("DBINE_TR_ZSRC"), columns: None, filter: None },
        target: load_spec("DBINE_TR_ZDST2", &cols, false, false),
    };
    let seen = Mutex::new(Vec::new());
    let n = driver.copy_native(&mut *s, &mut *t, &spec, &|n| seen.lock().unwrap().push(n)).await.expect("copy_native");
    assert_eq!(n, 3);
    assert_eq!(seen.into_inner().unwrap().last(), Some(&3));
    let out = run(
        &mut s,
        "SELECT COUNT(*) FROM dbine_tr_zsrc a JOIN dbine_tr_zdst2 b ON a.id = b.id
         WHERE DECODE(a.d, b.d, 1, 0) = 1 AND DECODE(a.ts, b.ts, 1, 0) = 1
           AND DECODE(TO_CHAR(a.tz, 'SYYYY-MM-DD HH24:MI:SS.FF9 TZR TZD'), TO_CHAR(b.tz, 'SYYYY-MM-DD HH24:MI:SS.FF9 TZR TZD'), 1, 0) = 1",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3), "same regions");
    let out = run(
        &mut s,
        "SELECT JSON_VALUE(js, '$.t.type()'), JSON_VALUE(js, '$.r.type()'), JSON_SERIALIZE(js RETURNING VARCHAR2(4000) EXTENDED)
         FROM dbine_tr_zdst2 WHERE id = 1",
    )
    .await;
    let r = &out.results[0].rows[0];
    assert_eq!(r[0], serde_json::json!("timestamp"));
    assert_eq!(r[1], serde_json::json!("binary"));
    assert!(r[2].as_str().unwrap().contains("12345678901234567890123.456789"), "{r:?}");
    for name in ["dbine_tr_zsrc", "dbine_tr_zdst", "dbine_tr_zdst2"] {
        drop_table(&mut s, name).await;
    }
}

/// The grid shows WITH TIME ZONE values with their own local clock, and a
/// value the client can't decode (a region) is an error, not a dead session.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_grid_time_zones() {
    let mut s = session().await;
    let out = run(
        &mut s,
        "SELECT TO_TIMESTAMP_TZ('2024-01-31 13:45:07.25 -03:00', 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'),
                TO_TIMESTAMP_TZ('2024-01-01 01:00:00 +05:30', 'YYYY-MM-DD HH24:MI:SS TZH:TZM'),
                CAST(TO_TIMESTAMP_TZ('2024-01-31 13:45:07 -03:00', 'YYYY-MM-DD HH24:MI:SS TZH:TZM') AS TIMESTAMP WITH LOCAL TIME ZONE),
                DBTIMEZONE
         FROM dual",
    )
    .await;
    let r = &out.results[0].rows[0];
    assert_eq!(r[0], serde_json::json!("2024-01-31 13:45:07.25 -03:00"));
    assert_eq!(r[1], serde_json::json!("2024-01-01 01:00:00 +05:30"));
    // WITH LOCAL TIME ZONE: the instant, in the database's time zone.
    if r[3] == serde_json::json!("+00:00") {
        assert_eq!(r[2], serde_json::json!("2024-01-31 16:45:07 +00:00"));
    }
    let mut out = QueryOutcome::default();
    let e = s
        .execute("SELECT TIMESTAMP '2024-07-01 10:00:00 Europe/Madrid' FROM dual", 10, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("TZR"), "{e}");
    let out = run(&mut s, "SELECT 1 FROM dual").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(1));
}

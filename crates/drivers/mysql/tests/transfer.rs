//! Bulk transfer against real servers (the `dbine-test-*` containers):
//!
//! ```sh
//! docker start dbine-test-mysql dbine-test-mariadb dbine-test-tidb dbine-test-starrocks \
//!   dbine-test-greptimedb dbine-test-manticore
//! cargo test --release -p dbine-driver-mysql --test transfer -- --ignored transfer --nocapture
//! ```
//!
//! Each engine reads `DBINE_TEST_<ENGINE>_URL` (`mysql://user:pass@host:port`),
//! defaulting to the container's port; a server that doesn't answer is
//! skipped. The MySQL test turns `local_infile` on for its second run (to
//! try both load paths) and restores it.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DB: &str = "dbine_transfer";

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// A session on `db` (after creating it), or `None` when the server is down.
async fn connect(id: &str, env: &str, default: &str, db: Option<&str>) -> Option<Box<dyn Session>> {
    let url = std::env::var(env).unwrap_or_else(|_| default.into());
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = match tokio::time::timeout(Duration::from_secs(10), d.connect(&cfg, None)).await {
        Ok(Ok(s)) => s,
        other => {
            eprintln!("{id}: {url} not reachable ({:?}); skipping", other.err().map(|_| "timeout"));
            return None;
        }
    };
    let Some(db) = db else { return Some(admin) };
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {db}")).await;
    Some(d.connect(&cfg, Some(db)).await.expect("connect to the test database"))
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error {
        panic!("{sql}: {e}");
    }
    out.results.into_iter().next().map(|r| r.rows).unwrap_or_default()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
    rows: u64,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.rows += b.len() as u64;
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
    assert_eq!(n, c.rows);
    c
}

async fn read_all(s: &mut Box<dyn Session>, name: &str) -> Collect {
    read(s, ReadSpec { table: table(name), columns: None, filter: None }).await
}

fn rows(c: &Collect) -> Vec<Vec<Cell>> {
    c.batches.iter().flat_map(|b| b.rows.clone()).collect()
}

/// Load `data` into `name`; returns the rows and the progress calls.
async fn load(s: &mut Box<dyn Session>, name: &str, data: &Collect, commit_rows: u64) -> (u64, Vec<u64>) {
    let spec = LoadSpec {
        table: table(name),
        columns: data.columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: true,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let calls = Mutex::new(Vec::new());
    let progress = |n: u64| calls.lock().unwrap().push(n);
    let mut source = Source(data.batches.iter().cloned().collect());
    let n = s.bulk_load(&spec, &data.columns, &mut source, &progress).await.unwrap_or_else(|e| panic!("bulk_load {name}: {e}"));
    (n, calls.into_inner().unwrap())
}

/// Rows in the order of their first column (a table's read has no order).
fn by_id(mut rows: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i as i128,
        Cell::UInt(u) => u as i128,
        _ => 0,
    });
    rows
}

fn assert_same(a: &[Vec<Cell>], b: &[Vec<Cell>]) {
    assert_eq!(a.len(), b.len(), "row count");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        for (j, (p, q)) in x.iter().zip(y).enumerate() {
            let short = |c: &Cell| match c {
                Cell::Bytes(b) if b.len() > 64 => format!("Bytes(len {})", b.len()),
                Cell::Text(t) if t.len() > 64 => format!("Text(len {})", t.len()),
                c => format!("{c:?}"),
            };
            assert!(p == q, "row {i} column {j}: {} != {}", short(p), short(q));
        }
    }
}

// ------------------------------------------------------------ MySQL family

const MYSQL_TYPES: &str = "(
    id INT PRIMARY KEY AUTO_INCREMENT, ti TINYINT, si SMALLINT UNSIGNED, i INT, bi BIGINT, ubi BIGINT UNSIGNED,
    de DECIMAL(38,10), f FLOAT, d DOUBLE, c CHAR(5), vc VARCHAR(100), t LONGTEXT, bn BINARY(4), vb VARBINARY(100),
    bl LONGBLOB, dt DATE, tm TIME(6), dtm DATETIME(6), ts TIMESTAMP(6) NULL, y YEAR, j JSON, b BIT(10),
    e ENUM('a','b c'), st SET('x','y'), u BINARY(16), g GEOMETRY
)";

fn mysql_types(geometry: bool) -> String {
    if geometry {
        MYSQL_TYPES.to_string()
    } else {
        MYSQL_TYPES.replace(", g GEOMETRY", "")
    }
}

async fn fill_mysql(s: &mut Box<dyn Session>, geometry: bool) {
    run(s, "DROP TABLE IF EXISTS src").await;
    run(s, &format!("CREATE TABLE src {}", mysql_types(geometry))).await;
    let g = |v: &str| if geometry { format!(", {v}") } else { String::new() };
    // id 0 needs NO_AUTO_VALUE_ON_ZERO here, and keep_identity on the load.
    run(s, "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_AUTO_VALUE_ON_ZERO'), time_zone = '+00:00'").await;
    run(
        s,
        &format!(
            "INSERT INTO src VALUES (0, -128, 65535, -2147483648, -9223372036854775808, 18446744073709551615,
            '-1234567890123456789012345678.0123456789', 0.1, 1e300, 'ab', 'tab\\there\\nnew \\\\ back ñ 🦀', '\\\\N',
            X'00FF0A09', X'5C00', X'0001', '2024-02-29', '-838:59:59.000000', '9999-12-31 23:59:59.999999',
            '2038-01-19 03:14:07.999999', 2155, '{{\"a\": [1, \"x\"]}}', b'1010101010', 'b c', 'x,y',
            UNHEX('0F8FAD5BD9CB469FA16570867728950E'){}),
            (7, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL{}),
            (8, 0, 0, 0, 0, 0, '0', -0.5, 2.2250738585072014e-308, '', '', '', X'00000000', X'', X'', '1000-01-01',
             '25:00:00.5', '1000-01-01 00:00:00', '1970-01-01 00:00:01', 1901, '[]', b'0', 'a', '', X'00000000000000000000000000000000'{})",
            g("ST_GeomFromText('POINT(1 2)')"),
            g("NULL"),
            g("ST_GeomFromText('LINESTRING(0 0, 1 1, 2 5)')"),
        )
    )
    .await;
    // 5 MB of every awkward byte (backslash, newline, tab, CR, NUL, 0xFF).
    run(s, "INSERT INTO src (id, bl, t) VALUES (9, UNHEX(REPEAT('00FF5C0A090D', 873814)), REPEAT('ñ\\\\\\t', 100000))").await;
}

async fn round_trip(s: &mut Box<dyn Session>, label: &str, geometry: bool) {
    let src = read_all(s, "src").await;
    assert_eq!(src.rows, 4);
    let r = by_id(rows(&src));
    // Typed cells.
    assert_eq!(r[0][4], Cell::Int(i64::MIN));
    assert_eq!(r[0][5], Cell::UInt(u64::MAX));
    assert_eq!(r[0][6], Cell::Decimal("-1234567890123456789012345678.0123456789".into()));
    assert_eq!(r[0][7], Cell::Float(0.1));
    assert_eq!(r[0][8], Cell::Float(1e300));
    assert_eq!(r[0][10], Cell::Text("tab\there\nnew \\ back ñ 🦀".into()));
    assert_eq!(r[0][11], Cell::Text("\\N".into()));
    assert_eq!(r[0][12], Cell::Bytes(vec![0, 0xff, 0x0a, 0x09]));
    assert_eq!(r[0][15], Cell::Date("2024-02-29".into()));
    // Binary protocol: no fraction when it's zero.
    assert!(matches!(&r[0][16], Cell::Text(t) if t.starts_with("-838:59:59")), "{:?}", r[0][16]);
    assert!(matches!(&r[0][17], Cell::DateTime(t) if t.starts_with("9999-12-31 23:59:59.999999")), "{:?}", r[0][17]);
    assert!(matches!(&r[0][18], Cell::DateTimeTz(t) if t.starts_with("2038-01-19 03:14:07.999999") && t.ends_with("+00:00")), "{:?}", r[0][18]);
    assert_eq!(r[0][19], Cell::Int(2155));
    assert!(matches!(&r[0][20], Cell::Json(_) | Cell::Text(_)), "{:?}", r[0][20]);
    assert_eq!(r[0][21], Cell::UInt(0b1010101010));
    assert_eq!(r[0][22], Cell::Text("b c".into()));
    assert_eq!(r[0][23], Cell::Text("x,y".into()));
    if geometry {
        assert!(matches!(&r[0][25], Cell::Bytes(b) if b.len() == 25), "{:?}", r[0][25]);
    }
    assert!(r[1][1..].iter().all(|c| *c == Cell::Null));
    assert_eq!(r[2][10], Cell::Text(String::new()));
    assert_eq!(r[2][16], Cell::Text("25:00:00.500000".into()));
    assert!(matches!(&r[3][14], Cell::Bytes(b) if b.len() == 873_814 * 6));

    run(s, "DROP TABLE IF EXISTS dst").await;
    run(s, &format!("CREATE TABLE dst {}", mysql_types(geometry))).await;
    let start = Instant::now();
    let (n, calls) = load(s, "dst", &src, 2).await;
    eprintln!("{label}: all types loaded in {:?}, progress {calls:?}", start.elapsed());
    assert_eq!(n, 4);
    assert_eq!(calls, vec![2, 4], "a progress call per committed window");
    let back = read_all(s, "dst").await;
    assert_same(&r, &by_id(rows(&back)));
}

/// 1M mixed rows: read, load, compare aggregates; prints rows/s.
async fn benchmark(s: &mut Box<dyn Session>, label: &str, create: &str, fill: &[String], checksum: &str) {
    run(s, "DROP TABLE IF EXISTS big").await;
    run(s, "DROP TABLE IF EXISTS big_dst").await;
    run(s, &format!("CREATE TABLE big {create}")).await;
    run(s, &format!("CREATE TABLE big_dst {create}")).await;
    let t = Instant::now();
    for sql in fill {
        run(s, sql).await;
    }
    eprintln!("{label}: 1M rows generated in {:?}", t.elapsed());

    let t = Instant::now();
    let data = read_all(s, "big").await;
    let read = t.elapsed();
    assert_eq!(data.rows, 1_000_000);
    let t = Instant::now();
    let (n, calls) = load(s, "big_dst", &data, LoadSpec::DEFAULT_COMMIT_ROWS).await;
    let loaded = t.elapsed();
    assert_eq!(n, 1_000_000);
    assert_eq!(calls.last(), Some(&1_000_000));
    eprintln!(
        "{label}: read {:.0} rows/s ({read:?}), bulk load {:.0} rows/s ({loaded:?}), {} commits",
        1e6 / read.as_secs_f64(),
        1e6 / loaded.as_secs_f64(),
        calls.len()
    );
    let a = run(s, &checksum.replace("{t}", "big")).await;
    let b = run(s, &checksum.replace("{t}", "big_dst")).await;
    assert_eq!(a, b, "checksums");
}

const BIG_MYSQL: &str = "(id BIGINT PRIMARY KEY, i INT, d DOUBLE, de DECIMAL(12,2), s VARCHAR(40), dt DATETIME(6), bl VARBINARY(16))";
const CHECK_MYSQL: &str = "SELECT COUNT(*), SUM(id), SUM(i), MIN(d), MAX(d), SUM(CRC32(d)), SUM(de), SUM(CRC32(s)), SUM(CRC32(bl)), MAX(dt) FROM {t}";

async fn digits(s: &mut Box<dyn Session>, ddl: &str) {
    run(s, "DROP TABLE IF EXISTS d10").await;
    run(s, &format!("CREATE TABLE d10 (n INT){ddl}")).await;
    run(s, "INSERT INTO d10 VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)").await;
}

/// 100k rows per statement (TiDB's transaction limit).
fn fill_big(expr: &str) -> Vec<String> {
    (0..10)
        .map(|k| {
            format!(
                "INSERT INTO big SELECT {expr} FROM (SELECT a.n + 10*b.n + 100*c.n + 1000*d.n + 10000*e.n + 100000*{k} AS x
                 FROM d10 a, d10 b, d10 c, d10 d, d10 e) q"
            )
        })
        .collect()
}

const BIG_ROW_MYSQL: &str = "x, x % 1000 - 500, x / 7, x / 100, CONCAT('row ', x, ' ñ\\t'), \
    TIMESTAMPADD(MICROSECOND, x, '2020-01-01 00:00:00'), UNHEX(LPAD(HEX(x), 8, '0'))";

async fn mysql_family(id: &str, env: &str, default: &str, geometry: bool) {
    let Some(mut s) = connect(id, env, default, Some(DB)).await else { return };
    let version = s.server_version().await.unwrap();
    eprintln!("{id}: {version}");
    fill_mysql(&mut s, geometry).await;
    round_trip(&mut s, id, geometry).await;
    if std::env::var_os("DBINE_TEST_NO_BENCH").is_some() {
        // The 1M-row benchmark is heavy on a small Docker VM.
        return;
    }
    digits(&mut s, "").await;
    benchmark(&mut s, id, BIG_MYSQL, &fill_big(BIG_ROW_MYSQL), CHECK_MYSQL).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_mysql() {
    let (env, default) = ("DBINE_TEST_MYSQL_URL", "mysql://root:pw@localhost:25011");
    let Some(mut admin) = connect("mysql", env, default, None).await else { return };
    let before = run(&mut admin, "SELECT @@GLOBAL.local_infile").await;
    let before = before[0][0].to_string().trim_matches('"').to_string();
    // First as the server is (MySQL 8 has local_infile off: prepared INSERTs)…
    run(&mut admin, "SET GLOBAL local_infile = 0").await;
    eprintln!("mysql: local_infile = 0 (prepared INSERTs)");
    mysql_family("mysql", env, default, true).await;
    // …then with LOAD DATA LOCAL.
    run(&mut admin, "SET GLOBAL local_infile = 1").await;
    eprintln!("mysql: local_infile = 1 (LOAD DATA LOCAL)");
    mysql_family("mysql", env, default, true).await;
    run(&mut admin, &format!("SET GLOBAL local_infile = {before}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_mariadb() {
    mysql_family("mariadb", "DBINE_TEST_MARIADB_URL", "mysql://root:pw@localhost:25012", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_tidb() {
    mysql_family("tidb", "DBINE_TEST_TIDB_URL", "mysql://root@localhost:25014", false).await;
}

// ------------------------------------------------- MySQL family: edge cases

fn col(name: &str) -> TransferColumn {
    TransferColumn { name: name.into(), type_name: String::new(), nullable: true }
}

fn spec(name: &str, columns: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// Loads `rows` (one batch each) into `name`: the outcome and the progress calls.
async fn try_load(s: &mut Box<dyn Session>, name: &str, columns: &[&str], rows: Vec<Vec<Cell>>, commit_rows: u64) -> (dbine_driver::Result<u64>, Vec<u64>) {
    let cols: Vec<TransferColumn> = columns.iter().map(|c| col(c)).collect();
    let calls = Mutex::new(Vec::new());
    let progress = |n: u64| calls.lock().unwrap().push(n);
    let mut source = Source(rows.into_iter().map(|r| RowBatch { bytes: r.iter().map(Cell::size).sum(), rows: vec![r] }).collect());
    let r = s.bulk_load(&spec(name, columns, commit_rows), &cols, &mut source, &progress).await;
    (r, calls.into_inner().unwrap())
}

/// Rows in `name`, counted by a session of its own (what's committed).
async fn committed(id: &str, env: &str, default: &str, name: &str) -> i64 {
    let mut other = connect(id, env, default, Some(DB)).await.unwrap();
    let r = run(&mut other, &format!("SELECT COUNT(*) FROM {name}")).await;
    r[0][0].to_string().trim_matches('"').parse().unwrap()
}

/// A failed window leaves nothing behind (TiDB's LOAD DATA commits on its
/// own; MySQL 8 drops non-UTF-8 bytes from a parameter silently), and a
/// warning is reported as the server's (TiDB refuses `SHOW WARNINGS LIMIT`).
async fn failures(id: &str, env: &str, default: &str) {
    let Some(mut s) = connect(id, env, default, Some(DB)).await else { return };
    let t = |i: i64, v: &str| vec![Cell::Int(i), Cell::Text(v.into())];
    async fn reset(s: &mut Box<dyn Session>) {
        run(s, "DROP TABLE IF EXISTS fails").await;
        run(s, "CREATE TABLE fails (id INT PRIMARY KEY, v VARCHAR(3) CHARACTER SET utf8mb3 NOT NULL)").await;
    }

    // A duplicate key in the second window: only the first one stays.
    reset(&mut s).await;
    let (r, calls) = try_load(&mut s, "fails", &["id", "v"], vec![t(1, "a"), t(2, "b"), t(3, "c"), t(3, "d")], 2).await;
    assert!(r.is_err(), "{id}: duplicate key: {r:?}");
    assert_eq!(calls, vec![2], "{id}: duplicate key");
    assert_eq!(committed(id, env, default, "fails").await, 2, "{id}: duplicate key");

    let bad: Vec<(&str, Vec<Cell>)> = vec![
        ("too long", t(2, "toolong")),
        ("NULL into NOT NULL", vec![Cell::Int(2), Cell::Null]),
        ("emoji into utf8mb3", t(2, "🦀")),
        ("text into INT", vec![Cell::Text("x1".into()), Cell::Text("b".into())]),
        ("non-UTF-8 bytes", vec![Cell::Int(2), Cell::Bytes(vec![0xFF, 0xFE])]),
        ("a non-UTF-8 byte", vec![Cell::Int(2), Cell::Bytes(vec![0x61, 0xFF, 0x62])]),
    ];
    for strict in [true, false] {
        for (label, row) in &bad {
            reset(&mut s).await;
            if !strict {
                // Warnings instead of errors: still a failure, the server's own.
                run(&mut s, "SET SESSION sql_mode = ''").await;
            }
            let (r, calls) = try_load(&mut s, "fails", &["id", "v"], vec![t(1, "ab"), row.clone()], 1000).await;
            if !strict {
                run(&mut s, "SET SESSION sql_mode = DEFAULT").await;
            }
            let e = r.expect_err(&format!("{id}: {label} (strict {strict})")).to_string();
            eprintln!("{id}: {label} (strict {strict}): {e}");
            assert!(!e.contains("1064") && !e.contains("syntax"), "{id}: {label}: {e}");
            assert!(calls.is_empty(), "{id}: {label}: {calls:?}");
            assert_eq!(committed(id, env, default, "fails").await, 0, "{id}: {label} (strict {strict})");
        }
    }
    // Valid UTF-8 bytes are text.
    reset(&mut s).await;
    let (r, _) = try_load(&mut s, "fails", &["id", "v"], vec![vec![Cell::Int(1), Cell::Bytes("ñ".into())]], 10).await;
    assert_eq!(r.unwrap(), 1);
    run(&mut s, "DROP TABLE fails").await;
}

/// A load dropped mid-window (the source stalls): nothing gets committed,
/// not even after the session goes.
async fn cancel(id: &str, env: &str, default: &str) {
    struct Stall(VecDeque<RowBatch>);
    #[dbine_driver::async_trait]
    impl BatchSource for Stall {
        async fn next(&mut self) -> Option<RowBatch> {
            match self.0.pop_front() {
                Some(b) => Some(b),
                None => std::future::pending().await,
            }
        }
    }
    let Some(mut s) = connect(id, env, default, Some(DB)).await else { return };
    run(&mut s, "DROP TABLE IF EXISTS cancels").await;
    run(&mut s, "CREATE TABLE cancels (id INT PRIMARY KEY, v VARCHAR(100))").await;
    let batches = (0..20)
        .map(|k| {
            let rows: Vec<Vec<Cell>> = (0..1000).map(|i| vec![Cell::Int(k * 1000 + i), Cell::Text(format!("row {i} {}", "x".repeat(60)))]).collect();
            RowBatch { bytes: rows.iter().flatten().map(Cell::size).sum(), rows }
        })
        .collect();
    let calls = Mutex::new(Vec::new());
    let progress = |n: u64| calls.lock().unwrap().push(n);
    let mut source = Stall(batches);
    let spec = spec("cancels", &["id", "v"], 1_000_000);
    let r = tokio::time::timeout(Duration::from_secs(3), s.bulk_load(&spec, &[col("id"), col("v")], &mut source, &progress)).await;
    assert!(r.is_err(), "{id}: the load should still be waiting");
    assert!(calls.lock().unwrap().is_empty());
    drop(s);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(committed(id, env, default, "cancels").await, 0, "{id}: rows committed after a cancel");
}

/// FLOAT's extremes, and a BLOB and a text of 12 MB (over half of
/// MariaDB's 16 MiB max_allowed_packet, so twice that as hex).
async fn extremes(id: &str, env: &str, default: &str, big: bool) {
    let Some(mut s) = connect(id, env, default, Some(DB)).await else { return };
    for t in ["fl", "fl2"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} (id INT PRIMARY KEY, f FLOAT, d DOUBLE)")).await;
    }
    run(&mut s, "INSERT INTO fl VALUES (1, 3.4028234663852886e38, 1.7976931348623157e308), (2, -3.4028234663852886e38, -1.7976931348623157e308), (3, 0.1, 0.1), (4, 1.1754943508222875e-38, 5e-324)").await;
    let src = read_all(&mut s, "fl").await;
    let (n, _) = load(&mut s, "fl2", &src, 1000).await;
    assert_eq!(n, 4);
    assert_same(&by_id(rows(&src)), &by_id(rows(&read_all(&mut s, "fl2").await)));
    let same = run(&mut s, "SELECT COUNT(*) FROM fl JOIN fl2 USING (id) WHERE fl.f = fl2.f AND fl.d = fl2.d").await;
    assert_eq!(same[0][0].to_string().trim_matches('"'), "4", "{id}: FLOAT extremes");
    run(&mut s, "DROP TABLE fl").await;
    run(&mut s, "DROP TABLE fl2").await;
    if !big {
        return;
    }
    for t in ["bigb", "bigb2"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} (id INT PRIMARY KEY, b LONGBLOB, t LONGTEXT)")).await;
    }
    run(&mut s, "INSERT INTO bigb VALUES (1, REPEAT(UNHEX('00FF5C0A'), 3000000), NULL), (2, NULL, REPEAT('ñ\\\\x\\t', 2400000))").await;
    let src = read_all(&mut s, "bigb").await;
    let (n, _) = load(&mut s, "bigb2", &src, 1000).await;
    assert_eq!(n, 2);
    let check = "SELECT COUNT(*), SUM(LENGTH(b)), SUM(CRC32(b)), SUM(LENGTH(t)), SUM(CRC32(t)) FROM {t}";
    let a = run(&mut s, &check.replace("{t}", "bigb")).await;
    let b = run(&mut s, &check.replace("{t}", "bigb2")).await;
    eprintln!("{id}: big values {a:?}");
    assert_eq!(a, b, "{id}: 12 MB values");
    run(&mut s, "DROP TABLE bigb").await;
    run(&mut s, "DROP TABLE bigb2").await;
    // A wide row of medium values (each under the slicing limit, 8.6 MB in
    // all): still sent in bounded packets, not one per row.
    let cols: Vec<String> = (0..33).map(|i| format!("b{i}")).collect();
    for t in ["wide", "wide2"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        let defs: Vec<String> = cols.iter().map(|c| format!("{c} LONGBLOB")).collect();
        run(&mut s, &format!("CREATE TABLE {t} (id INT PRIMARY KEY, {})", defs.join(", "))).await;
    }
    let vals: Vec<String> = (0..33).map(|i| format!("REPEAT(UNHEX('{:02X}5C0A09'), 65500)", i)).collect();
    run(&mut s, &format!("INSERT INTO wide VALUES (1, {})", vals.join(", "))).await;
    let src = read_all(&mut s, "wide").await;
    let (n, progress) = load(&mut s, "wide2", &src, 1000).await;
    assert_eq!((n, progress), (1, vec![1]), "{id}: wide row");
    let sums: Vec<String> = cols.iter().map(|c| format!("CRC32({c})")).collect();
    let check = format!("SELECT COUNT(*), SUM(LENGTH(CONCAT({}))), SUM({}) FROM {{t}}", cols.join(", "), sums.join(" + "));
    let a = run(&mut s, &check.replace("{t}", "wide")).await;
    let b = run(&mut s, &check.replace("{t}", "wide2")).await;
    eprintln!("{id}: wide row {a:?}");
    assert_eq!(a, b, "{id}: wide row of medium values");
    run(&mut s, "DROP TABLE wide").await;
    run(&mut s, "DROP TABLE wide2").await;
}

async fn edge_cases(id: &str, env: &str, default: &str, big: bool) {
    failures(id, env, default).await;
    cancel(id, env, default).await;
    extremes(id, env, default, big).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edge_cases_mysql() {
    let (env, default) = ("DBINE_TEST_MYSQL_URL", "mysql://root:pw@localhost:25011");
    let Some(mut admin) = connect("mysql", env, default, None).await else { return };
    let before = run(&mut admin, "SELECT @@GLOBAL.local_infile").await;
    let before = before[0][0].to_string().trim_matches('"').to_string();
    for on in [0, 1] {
        run(&mut admin, &format!("SET GLOBAL local_infile = {on}")).await;
        eprintln!("mysql: local_infile = {on}");
        edge_cases("mysql", env, default, true).await;
    }
    run(&mut admin, &format!("SET GLOBAL local_infile = {before}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edge_cases_mariadb() {
    edge_cases("mariadb", "DBINE_TEST_MARIADB_URL", "mysql://root:pw@localhost:25012", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_edge_cases_tidb() {
    // A 12 MB row is over TiDB's txn-entry-size-limit (6 MB): no big values.
    edge_cases("tidb", "DBINE_TEST_TIDB_URL", "mysql://root@localhost:25014", false).await;
}

// ------------------------------------------------------------ analytical engines

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_starrocks() {
    let Some(mut s) = connect("starrocks", "DBINE_TEST_STARROCKS_URL", "mysql://root@localhost:25030", Some(DB)).await else {
        return;
    };
    eprintln!("starrocks: {}", s.server_version().await.unwrap());
    let props = " DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES ('replication_num' = '1')";
    let types = format!(
        "(id INT, ti TINYINT, bi BIGINT, li LARGEINT, de DECIMAL(38,10), f FLOAT, d DOUBLE, b BOOLEAN, c CHAR(5),
          vc VARCHAR(200), s STRING, vb VARBINARY(1048576), dt DATE, dtm DATETIME, j JSON){props}"
    );
    for t in ["src", "dst"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} {types}")).await;
    }
    run(
        &mut s,
        "INSERT INTO src VALUES
         (1, -128, -9223372036854775808, 170141183460469231731687303715884105727, '-1234567890123456789012345678.0123456789',
          0.1, 1e300, true, 'ab', 'tab\\there\\nnew \\\\ back ñ 🦀', '\\\\N', X'00FF0A095C', '2024-02-29',
          '2024-02-29 13:14:15', parse_json('{\"a\": [1, \"x\"]}')),
         (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
         (3, 0, 0, 0, 0, -0.5, 0, false, '', '', '', X'', '0001-01-01', '0001-01-01 00:00:00', parse_json('[]'))",
    )
    .await;
    // StarRocks strings and binaries hold up to 1 MB (not 5): a 512 KB blob
    // (its hex text is the 1 MB).
    run(&mut s, "INSERT INTO src (id, vb) VALUES (4, to_binary(repeat('00FF5C0A090D', 174762), 'hex'))").await;
    let src = read_all(&mut s, "src").await;
    assert_eq!(src.rows, 4);
    let r = by_id(rows(&src));
    eprintln!("starrocks row 1: {:?}", r[0].iter().map(|c| format!("{c:?}").chars().take(60).collect::<String>()).collect::<Vec<_>>());
    assert_eq!(r[0][2], Cell::Int(i64::MIN));
    assert_eq!(r[0][3], Cell::Decimal("170141183460469231731687303715884105727".into()));
    assert_eq!(r[0][9], Cell::Text("tab\there\nnew \\ back ñ 🦀".into()));
    assert_eq!(r[0][11], Cell::Bytes(vec![0, 0xff, 0x0a, 0x09, 0x5c]));
    let (n, calls) = load(&mut s, "dst", &src, 2).await;
    assert_eq!(n, 4);
    assert_eq!(calls.last(), Some(&4));
    let back = by_id(rows(&read_all(&mut s, "dst").await));
    assert_same(&r, &back);

    digits(&mut s, props.replace("id", "n").as_str()).await;
    let row = "x, x % 1000 - 500, x / 7, x / 100, concat('row ', x, ' ñ\\t'), \
        date_add('2020-01-01 00:00:00', INTERVAL x SECOND), to_binary(lpad(hex(x), 8, '0'), 'hex')";
    benchmark(
        &mut s,
        "starrocks",
        &format!("(id BIGINT, i INT, d DOUBLE, de DECIMAL(12,2), s VARCHAR(40), dt DATETIME, bl VARBINARY(16)){props}"),
        &fill_big(row),
        "SELECT COUNT(*), SUM(id), SUM(i), MIN(d), MAX(d), SUM(de), SUM(length(s)), MAX(s), MAX(dt), SUM(length(bl)) FROM {t}",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_greptimedb() {
    let Some(mut s) = connect("greptimedb", "DBINE_TEST_GREPTIMEDB_URL", "mysql://localhost:25017", Some(DB)).await else {
        return;
    };
    eprintln!("greptimedb: {}", s.server_version().await.unwrap());
    let types = "(ts TIMESTAMP(3) TIME INDEX, k INT, i BIGINT, u BIGINT UNSIGNED, f DOUBLE, s STRING, b BOOLEAN, PRIMARY KEY (k))";
    for t in ["src", "dst"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
        run(&mut s, &format!("CREATE TABLE {t} {types}")).await;
    }
    run(
        &mut s,
        "INSERT INTO src VALUES ('2024-01-01 00:00:00.123', 1, -9223372036854775808, 18446744073709551615, 0.1,
         'tab\\there ñ \\\\ it''s', true), ('2024-01-01 00:00:01', 2, NULL, NULL, NULL, NULL, NULL)",
    )
    .await;
    let src = read_all(&mut s, "src").await;
    assert_eq!(src.rows, 2);
    eprintln!("greptimedb rows: {:?}", rows(&src));
    let (n, _) = load(&mut s, "dst", &src, 1).await;
    assert_eq!(n, 2);
    let mut a = rows(&src);
    let mut b = rows(&read_all(&mut s, "dst").await);
    a.sort_by_key(|x| format!("{:?}", x[1]));
    b.sort_by_key(|x| format!("{:?}", x[1]));
    assert_same(&a, &b);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_manticore() {
    let Some(mut s) = connect("manticore", "DBINE_TEST_MANTICORE_URL", "mysql://localhost:25016", None).await else { return };
    assert!(!driver("manticore").supports_bulk_load());
    run(&mut s, "DROP TABLE IF EXISTS dbine_transfer_rt").await;
    run(&mut s, "CREATE TABLE dbine_transfer_rt (title TEXT, n INT, price FLOAT)").await;
    // 25,000 rows: more than one page (and than max_matches' default).
    for k in 0..25 {
        let values: Vec<String> = (0..1000).map(|i| format!("({}, 'doc {i}', {i}, 1.5)", k * 1000 + i + 1)).collect();
        run(&mut s, &format!("INSERT INTO dbine_transfer_rt (id, title, n, price) VALUES {}", values.join(", "))).await;
    }
    let all = read_all(&mut s, "dbine_transfer_rt").await;
    assert_eq!(all.rows, 25_000);
    let ids: Vec<Cell> = rows(&all).into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids.first(), Some(&Cell::Int(1)));
    assert_eq!(ids.last(), Some(&Cell::Int(25_000)));
    // Only some columns, without id: paged anyway, id not handed over.
    let some = read(&mut s, ReadSpec { table: table("dbine_transfer_rt"), columns: Some(vec!["n".into()]), filter: None }).await;
    assert_eq!(some.rows, 25_000);
    assert_eq!(some.columns.len(), 1);
    run(&mut s, "DROP TABLE dbine_transfer_rt").await;
}

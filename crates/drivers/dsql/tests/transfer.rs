//! Bulk transfer through the password test hook against a plain PostgreSQL
//! (DSQL has no emulator: this checks the wire, the conversions, the
//! windows and the parallel connections, not DSQL's own limits).
//!
//! ```sh
//! docker start dbine-test-dsqlpg   # postgres:16, password dbine, port 25301
//! DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test transfer -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

async fn connect() -> Option<Box<dyn Session>> {
    let url = std::env::var("DBINE_TEST_DSQL_URL").unwrap_or_else(|_| "localhost:25301".into());
    let (host, port) = url.split_once(':')?;
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some("postgres".into()),
        ..Default::default()
    };
    match dbine_driver_dsql::connect_with_password(&cfg, "dbine").await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("skipped, no server at {url}: {e}");
            None
        }
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn obj(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("public".into()), name: name.into() }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
    batches: usize,
}
impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> io::Result<()> {
        self.columns = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches += 1;
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct VecSource(std::vec::IntoIter<RowBatch>);
#[dbine_driver::async_trait]
impl BatchSource for VecSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn source(rows: Vec<Vec<Cell>>) -> VecSource {
    let batches: Vec<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    VecSource(batches.into_iter())
}

async fn read(s: &mut Box<dyn Session>, table: &str, columns: Option<Vec<String>>, filter: Option<&str>) -> dbine_driver::Result<Collect> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: obj(table), columns, filter: filter.map(Into::into) };
    s.read_batches(&spec, sink.clone()).await?;
    let c = std::mem::take(&mut *sink.lock().unwrap());
    Ok(c)
}

const COLS: &[&str] = &["id", "u", "i2", "i8", "n", "r", "d", "b", "vc", "ch", "t", "bin", "dt", "tm", "ts", "tstz", "iv"];

/// PostgreSQL prints fractional seconds without trailing zeros.
fn frac(s: String) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn row(i: i64) -> Vec<Cell> {
    if i % 10 == 9 {
        // Every nullable column NULL.
        let mut r = vec![Cell::Int(i)];
        r.extend(std::iter::repeat_n(Cell::Null, COLS.len() - 1));
        return r;
    }
    let text = |s: String| Cell::Text(s);
    vec![
        Cell::Int(i),
        Cell::Uuid(format!("00000000-0000-4000-8000-{i:012x}")),
        Cell::Int(i % 32_000 - 16_000),
        Cell::Int(i * 1_000_000_007 - 9_000_000_000_000_000_000 / 2),
        Cell::Decimal(format!("{}.{:010}", i * 12_345_678_901, i % 997)),
        Cell::Float((i % 4096) as f64 * 0.25),
        Cell::Float(i as f64 / 7.0),
        Cell::Bool(i % 2 == 0),
        text(format!("fila {i} — ñandú")),
        text(format!("{:<5}", i % 100_000).chars().take(5).collect()),
        text(format!("texto largo {} {}", i, "x".repeat((i % 50) as usize))),
        Cell::Bytes((0..(i % 64) as u8).map(|x| x.wrapping_mul(37).wrapping_add(i as u8)).collect()),
        Cell::Date(format!("{:04}-{:02}-{:02}", 1990 + i % 40, 1 + i % 12, 1 + i % 28)),
        Cell::Time(frac(format!("{:02}:{:02}:{:02}.{:06}", i % 24, i % 60, (i / 60) % 60, i % 1_000_000))),
        Cell::DateTime(frac(format!("2024-{:02}-{:02} {:02}:{:02}:{:02}.{:06}", 1 + i % 12, 1 + i % 28, i % 24, i % 60, i % 60, i % 1_000_000))),
        Cell::DateTimeTz(format!("2023-{:02}-{:02} {:02}:30:00+00:00", 1 + i % 12, 1 + i % 28, i % 24)),
        text(format!("{:02}:00:00", i % 24)),
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_types_windows_and_read_back() {
    let Some(mut s) = connect().await else { return };
    run(
        &mut s,
        "SET TIME ZONE 'UTC';
         DROP TABLE IF EXISTS xfer_all;
         CREATE TABLE xfer_all (id bigint PRIMARY KEY, u uuid, i2 smallint, i8 bigint, n numeric(38,10), r real,
             d double precision, b boolean, vc varchar(50), ch char(5), t text, bin bytea, dt date, tm time,
             ts timestamp, tstz timestamptz, iv interval)",
    )
    .await;
    let driver = dbine_driver_dsql::drivers().remove(0);
    assert!(driver.supports_bulk_load());

    const N: i64 = 50_000;
    let rows: Vec<Vec<Cell>> = (0..N).map(row).collect();
    let spec = LoadSpec {
        table: obj("xfer_all"),
        columns: COLS.iter().map(|c| c.to_string()).collect(),
        table_lock: true,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
    let seen2 = seen.clone();
    let progress = move |n: u64| seen2.lock().unwrap().push(n);
    let started = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut source(rows.clone()), &progress).await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    println!("dsql bulk_load: {N} rows in {secs:.2} s = {:.0} rows/s", N as f64 / secs);
    assert_eq!(loaded, N as u64);
    // One progress call per committed window of at most 3,000 rows, in order.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), (N as usize).div_ceil(3000));
    assert!(seen.windows(2).all(|w| w[0] < w[1] && w[1] - w[0] <= 3000), "{seen:?}");
    assert_eq!(*seen.last().unwrap(), N as u64);

    run(&mut s, "SET TIME ZONE 'UTC'").await;
    let started = Instant::now();
    let got = read(&mut s, "xfer_all", Some(COLS.iter().map(|c| c.to_string()).collect()), None).await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    println!("dsql read_batches: {N} rows in {secs:.2} s = {:.0} rows/s", N as f64 / secs);
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), COLS);
    assert_eq!(got.columns[4].type_name, "numeric(38,10)");
    assert!(!got.columns[0].nullable && got.columns[1].nullable);
    assert!(got.batches >= 50);
    let mut back = got.rows;
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => panic!("id {:?}", r[0]),
    });
    assert_eq!(back.len(), rows.len());
    for (a, b) in rows.iter().zip(&back) {
        assert_eq!(a, b, "row {:?}", a[0]);
    }

    // The requested order, a filter, and a column that isn't there.
    let got = read(&mut s, "xfer_all", Some(vec!["B".into(), "id".into()]), Some("id < 3")).await.unwrap();
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["b", "id"]);
    let mut r = got.rows;
    r.sort_by_key(|r| format!("{:?}", r[1]));
    assert_eq!(r, vec![vec![Cell::Bool(true), Cell::Int(0)], vec![Cell::Bool(false), Cell::Int(1)], vec![Cell::Bool(true), Cell::Int(2)]]);
    assert!(read(&mut s, "xfer_all", Some(vec!["nope".into()]), None).await.is_err());
    run(&mut s, "DROP TABLE xfer_all").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_large_binaries_errors_and_identity() {
    let Some(mut s) = connect().await else { return };
    run(
        &mut s,
        "DROP TABLE IF EXISTS xfer_big;
         CREATE TABLE xfer_big (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name varchar(3), data bytea)",
    )
    .await;
    // 6 MiB binaries: each over the 4 MiB window, so each is a window alone.
    let big = |seed: u8| Cell::Bytes((0..6 * 1024 * 1024).map(|i: usize| (i as u8).wrapping_mul(31) ^ seed).collect());
    let rows = vec![
        vec![Cell::Int(1), Cell::Text("a".into()), big(1)],
        vec![Cell::Int(2), Cell::Text("b".into()), big(2)],
        vec![Cell::Int(3), Cell::Text("c".into()), Cell::Null],
    ];
    let mut spec = LoadSpec {
        table: obj("xfer_big"),
        columns: vec!["id".into(), "name".into(), "data".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 0,
        commit_bytes: 0,
    };
    let calls = Arc::new(Mutex::new(0));
    let c2 = calls.clone();
    let progress = move |_: u64| *c2.lock().unwrap() += 1;
    assert_eq!(s.bulk_load(&spec, &[], &mut source(rows.clone()), &progress).await.unwrap(), 3);
    assert_eq!(*calls.lock().unwrap(), 3);
    let got = read(&mut s, "xfer_big", None, None).await.unwrap();
    let mut back = got.rows;
    back.sort_by_key(|r| format!("{:?}", r[0]));
    assert_eq!(back, rows);

    // Without keep_identity a GENERATED ALWAYS column refuses the values.
    spec.keep_identity = false;
    let one = vec![vec![Cell::Int(9), Cell::Text("z".into()), Cell::Null]];
    assert!(s.bulk_load(&spec, &[], &mut source(one), &|_| {}).await.is_err());
    // Too long for varchar(3): an error, never truncated.
    spec.keep_identity = true;
    let long = vec![vec![Cell::Int(10), Cell::Text("demasiado".into()), Cell::Null]];
    let e = s.bulk_load(&spec, &[], &mut source(long), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("too long"), "{e}");
    run(&mut s, "DROP TABLE xfer_big").await;
}

async fn count(s: &mut Box<dyn Session>, table: &str) -> usize {
    read(s, table, Some(vec!["id".into()]), None).await.unwrap().rows.len()
}

/// Reads go in keyset pages by the primary key (DSQL ends a transaction
/// after 5 minutes): a composite key not among the requested columns, a
/// filter, and every row exactly once.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_reads_in_keyset_pages() {
    let Some(mut s) = connect().await else { return };
    run(
        &mut s,
        "DROP TABLE IF EXISTS xfer_pages;
         CREATE TABLE xfer_pages (\"K b\" text, id int, v text, PRIMARY KEY (\"K b\", id));
         INSERT INTO xfer_pages SELECT CASE WHEN g % 3 = 0 THEN 'o''k\\' ELSE 'x' || (g % 7) END, g, 'v' || g
           FROM generate_series(1, 20000) g",
    )
    .await;
    let got = read(&mut s, "xfer_pages", Some(vec!["v".into()]), None).await.unwrap();
    assert_eq!(got.columns.len(), 1);
    let mut vs: Vec<String> = got
        .rows
        .iter()
        .map(|r| match &r[..] {
            [Cell::Text(v)] => v.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    vs.sort();
    vs.dedup();
    assert_eq!(vs.len(), 20_000);
    let got = read(&mut s, "xfer_pages", Some(vec!["id".into()]), Some("id % 2 = 0")).await.unwrap();
    assert_eq!(got.rows.len(), 10_000);
    run(&mut s, "DROP TABLE xfer_pages").await;
}

/// A window refused by a size limit (SQLSTATE class 54; here PostgreSQL's
/// index row size, on DSQL its 10 MiB with index entries) is split and sent
/// again in halves; a single row that still doesn't fit is the error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_splits_windows_too_big() {
    let Some(mut s) = connect().await else { return };
    run(
        &mut s,
        "DROP TABLE IF EXISTS xfer_split;
         CREATE TABLE xfer_split (id int PRIMARY KEY, t text);
         CREATE INDEX xfer_split_t ON xfer_split (t)",
    )
    .await;
    // Incompressible text over btree's ~2.7 kB per entry.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let wide: String = (0..8000)
        .map(|_| {
            // xorshift64: no pattern for the compressor to find.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'!' + (x % 90) as u8)
        })
        .collect();
    let spec = LoadSpec {
        table: obj("xfer_split"),
        columns: vec!["id".into(), "t".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 0,
        commit_bytes: 0,
    };
    let mut rows: Vec<Vec<Cell>> = (0..21).map(|i| vec![Cell::Int(i), Cell::Text(format!("fila {i}"))]).collect();
    rows[15][1] = Cell::Text(wide);
    let e = s.bulk_load(&spec, &[], &mut source(rows), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("index row size"), "{e}");
    // The halves without the wide row went in: the window was split.
    let n = count(&mut s, "xfer_split").await;
    assert!((11..21).contains(&n), "{n}");
    run(&mut s, "DROP TABLE xfer_split").await;
}

/// When a window fails, no new window starts and the ones in flight are
/// awaited: nothing commits after bulk_load returned its error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_failure_leaves_nothing_in_flight() {
    let Some(mut s) = connect().await else { return };
    run(&mut s, "DROP TABLE IF EXISTS xfer_fail; CREATE TABLE xfer_fail (id int PRIMARY KEY, t text)").await;
    let spec = LoadSpec {
        table: obj("xfer_fail"),
        columns: vec!["id".into(), "t".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 500,
        commit_bytes: 0,
    };
    // A duplicate key in a middle window, and a short row at the end.
    let mut rows: Vec<Vec<Cell>> = (0..60_000).map(|i| vec![Cell::Int(i), Cell::Text("x".repeat(200))]).collect();
    rows[30_001][0] = Cell::Int(5);
    rows.push(vec![Cell::Int(1)]);
    let e = s.bulk_load(&spec, &[], &mut source(rows), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("duplicate key"), "{e}");
    let after = count(&mut s, "xfer_fail").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(count(&mut s, "xfer_fail").await, after, "rows committed after the error");
    assert!(after < 60_000);

    // The producer's own error (a short row) stops the workers too.
    run(&mut s, "TRUNCATE xfer_fail").await;
    let mut rows: Vec<Vec<Cell>> = (0..60_000).map(|i| vec![Cell::Int(i), Cell::Text("y".into())]).collect();
    rows.insert(20_000, vec![Cell::Int(-1)]);
    let e = s.bulk_load(&spec, &[], &mut source(rows), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("una fila trae 1 valores"), "{e}");
    let after = count(&mut s, "xfer_fail").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(count(&mut s, "xfer_fail").await, after, "rows committed after the error");
    assert!(after <= 20_000, "{after}");
    run(&mut s, "DROP TABLE xfer_fail").await;
}

/// keep_identity moves the identity's sequence past the loaded values, and
/// an offset bound for a timestamp without zone is refused, not dropped.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_identity_reseed_and_zone_check() {
    let Some(mut s) = connect().await else { return };
    run(
        &mut s,
        "DROP TABLE IF EXISTS xfer_seq;
         CREATE TABLE xfer_seq (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text, ts timestamp)",
    )
    .await;
    let spec = LoadSpec {
        table: obj("xfer_seq"),
        columns: vec!["id".into(), "name".into(), "ts".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 0,
        commit_bytes: 0,
    };
    let rows = vec![
        vec![Cell::Int(10), Cell::Text("a".into()), Cell::DateTimeTz("2024-01-01 10:00:00+00:00".into())],
        vec![Cell::Int(250), Cell::Text("b".into()), Cell::Null],
    ];
    assert_eq!(s.bulk_load(&spec, &[], &mut source(rows), &|_| {}).await.unwrap(), 2);
    run(&mut s, "INSERT INTO xfer_seq (name) VALUES ('nueva')").await;
    let got = read(&mut s, "xfer_seq", Some(vec!["id".into(), "ts".into()]), Some("name = 'nueva' OR id = 10")).await.unwrap();
    let mut r = got.rows;
    r.sort_by_key(|r| format!("{:?}", r[0]));
    assert_eq!(r, vec![vec![Cell::Int(10), Cell::DateTime("2024-01-01 10:00:00".into())], vec![Cell::Int(251), Cell::Null]]);

    let bad = vec![vec![Cell::Int(300), Cell::Text("c".into()), Cell::DateTimeTz("2024-01-01 10:00:00-03:00".into())]];
    let e = s.bulk_load(&spec, &[], &mut source(bad), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("timestamp sin zona"), "{e}");
    run(&mut s, "DROP TABLE xfer_seq").await;
}

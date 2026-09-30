//! Bulk transfer end to end on real database files: typed reads, the
//! prepared-INSERT load, the attached-file native copy and a load benchmark
//! (1M rows in release; `cargo test --release -p dbine-driver-sqlite --test
//! transfer -- --nocapture` prints the rates).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn temp_db(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("dbine-sqlite-xfer-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

async fn open(path: &std::path::Path, read_only: bool) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), read_only, ..Default::default() };
    dbine_driver_sqlite::drivers().pop().unwrap().connect(&cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
}

#[derive(Default)]
struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
    batches: usize,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.cols = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.batches += 1;
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

/// Batches of `rows`, `per` rows each.
struct VecSource(std::vec::IntoIter<RowBatch>);

impl VecSource {
    fn new(rows: Vec<Vec<Cell>>, per: usize) -> Self {
        let batches: Vec<RowBatch> = rows.chunks(per).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
        VecSource(batches.into_iter())
    }
}

#[dbine_driver::async_trait]
impl BatchSource for VecSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn load_spec(name: &str, cols: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, i INTEGER, r REAL, s TEXT NOT NULL, b BLOB, n NUMERIC, d DATE)";

fn big_blob() -> Vec<u8> {
    (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn read_is_typed_and_blobs_whole() {
    let path = temp_db("read");
    let mut s = open(&path, false).await;
    run(&mut s, DDL).await;
    run(
        &mut s,
        "INSERT INTO t VALUES (1, -9223372036854775808, 1.5, 'ñandú', x'00FF', 12.5, '2024-01-31');
         INSERT INTO t VALUES (2, NULL, NULL, '', NULL, NULL, NULL);
         INSERT INTO t VALUES (3, 9223372036854775807, -0.25, 'x', zeroblob(5242880), 7, 'no-date');",
    )
    .await;
    let (cols, rows) = read_all(&mut s, &ReadSpec { table: table("t"), columns: None, filter: None }).await;
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "i", "r", "s", "b", "n", "d"]);
    assert_eq!(cols[4].type_name, "BLOB");
    assert!(!cols[0].nullable && !cols[3].nullable && cols[1].nullable);
    assert_eq!(
        rows[0],
        vec![
            Cell::Int(1),
            Cell::Int(i64::MIN),
            Cell::Float(1.5),
            Cell::Text("ñandú".into()),
            Cell::Bytes(vec![0, 255]),
            Cell::Float(12.5),
            Cell::Text("2024-01-31".into())
        ]
    );
    assert_eq!(rows[1][1..], [Cell::Null, Cell::Null, Cell::Text(String::new()), Cell::Null, Cell::Null, Cell::Null]);
    assert_eq!(rows[2][4], Cell::Bytes(vec![0; 5 * 1024 * 1024]));
    assert_eq!(rows[2][5], Cell::Int(7));

    // Column subset and filter.
    let spec = ReadSpec { table: table("t"), columns: Some(vec!["s".into(), "id".into()]), filter: Some("id >= 2".into()) };
    let (cols, rows) = read_all(&mut s, &spec).await;
    assert_eq!(cols.len(), 2);
    assert_eq!(rows, vec![vec![Cell::Text(String::new()), Cell::Int(2)], vec![Cell::Text("x".into()), Cell::Int(3)]]);

    // An unknown column is an error (SQLite would read a bare "nope" as a
    // string literal).
    let spec = ReadSpec { table: table("t"), columns: Some(vec!["id".into(), "nope".into()]), filter: None };
    let sink = Arc::new(Mutex::new(Collect::default()));
    let e = s.read_batches(&spec, sink).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("nope")), "{e:?}");

    // Only the keys SQLite keeps from NULL read as NOT NULL: the rowid
    // alias and a WITHOUT ROWID key, not any other key of a rowid table.
    run(
        &mut s,
        "CREATE TABLE v (x INTEGER, k TEXT, PRIMARY KEY (k));
         INSERT INTO v VALUES (1, NULL);
         CREATE TABLE w (k TEXT PRIMARY KEY, x INTEGER) WITHOUT ROWID;
         CREATE TABLE z (k INTEGER PRIMARY KEY DESC, x INTEGER);",
    )
    .await;
    let (cols, rows) = read_all(&mut s, &ReadSpec { table: table("v"), columns: None, filter: None }).await;
    assert!(cols[1].nullable && rows[0][1] == Cell::Null);
    let (cols, _) = read_all(&mut s, &ReadSpec { table: table("w"), columns: None, filter: None }).await;
    assert!(!cols[0].nullable && cols[1].nullable);
    let (cols, _) = read_all(&mut s, &ReadSpec { table: table("z"), columns: None, filter: None }).await;
    assert!(cols[0].nullable, "INTEGER PRIMARY KEY DESC is no rowid alias");
    drop(s);
    let _ = std::fs::remove_file(&path);
}

fn sample_rows() -> Vec<Vec<Cell>> {
    vec![
        vec![
            Cell::Int(1),
            Cell::Int(i64::MAX),
            Cell::Float(-1e300),
            Cell::Text("a'b\"c".into()),
            Cell::Bytes(big_blob()),
            Cell::Decimal("12.25".into()),
            Cell::Date("2024-02-29".into()),
        ],
        vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Text(String::new()), Cell::Null, Cell::Null, Cell::Null],
        vec![
            Cell::Int(3),
            Cell::Bool(true),
            Cell::Float(0.1),
            Cell::Uuid("123e4567-e89b-12d3-a456-426614174000".into()),
            Cell::Bytes(vec![]),
            Cell::UInt(u64::MAX),
            Cell::DateTimeTz("2024-01-01 10:00:00+02:00".into()),
        ],
    ]
}

#[tokio::test]
async fn bulk_load_round_trip_with_windows() {
    let path = temp_db("load");
    let mut s = open(&path, false).await;
    run(&mut s, DDL).await;
    let cols = ["id", "i", "r", "s", "b", "n", "d"];
    let committed = Arc::new(Mutex::new(Vec::new()));
    let c2 = committed.clone();
    let progress = move |n: u64| c2.lock().unwrap().push(n);
    let mut src = VecSource::new(sample_rows(), 2);
    let n = s.bulk_load(&load_spec("t", &cols, 2), &[], &mut src, &progress).await.unwrap();
    assert_eq!(n, 3);
    assert_eq!(*committed.lock().unwrap(), vec![2, 3]);

    let (_, rows) = read_all(&mut s, &ReadSpec { table: table("t"), columns: None, filter: None }).await;
    let want = sample_rows();
    assert_eq!(rows[0][..5], want[0][..5]);
    assert_eq!(rows[0][4], Cell::Bytes(big_blob()));
    assert_eq!(rows[0][5], Cell::Float(12.25)); // NUMERIC affinity
    assert_eq!(rows[1], want[1]);
    assert_eq!(rows[2][1], Cell::Int(1));
    assert_eq!(rows[2][3], Cell::Text("123e4567-e89b-12d3-a456-426614174000".into()));
    assert_eq!(rows[2][4], Cell::Bytes(vec![]));
    assert_eq!(rows[2][5], Cell::Float(u64::MAX as f64)); // NUMERIC affinity turns the digits into a REAL
    assert_eq!(rows[2][6], Cell::Text("2024-01-01 10:00:00+02:00".into()));

    // A failing row rolls back its window only: the first one stays.
    run(&mut s, "DELETE FROM t").await;
    let bad = vec![
        vec![Cell::Int(1), Cell::Null, Cell::Null, Cell::Text("ok".into()), Cell::Null, Cell::Null, Cell::Null],
        vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null],
    ];
    let mut src = VecSource::new(bad, 1);
    let e = s.bulk_load(&load_spec("t", &cols, 1), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("fila 2") && m.contains("NOT NULL")), "{e:?}");
    let (_, rows) = read_all(&mut s, &ReadSpec { table: table("t"), columns: Some(vec!["id".into()]), filter: None }).await;
    assert_eq!(rows, vec![vec![Cell::Int(1)]]);

    // The connection is usable afterwards (no transaction left open).
    run(&mut s, "INSERT INTO t (id, s) VALUES (9, 'z')").await;

    // A read-only session refuses to load.
    drop(s);
    let mut ro = open(&path, true).await;
    let mut src = VecSource::new(sample_rows(), 10);
    assert!(ro.bulk_load(&load_spec("t", &cols, 10), &[], &mut src, &|_| {}).await.is_err());
    drop(ro);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn native_copy_between_files() {
    let (a, b) = (temp_db("native-a"), temp_db("native-b"));
    let mut src = open(&a, false).await;
    run(&mut src, DDL).await;
    let mut src_load = VecSource::new(sample_rows(), 10);
    let cols = ["id", "i", "r", "s", "b", "n", "d"];
    src.bulk_load(&load_spec("t", &cols, 100), &[], &mut src_load, &|_| {}).await.unwrap();
    drop(src);

    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    assert!(d.supports_bulk_load() && d.supports_native_copy("sqlite") && !d.supports_native_copy("libsql"));
    // The source opens read-only, as the migration opens it.
    let mut src = open(&a, true).await;
    let mut dst = open(&b, false).await;
    run(&mut dst, DDL).await;
    let before = std::fs::read(&a).unwrap();
    let spec = CopySpec {
        source: ReadSpec { table: table("t"), columns: None, filter: None },
        target: load_spec("t", &cols, 100),
    };
    let done = AtomicU64::new(0);
    let n = d.copy_native(&mut *src, &mut *dst, &spec, &|n| done.store(n, Ordering::SeqCst)).await.unwrap();
    assert_eq!((n, done.load(Ordering::SeqCst)), (3, 3));
    let all = ReadSpec { table: table("t"), columns: None, filter: None };
    assert_eq!(read_all(&mut src, &all).await.1, read_all(&mut dst, &all).await.1);
    assert_eq!(std::fs::read(&a).unwrap(), before, "the source file must not change");
    // Nothing stays attached.
    let mut out = QueryOutcome::default();
    dst.execute("SELECT count(*) FROM pragma_database_list WHERE name = 'dbine_copy_src'", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(0));

    // A subset into other column names.
    run(&mut dst, "CREATE TABLE u (k INTEGER, v TEXT)").await;
    let mut spec = CopySpec {
        source: ReadSpec { table: table("t"), columns: Some(vec!["id".into(), "s".into()]), filter: None },
        target: load_spec("u", &["k", "v"], 100),
    };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 3);

    // A filter would run against the target's tables: the migration reads
    // by batches instead.
    spec.source.filter = Some("id > 1".into());
    assert!(matches!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await, Err(Error::Unsupported(_))));

    // An unknown source column is an error, not the string 'nope'.
    spec.source.filter = None;
    spec.source.columns = Some(vec!["id".into(), "nope".into()]);
    let e = d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("nope")), "{e:?}");
    let u = ReadSpec { table: table("u"), columns: None, filter: None };
    assert_eq!(read_all(&mut dst, &u).await.1.len(), 3);

    // An in-memory source can't be attached: the migration falls back.
    let mut mem = open(std::path::Path::new(":memory:"), false).await;
    run(&mut mem, DDL).await;
    let spec = CopySpec { source: all.clone(), target: load_spec("t", &cols, 100) };
    assert!(matches!(d.copy_native(&mut *mem, &mut *dst, &spec, &|_| {}).await, Err(Error::Unsupported(_))));
    drop((src, dst));
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> serde_json::Value {
    let mut out = QueryOutcome::default();
    s.execute(&format!("SELECT count(*) FROM {t}"), 10, &mut out).await.unwrap();
    out.results[0].rows[0][0].clone()
}

#[tokio::test]
async fn nan_is_refused_not_nulled() {
    let path = temp_db("nan");
    let mut s = open(&path, false).await;
    run(&mut s, "CREATE TABLE f (id INTEGER, r REAL)").await;
    let rows = vec![vec![Cell::Int(1), Cell::Float(1.0)], vec![Cell::Int(2), Cell::Float(f64::NAN)]];
    let mut src = VecSource::new(rows, 10);
    let e = s.bulk_load(&load_spec("f", &["id", "r"], 100), &[], &mut src, &|_| {}).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("NaN") && m.contains("fila 2") && m.contains("\"r\"")), "{e:?}");
    assert_eq!(count(&mut s, "f").await, serde_json::json!(0));
    drop(s);
    let _ = std::fs::remove_file(&path);
}

/// `batches` batches of `per` rows (ids from 1), then never ends.
struct StallSource {
    left: usize,
    per: usize,
    next_id: i64,
}

#[dbine_driver::async_trait]
impl BatchSource for StallSource {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.left == 0 {
            return std::future::pending().await;
        }
        self.left -= 1;
        let rows = (0..self.per)
            .map(|_| {
                self.next_id += 1;
                vec![Cell::Int(self.next_id)]
            })
            .collect();
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// A dropped load (a failed read, a cancellation) commits nothing after
/// the drop, not even the windows already queued to the loading thread.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_load_commits_nothing_later() {
    let path = temp_db("drop");
    let mut s = open(&path, false).await;
    run(&mut s, "CREATE TABLE a (id INTEGER)").await;
    // Another connection holds the write lock, so the load's first window
    // waits while its batches queue up.
    let mut other = open(&path, false).await;
    run(&mut other, "BEGIN IMMEDIATE").await;
    let mut src = StallSource { left: 6, per: 200, next_id: 0 };
    let reported = Arc::new(AtomicU64::new(0));
    let r2 = reported.clone();
    let progress = move |n: u64| r2.store(n, Ordering::SeqCst);
    let spec = load_spec("a", &["id"], 200);
    let load = s.bulk_load(&spec, &[], &mut src, &progress);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(300), load).await.is_err());
    // Let the load's thread go on: it must roll back, not commit.
    run(&mut other, "COMMIT").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(count(&mut other, "a").await, serde_json::json!(0));
    assert_eq!(reported.load(Ordering::SeqCst), 0);
    // The loading connection is usable again, with no transaction open.
    run(&mut s, "INSERT INTO a VALUES (1)").await;
    assert_eq!(count(&mut other, "a").await, serde_json::json!(1));
    drop((s, other));
    let _ = std::fs::remove_file(&path);
}

/// Batches with a pause in between (the window stays open meanwhile).
struct SlowSource(Vec<RowBatch>, std::time::Duration);

#[dbine_driver::async_trait]
impl BatchSource for SlowSource {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.0.is_empty() {
            return None;
        }
        tokio::time::sleep(self.1).await;
        Some(self.0.remove(0))
    }
}

/// Tables loaded in parallel into the same file take turns on its single
/// write lock instead of failing with "database is locked" once a window
/// outlasts the busy timeout (5 s).
#[tokio::test(flavor = "multi_thread")]
async fn parallel_loads_into_one_file_take_turns() {
    let path = temp_db("parallel");
    let mut a = open(&path, false).await;
    run(&mut a, "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER)").await;
    let mut b = open(&path, false).await;
    let batch = |id: i64| RowBatch { rows: vec![vec![Cell::Int(id)]], bytes: 0 };
    // `a`'s one window stays open about 6 s; `b` starts while it's open.
    let mut slow = SlowSource(vec![batch(1), batch(2), batch(3), batch(4)], std::time::Duration::from_millis(1_500));
    let mut quick = VecSource::new(vec![vec![Cell::Int(1)]], 1);
    let spec_a = load_spec("a", &["id"], 1_000);
    let spec_b = load_spec("b", &["id"], 1_000);
    let load_a = a.bulk_load(&spec_a, &[], &mut slow, &|_| {});
    let load_b = async {
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
        b.bulk_load(&spec_b, &[], &mut quick, &|_| {}).await
    };
    let (ra, rb) = tokio::join!(load_a, load_b);
    assert_eq!((ra.unwrap(), rb.unwrap()), (4, 1));
    assert_eq!(count(&mut a, "b").await, serde_json::json!(1));
    drop((a, b));
    let _ = std::fs::remove_file(&path);
}

/// Another connection's write into the file (a finished table's indexes,
/// another table emptied) doesn't go through the loads' turns: it gets in
/// while a window waits on a slow source, instead of failing with
/// "database is locked" after its busy timeout (5 s).
#[tokio::test(flavor = "multi_thread")]
async fn outside_write_gets_in_while_a_window_waits() {
    let path = temp_db("outside");
    let mut a = open(&path, false).await;
    run(&mut a, "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER)").await;
    let mut b = open(&path, false).await;
    let batch = |id: i64| RowBatch { rows: vec![vec![Cell::Int(id)]], bytes: 0 };
    // Without a bound, `a`'s one window would stay open about 9 s.
    let mut slow = SlowSource((1..=6).map(batch).collect(), std::time::Duration::from_millis(1_500));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let c2 = commits.clone();
    let progress = move |n: u64| c2.lock().unwrap().push(n);
    let spec = load_spec("a", &["id"], 1_000);
    let load = a.bulk_load(&spec, &[], &mut slow, &progress);
    let index = async {
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
        let t = Instant::now();
        let mut out = QueryOutcome::default();
        let r = b.execute("CREATE INDEX ib ON b(id)", 10, &mut out).await;
        (r, t.elapsed())
    };
    let (loaded, (indexed, took)) = tokio::join!(load, index);
    assert_eq!(loaded.unwrap(), 6);
    assert!(indexed.is_ok(), "{indexed:?}");
    assert!(took < std::time::Duration::from_secs(4), "{took:?}");
    let commits = commits.lock().unwrap().clone();
    assert!(commits.len() > 1 && commits.last() == Some(&6), "{commits:?}");
    assert_eq!(count(&mut b, "a").await, serde_json::json!(6));
    drop((a, b));
    let _ = std::fs::remove_file(&path);
}

/// A load whose window has to begin behind another connection's write
/// that outlasts the busy timeout waits it out instead of failing.
#[tokio::test(flavor = "multi_thread")]
async fn load_waits_out_a_long_outside_write() {
    let path = temp_db("long-write");
    let mut a = open(&path, false).await;
    run(&mut a, "CREATE TABLE a (id INTEGER)").await;
    let mut other = open(&path, false).await;
    run(&mut other, "BEGIN IMMEDIATE").await;
    let mut src = VecSource::new((1..=3).map(|i| vec![Cell::Int(i)]).collect(), 1);
    let spec = load_spec("a", &["id"], 1_000);
    let load = a.bulk_load(&spec, &[], &mut src, &|_| {});
    let hold = async {
        tokio::time::sleep(std::time::Duration::from_millis(6_500)).await;
        run(&mut other, "COMMIT").await;
    };
    let (loaded, ()) = tokio::join!(load, hold);
    assert_eq!(loaded.unwrap(), 3);
    assert_eq!(count(&mut other, "a").await, serde_json::json!(3));
    drop((a, other));
    let _ = std::fs::remove_file(&path);
}

/// The native copy goes in windows by rowid ranges: the rows arrive whole
/// and another connection's write gets into the target file meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn native_copy_lets_other_writes_in() {
    let (a, b) = (temp_db("chunk-a"), temp_db("chunk-b"));
    let ddl = "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)";
    let mut src = open(&a, false).await;
    run(&mut src, ddl).await;
    // Sparse rowids (gaps and a negative one) and a column named `rowid`.
    run(&mut src, "CREATE TABLE r (rowid TEXT, v INTEGER)").await;
    run(
        &mut src,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000)
         INSERT INTO t SELECT CASE WHEN x = 1 THEN -5 ELSE x * 3 END, printf('row %08d %s', x, hex(randomblob(40))) FROM c",
    )
    .await;
    run(&mut src, "INSERT INTO r VALUES ('a', 1), ('b', 2), (NULL, 3)").await;
    let mut dst = open(&b, false).await;
    // A costly check makes the copy outlast the busy timeout (5 s) in one
    // statement.
    run(&mut dst, "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT CHECK (length(randomblob(30000)) > 0))").await;
    run(&mut dst, "CREATE TABLE r (rowid TEXT, v INTEGER); CREATE TABLE z (x INTEGER)").await;
    let mut other = open(&b, false).await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let spec = CopySpec { source: ReadSpec { table: table("t"), columns: None, filter: None }, target: load_spec("t", &["id", "s"], 0) };
    let commits = Arc::new(Mutex::new(Vec::new()));
    let c2 = commits.clone();
    let progress = move |n: u64| c2.lock().unwrap().push(n);
    let started = Instant::now();
    let copy = d.copy_native(&mut *src, &mut *dst, &spec, &progress);
    let write = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut out = QueryOutcome::default();
        other.execute("INSERT INTO z VALUES (1)", 10, &mut out).await
    };
    let (copied, wrote) = tokio::join!(copy, write);
    let took = started.elapsed();
    assert_eq!(copied.unwrap(), 20_000);
    assert!(wrote.is_ok(), "{wrote:?}");
    let commits = commits.lock().unwrap().clone();
    assert_eq!(commits.last(), Some(&20_000), "{commits:?}");
    println!("native copy of 20k rows: {took:?} in {} commits", commits.len());
    let all = ReadSpec { table: table("t"), columns: None, filter: None };
    assert_eq!(read_all(&mut src, &all).await.1, read_all(&mut dst, &all).await.1);

    // A column that takes the name `rowid` goes under `_rowid_`.
    let spec = CopySpec { source: ReadSpec { table: table("r"), columns: None, filter: None }, target: load_spec("r", &["rowid", "v"], 0) };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 3);
    let r = ReadSpec { table: table("r"), columns: None, filter: None };
    assert_eq!(read_all(&mut src, &r).await.1, read_all(&mut dst, &r).await.1);
    // An empty table copies nothing.
    run(&mut src, "CREATE TABLE e (x INTEGER)").await;
    run(&mut dst, "CREATE TABLE e (x INTEGER)").await;
    let spec = CopySpec { source: ReadSpec { table: table("e"), columns: None, filter: None }, target: load_spec("e", &["x"], 0) };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 0);
    drop((src, dst, other));
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// A source without rowid ranges (a view, a WITHOUT ROWID table) goes in
/// one statement; one that would keep the target locked too long is
/// interrupted, leaves nothing and falls back to batches.
#[tokio::test(flavor = "multi_thread")]
async fn native_copy_without_rowid_is_bounded() {
    let (a, b) = (temp_db("norowid-a"), temp_db("norowid-b"));
    let mut src = open(&a, false).await;
    run(&mut src, "CREATE TABLE w (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID; INSERT INTO w VALUES ('x', 1), ('y', 2)").await;
    run(
        &mut src,
        "CREATE VIEW huge AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000000) SELECT x FROM c",
    )
    .await;
    let mut dst = open(&b, false).await;
    run(&mut dst, "CREATE TABLE w (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID; CREATE TABLE h (x INTEGER)").await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let spec = CopySpec { source: ReadSpec { table: table("w"), columns: None, filter: None }, target: load_spec("w", &["k", "v"], 0) };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 2);

    let spec = CopySpec { source: ReadSpec { table: table("huge"), columns: None, filter: None }, target: load_spec("h", &["x"], 0) };
    let t = Instant::now();
    let r = d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("vista")), "{r:?}");
    assert!(t.elapsed() < std::time::Duration::from_secs(4), "{:?}", t.elapsed());
    assert_eq!(count(&mut dst, "h").await, serde_json::json!(0));
    // No transaction left open.
    run(&mut dst, "INSERT INTO h VALUES (1)").await;
    drop((src, dst));
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// Batches of 500 rows, as fast as they're taken, until `until`.
struct TimedSource {
    until: Instant,
    next: i64,
}

#[dbine_driver::async_trait]
impl BatchSource for TimedSource {
    async fn next(&mut self) -> Option<RowBatch> {
        if Instant::now() >= self.until {
            return None;
        }
        let rows = (self.next..self.next + 500).map(|i| vec![Cell::Int(i)]).collect();
        self.next += 500;
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// Several outside writes waiting at once while parallel loads keep the
/// file busy all get in. SQLite's busy handler isn't first come, first
/// served: a load that took the file back right after each of them would
/// keep the next one waiting a whole [`HOLD`] more, past its busy timeout
/// (5 s), and it would fail with "database is locked".
#[tokio::test(flavor = "multi_thread")]
async fn queued_outside_writes_all_get_in() {
    use std::time::Duration;
    let path = temp_db("queued");
    let mut setup = open(&path, false).await;
    run(&mut setup, "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER); CREATE TABLE c (id INTEGER); CREATE TABLE z (k INTEGER)").await;
    let (mut la, mut lb, mut lc) = (open(&path, false).await, open(&path, false).await, open(&path, false).await);
    let mut w = Vec::new();
    for _ in 0..6 {
        w.push(open(&path, false).await);
    }
    let until = Instant::now() + Duration::from_secs(9);
    let (mut sa, mut sb, mut sc) = (TimedSource { until, next: 0 }, TimedSource { until, next: 0 }, TimedSource { until, next: 0 });
    let (pa, pb, pc) = (load_spec("a", &["id"], 10_000_000), load_spec("b", &["id"], 10_000_000), load_spec("c", &["id"], 10_000_000));
    // All asked for at once; each holds the file 150 ms once it gets it
    // (an index on a small table, a table emptied).
    let write = |mut s: Box<dyn Session>, k: i64| async move {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let t = Instant::now();
        let mut out = QueryOutcome::default();
        let r = s.execute("BEGIN IMMEDIATE", 10, &mut out).await;
        let waited = t.elapsed();
        if r.is_ok() {
            s.execute(&format!("INSERT INTO z VALUES ({k})"), 10, &mut out).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            s.execute("COMMIT", 10, &mut out).await.unwrap();
        }
        (r.map(|_| ()), waited)
    };
    let mut w = w.into_iter();
    let mut next = || w.next().unwrap();
    let writes = async {
        let (x1, x2, x3, x4, x5, x6) =
            tokio::join!(write(next(), 1), write(next(), 2), write(next(), 3), write(next(), 4), write(next(), 5), write(next(), 6));
        [x1, x2, x3, x4, x5, x6]
    };
    let (ra, rb, rc, xs) = tokio::join!(
        la.bulk_load(&pa, &[], &mut sa, &|_| {}),
        lb.bulk_load(&pb, &[], &mut sb, &|_| {}),
        lc.bulk_load(&pc, &[], &mut sc, &|_| {}),
        writes
    );
    println!("outside writes waited {:?}", xs.iter().map(|x| x.1).collect::<Vec<_>>());
    for (i, x) in xs.iter().enumerate() {
        assert!(x.0.is_ok(), "outside write {} after {:?}: {:?}", i + 1, x.1, x.0);
    }
    for (t, r) in [("a", ra), ("b", rb), ("c", rc)] {
        let n = r.unwrap();
        assert!(n > 0);
        assert_eq!(count(&mut setup, t).await, serde_json::json!(n));
    }
    assert_eq!(count(&mut setup, "z").await, serde_json::json!(6));
    drop((setup, la, lb, lc));
    let _ = std::fs::remove_file(&path);
}

/// A filtered copy inside one file goes natively: on the target's
/// connection the filter's names resolve as on the source's. By batches,
/// the read's `SELECT` would keep the file shared-locked while the load
/// waits to commit, which in rollback-journal mode never happens.
#[tokio::test]
async fn same_file_filtered_copy_is_native() {
    let path = temp_db("same-filter");
    let mut src = open(&path, false).await;
    run(
        &mut src,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE TABLE keep (id INTEGER);
         CREATE TABLE u (id INTEGER, v TEXT); CREATE TABLE w (id INTEGER, v TEXT); CREATE VIEW tv AS SELECT id, v FROM t;
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000) INSERT INTO t SELECT x, 'v' || x FROM c;
         INSERT INTO keep VALUES (7), (4001)",
    )
    .await;
    let mut dst = open(&path, false).await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let filter = Some("id % 2 = 0 OR id IN (SELECT id FROM keep)".to_string());
    let read = ReadSpec { table: table("t"), columns: None, filter: filter.clone() };
    let spec = CopySpec { source: read.clone(), target: load_spec("u", &["id", "v"], 0) };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 2_502);
    let u = ReadSpec { table: table("u"), columns: None, filter: None };
    assert_eq!(read_all(&mut dst, &u).await.1, read_all(&mut src, &read).await.1);
    // A view goes in one statement, filtered too.
    let spec = CopySpec {
        source: ReadSpec { table: table("tv"), columns: None, filter: Some("id <= 10".into()) },
        target: load_spec("w", &["id", "v"], 0),
    };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 10);
    // What isn't one condition goes by batches.
    let spec = CopySpec {
        source: ReadSpec { table: table("t"), columns: None, filter: Some("id > 1 LIMIT 5".into()) },
        target: load_spec("w", &["id", "v"], 0),
    };
    let r = d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(_))), "{r:?}");
    assert_eq!(count(&mut dst, "w").await, serde_json::json!(10));
    drop((src, dst));
    let _ = std::fs::remove_file(&path);
}

/// A read by batches feeding a load, as the migration pipes them.
struct Pipe(tokio::sync::mpsc::Sender<RowBatch>);

impl BatchSink for Pipe {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0.blocking_send(b).map_err(|_| std::io::Error::other("la carga terminó"))
    }
}

struct PipeSource(tokio::sync::mpsc::Receiver<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for PipeSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.recv().await
    }
}

/// 200k rows read by batches from one table and loaded into another of the
/// same file: the load's result and the rows it left.
async fn same_file_by_batches(wal: bool) -> (dbine_driver::Result<u64>, serde_json::Value) {
    let path = temp_db(if wal { "pipe-wal" } else { "pipe-del" });
    let mut src = open(&path, false).await;
    if wal {
        run(&mut src, "PRAGMA journal_mode = WAL").await;
    }
    run(
        &mut src,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE TABLE u (id INTEGER, v TEXT);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 200000) INSERT INTO t SELECT x, printf('row %08d', x) FROM c",
    )
    .await;
    let dst = open(&path, false).await;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let sink: dbine_driver::transfer::BatchSinkRef = Arc::new(Mutex::new(Pipe(tx)));
    let spec = ReadSpec { table: table("t"), columns: None, filter: None };
    let read = src.read_batches(&spec, sink);
    let load = async move {
        let mut dst = dst;
        let mut source = PipeSource(rx);
        let r = dst.bulk_load(&load_spec("u", &["id", "v"], 20_000), &[], &mut source, &|_| {}).await;
        drop(source);
        (r, dst)
    };
    let (_, (loaded, mut dst)) = tokio::join!(read, load);
    let n = count(&mut dst, "u").await;
    drop((src, dst));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    (loaded, n)
}

/// In rollback-journal mode the load can't commit while the read of the
/// same file is open: it fails saying why (and how to avoid it), leaving
/// nothing. In WAL mode it goes through.
#[tokio::test(flavor = "multi_thread")]
async fn same_file_by_batches_fails_clearly() {
    let (r, n) = same_file_by_batches(false).await;
    assert!(matches!(&r, Err(Error::Query(m)) if m.contains("WAL")), "{r:?}");
    assert_eq!(n, serde_json::json!(0));
    let (r, n) = same_file_by_batches(true).await;
    assert_eq!(r.unwrap(), 200_000);
    assert_eq!(n, serde_json::json!(200_000));
}

/// A source file SQLite can't attach (here: gone from its path while the
/// source session still has it open) is read by batches instead.
#[tokio::test]
async fn unattachable_source_falls_back() {
    let (a, b) = (temp_db("bad-src"), temp_db("bad-dst"));
    let mut src = open(&a, false).await;
    run(&mut src, "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)").await;
    std::fs::remove_file(&a).unwrap();
    let mut dst = open(&b, false).await;
    run(&mut dst, "CREATE TABLE t (id INTEGER)").await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let spec = CopySpec { source: ReadSpec { table: table("t"), columns: None, filter: None }, target: load_spec("t", &["id"], 100) };
    let r = d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await;
    assert!(matches!(&r, Err(Error::Unsupported(m)) if m.contains("adjuntar")), "{r:?}");
    assert_eq!(read_all(&mut src, &spec.source).await.1, vec![vec![Cell::Int(1)]]);
    drop((src, dst));
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// 1M rows (100k in debug builds) of five mixed columns: the load's rate,
/// the read's rate and the native copy's, each checked by count.
#[tokio::test]
async fn load_benchmark() {
    let n: i64 = if cfg!(debug_assertions) { 100_000 } else { 1_000_000 };
    let rows: Vec<Vec<Cell>> = (0..n)
        .map(|i| {
            vec![Cell::Int(i), Cell::Int(i * 7), Cell::Float(i as f64 / 3.0), Cell::Text(format!("row {i:08}")), Cell::Bytes(vec![(i % 256) as u8; 16])]
        })
        .collect();
    let ddl = "CREATE TABLE big (id INTEGER PRIMARY KEY, a INTEGER, f REAL, s TEXT, b BLOB)";
    let cols = ["id", "a", "f", "s", "b"];
    let (a, b) = (temp_db("bench-a"), temp_db("bench-b"));
    let mut s = open(&a, false).await;
    run(&mut s, ddl).await;
    let mut src = VecSource::new(rows, 1_000);
    let t = Instant::now();
    let loaded = s.bulk_load(&load_spec("big", &cols, LoadSpec::DEFAULT_COMMIT_ROWS), &[], &mut src, &|_| {}).await.unwrap();
    let load = t.elapsed();
    assert_eq!(loaded, n as u64);

    let t = Instant::now();
    let (_, back) = read_all(&mut s, &ReadSpec { table: table("big"), columns: None, filter: None }).await;
    let read = t.elapsed();
    assert_eq!(back.len(), n as usize);
    assert_eq!(back[12_345], vec![Cell::Int(12_345), Cell::Int(86_415), Cell::Float(12_345.0 / 3.0), Cell::Text("row 00012345".into()), Cell::Bytes(vec![(12_345 % 256) as u8; 16])]);
    drop(back);

    let mut dst = open(&b, false).await;
    run(&mut dst, ddl).await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let spec = CopySpec { source: ReadSpec { table: table("big"), columns: None, filter: None }, target: load_spec("big", &cols, 0) };
    let t = Instant::now();
    assert_eq!(d.copy_native(&mut *s, &mut *dst, &spec, &|_| {}).await.unwrap(), n as u64);
    let native = t.elapsed();
    let rate = |d: std::time::Duration| (n as f64 / d.as_secs_f64()) as u64;
    println!(
        "sqlite {n} rows: bulk_load {:?} ({} rows/s), read_batches {:?} ({} rows/s), copy_native {:?} ({} rows/s)",
        load,
        rate(load),
        read,
        rate(read),
        native,
        rate(native)
    );
    drop((s, dst));
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[tokio::test]
async fn native_copy_leaves_computed_columns_to_the_engine() {
    // `SELECT *` would bring the computed column too: one value more than
    // the INSERT takes.
    let path = temp_db("computed");
    let mut src = open(&path, false).await;
    run(
        &mut src,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, m INTEGER, doble INTEGER GENERATED ALWAYS AS (m * 2) VIRTUAL);
         CREATE TABLE u (id INTEGER PRIMARY KEY, m INTEGER, doble INTEGER GENERATED ALWAYS AS (m * 2) VIRTUAL);
         INSERT INTO t (id, m) VALUES (1, 5), (2, 7)",
    )
    .await;
    let mut dst = open(&path, false).await;
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let spec = CopySpec { source: ReadSpec { table: table("t"), columns: Some(vec!["id".into(), "m".into()]), filter: None }, target: load_spec("u", &["id", "m"], 0) };
    assert_eq!(d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap(), 2);
    let all = |t: &str| ReadSpec { table: table(t), columns: Some(vec!["id".into(), "m".into(), "doble".into()]), filter: None };
    assert_eq!(read_all(&mut dst, &all("u")).await.1, read_all(&mut src, &all("t")).await.1);
    drop((src, dst));
    let _ = std::fs::remove_file(&path);
}

//! Bulk transfer against real servers (ignored by default):
//!
//! ```sh
//! cargo test -p dbine-driver-postgres --test transfer -- --ignored transfer --nocapture
//! cargo test --release -p dbine-driver-postgres --test transfer -- --ignored transfer_benchmark --nocapture
//! ```
//!
//! PostgreSQL: `DBINE_TEST_POSTGRES_URL`, by default the `dbine-test-postgres`
//! container (`postgres://postgres:pw@localhost:25010/postgres`). It creates
//! and drops the databases `dbine_xfer_<test>_src` and `dbine_xfer_<test>_dst`.
//! CockroachDB: `DBINE_TEST_COCKROACH_URL`, by default `dbine-test-cockroach`.

use dbine_driver::transfer::{BatchSink, BatchSource, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let out = run(s, sql).await;
    out.results[0].rows[0][0].as_str().map(str::to_string).unwrap_or_else(|| out.results[0].rows[0][0].to_string())
}

/// Batches into a bounded channel (blocking the reader when it's full).
struct ChannelSink(mpsc::Sender<RowBatch>);
impl BatchSink for ChannelSink {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        let tx = self.0.clone();
        tokio::task::block_in_place(|| tx.blocking_send(b)).map_err(|_| io::Error::other("closed"))
    }
}

struct ChannelSource(mpsc::Receiver<RowBatch>);
#[dbine_driver::async_trait]
impl BatchSource for ChannelSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.recv().await
    }
}

fn obj(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("public".into()), name: name.into() }
}

/// read_batches on `src` piped into bulk_load on `dst`; the rows loaded.
async fn read_and_load(src: Box<dyn Session>, dst: Box<dyn Session>, from: &str, to: &str, columns: Vec<String>) -> (u64, Box<dyn Session>, Box<dyn Session>) {
    let (tx, rx) = mpsc::channel(16);
    let read = ReadSpec { table: obj(from), columns: Some(columns.clone()), filter: None };
    let load = LoadSpec {
        table: obj(to),
        columns,
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let mut src = src;
    let reader = tokio::spawn(async move {
        let sink: dbine_driver::BatchSinkRef = Arc::new(Mutex::new(ChannelSink(tx)));
        let n = src.read_batches(&read, sink).await.expect("read_batches");
        (n, src)
    });
    let mut dst = dst;
    let loader = tokio::spawn(async move {
        let mut source = ChannelSource(rx);
        let n = dst.bulk_load(&load, &[], &mut source, &|_| {}).await.expect("bulk_load");
        (n, dst)
    });
    let (read, src) = reader.await.unwrap();
    let (loaded, dst) = loader.await.unwrap();
    assert_eq!(read, loaded);
    (loaded, src, dst)
}

async fn native(d: &Arc<dyn Driver>, src: &mut Box<dyn Session>, dst: &mut Box<dyn Session>, from: &str, to: &str, columns: Vec<String>) -> u64 {
    let spec = CopySpec {
        source: ReadSpec { table: obj(from), columns: Some(columns.clone()), filter: None },
        target: LoadSpec {
            table: obj(to),
            columns,
            table_lock: false,
            keep_identity: true,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        },
    };
    d.copy_native(src.as_mut(), dst.as_mut(), &spec, &|_| {}).await.expect("copy_native")
}

struct Pg {
    d: Arc<dyn Driver>,
    admin: Box<dyn Session>,
    src: Box<dyn Session>,
    dst: Box<dyn Session>,
    password: String,
    tag: String,
}

async fn setup(tag: &str) -> Pg {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010/postgres".into());
    let cfg = parse_url("postgres", &url);
    let d = driver("postgres");
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let (src_db, dst_db) = (format!("dbine_xfer_{tag}_src"), format!("dbine_xfer_{tag}_dst"));
    for db in [&src_db, &dst_db] {
        run(&mut admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
        run(&mut admin, &format!("CREATE DATABASE {db}")).await;
    }
    let src = d.connect(&cfg, Some(&src_db)).await.unwrap();
    let mut dst = d.connect(&cfg, Some(&dst_db)).await.unwrap();
    run(&mut dst, "CREATE EXTENSION dblink").await;
    Pg { d, admin, src, dst, password: cfg.password.unwrap_or_default(), tag: tag.to_string() }
}

async fn teardown(mut pg: Pg) {
    drop(pg.src);
    drop(pg.dst);
    for db in [format!("dbine_xfer_{}_src", pg.tag), format!("dbine_xfer_{}_dst", pg.tag)] {
        run(&mut pg.admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
    }
}

/// Rows of `a` (target) missing in the source's `b` and the other way round.
async fn except_both_ways(pg: &mut Pg, table: &str, select: &str, record: &str) -> (i64, i64) {
    let remote = format!("dblink('dbname=dbine_xfer_{}_src user=postgres password={}', $q${select} FROM public.src$q$) AS r({record})", pg.tag, pg.password);
    let a = scalar(&mut pg.dst, &format!("SELECT count(*) FROM ({select} FROM public.{table} EXCEPT ALL SELECT * FROM {remote}) x")).await;
    let b = scalar(&mut pg.dst, &format!("SELECT count(*) FROM (SELECT * FROM {remote} EXCEPT ALL {select} FROM public.{table}) x")).await;
    (a.parse().unwrap(), b.parse().unwrap())
}

const TYPES: &str = "id serial PRIMARY KEY, b bool, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8, n numeric(38,10), n_free numeric,
    t text, vc varchar(40), c char(5), by bytea, u uuid, d date, tm time, ts timestamp, tstz timestamptz, j json, jb jsonb,
    arr int4[], iv interval, m money";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_postgres_all_types() {
    let mut pg = setup("types").await;
    run(&mut pg.src, &format!("CREATE TABLE src ({TYPES})")).await;
    run(
        &mut pg.src,
        "INSERT INTO src (b, i2, i4, i8, f4, f8, n, n_free, t, vc, c, by, u, d, tm, ts, tstz, j, jb, arr, iv, m) VALUES
         (true, -32768, -2147483648, -9223372036854775808, 1.5, 3.141592653589793, 9999999999999999999999999999.9999999999, 'NaN',
          'ñandú\t\\ \"q\"', 'x', 'ab', '\\x00ff10', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '0044-03-15 BC', '23:59:59.999999',
          '2262-04-11 23:47:16.854775', '2024-05-01 12:00:00.123456-03', '{\"a\": [1, 2.50]}', '{\"b\": null}', '{1,NULL,3}', '1 year 2 mons 3 days 04:05:06.7', 12.34),
         (false, 32767, 2147483647, 9223372036854775807, 'Infinity', '-1e-300', -9999999999999999999999999999.9999999999, '0.000000000000000000001',
          '', '', '', '', '00000000-0000-0000-0000-000000000000', 'infinity', '00:00:00', '-infinity', 'infinity', '[]', '[]', '{}', '-1 day', -0.01),
         (NULL, NULL, NULL, NULL, NULL, NULL, 0.0000000001, 123456789012345678901234567890.123456789, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
          '1999-12-31 23:59:59', '1970-01-01 00:00:00+00', NULL, NULL, NULL, NULL, NULL),
         (NULL, NULL, NULL, NULL, 'NaN', 'NaN', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO src (by, t) SELECT decode(repeat(md5(g::text), 328000), 'hex'), 'big' FROM generate_series(1, 1) g;
         INSERT INTO src (i4, n, tstz) SELECT g, g * 1.0000000001, '2000-01-01'::timestamptz + g * interval '1.000001 second' FROM generate_series(1, 5000) g;",
    )
    .await;
    let big = scalar(&mut pg.src, "SELECT length(by) FROM src WHERE t = 'big'").await;
    eprintln!("bytea row: {big} bytes");
    assert!(big.parse::<i64>().unwrap() >= 5_000_000);
    for t in ["via_batches", "via_binary", "via_native"] {
        run(&mut pg.dst, &format!("CREATE TABLE {t} ({TYPES})")).await;
    }
    let cols: Vec<String> =
        "id b i2 i4 i8 f4 f8 n n_free t vc c by u d tm ts tstz j jb arr iv m".split(' ').map(str::to_string).collect();

    // All the columns: arrays, interval and money make the load use COPY's
    // text format. Without them it's binary.
    let Pg { d, admin, src, dst, password, tag } = pg;
    let (n, src, dst) = read_and_load(src, dst, "src", "via_batches", cols.clone()).await;
    eprintln!("read_batches → bulk_load (text COPY): {n} rows");
    let (nb, src, dst) = read_and_load(src, dst, "src", "via_binary", cols[..20].to_vec()).await;
    eprintln!("read_batches → bulk_load (binary COPY): {nb} rows");
    assert_eq!(n, nb);
    let mut pg = Pg { d, admin, src, dst, password, tag };
    let d = pg.d.clone();
    // money's binary value depends on the server's lc_monetary: the native
    // copy refuses it (see transfer_postgres_native_refuses_server_bound_types).
    let n2 = native(&d, &mut pg.src, &mut pg.dst, "src", "via_native", cols[..22].to_vec()).await;
    eprintln!("copy_native: {n2} rows");
    assert_eq!(n, n2);

    // json has no equality: compared as text.
    let select = "SELECT id, b, i2, i4, i8, f4, f8, n, n_free, t, vc, c, by, u, d, tm, ts, tstz, j::text, jb, arr, iv, m";
    let record = "id int, b bool, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8, n numeric, n_free numeric, t text, vc varchar, c char(5), \
                  by bytea, u uuid, d date, tm time, ts timestamp, tstz timestamptz, j text, jb jsonb, arr int4[], iv interval, m money";
    let (a, b) = except_both_ways(&mut pg, "via_batches", select, record).await;
    eprintln!("via_batches: EXCEPT target→source {a}, source→target {b}");
    assert_eq!((a, b), (0, 0), "via_batches");
    let (select_native, record_native) = (select.trim_end_matches(", m"), record.trim_end_matches(", m money"));
    let (a, b) = except_both_ways(&mut pg, "via_native", select_native, record_native).await;
    eprintln!("via_native: EXCEPT target→source {a}, source→target {b}");
    assert_eq!((a, b), (0, 0), "via_native");
    let select = "SELECT id, b, i2, i4, i8, f4, f8, n, n_free, t, vc, c, by, u, d, tm, ts, tstz, j::text, jb";
    let record = "id int, b bool, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8, n numeric, n_free numeric, t text, vc varchar, c char(5), \
                  by bytea, u uuid, d date, tm time, ts timestamp, tstz timestamptz, j text, jb jsonb";
    let (a, b) = except_both_ways(&mut pg, "via_binary", select, record).await;
    eprintln!("via_binary: EXCEPT target→source {a}, source→target {b}");
    assert_eq!((a, b), (0, 0), "via_binary");
    teardown(pg).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_benchmark_1m() {
    let mut pg = setup("bench").await;
    let def = "id bigint, n int, amount numeric(12,2), name text, ts timestamptz, flag bool, u uuid";
    run(&mut pg.src, &format!("CREATE TABLE src ({def})")).await;
    run(
        &mut pg.src,
        "INSERT INTO src SELECT g, g % 1000, g / 100.0, 'name ' || g, '2020-01-01'::timestamptz + g * interval '1 second', g % 2 = 0,
         md5(g::text)::uuid FROM generate_series(1, 1000000) g",
    )
    .await;
    for t in ["via_batches", "via_native"] {
        run(&mut pg.dst, &format!("CREATE TABLE {t} ({def})")).await;
    }
    let cols: Vec<String> = "id n amount name ts flag u".split(' ').map(str::to_string).collect();
    let Pg { d, admin, src, dst, password, tag } = pg;
    let t0 = Instant::now();
    let (n, src, dst) = read_and_load(src, dst, "src", "via_batches", cols.clone()).await;
    let e1 = t0.elapsed().as_secs_f64();
    let mut pg = Pg { d, admin, src, dst, password, tag };
    let d = pg.d.clone();
    let t0 = Instant::now();
    let n2 = native(&d, &mut pg.src, &mut pg.dst, "src", "via_native", cols).await;
    let e2 = t0.elapsed().as_secs_f64();
    eprintln!("read_batches → bulk_load: {n} rows in {e1:.2} s = {:.0} rows/s", n as f64 / e1);
    eprintln!("copy_native:             {n2} rows in {e2:.2} s = {:.0} rows/s", n2 as f64 / e2);
    let select = "SELECT id, n, amount, name, ts, flag, u";
    let record = "id bigint, n int, amount numeric, name text, ts timestamptz, flag bool, u uuid";
    for t in ["via_batches", "via_native"] {
        let (a, b) = except_both_ways(&mut pg, t, select, record).await;
        eprintln!("{t}: EXCEPT target→source {a}, source→target {b}");
        assert_eq!((a, b), (0, 0), "{t}");
    }
    teardown(pg).await;
}

/// A source session whose text output doesn't parse the same elsewhere:
/// DMY dates, 15-digit floats (the pre-12 default), SQL-standard
/// intervals. The read must override all of it.
async fn unportable_text_output(s: &mut Box<dyn Session>) {
    for set in ["SET DateStyle = 'SQL, DMY'", "SET extra_float_digits = 0", "SET IntervalStyle = sql_standard"] {
        run(s, set).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_postgres_text_output_and_far_dates() {
    use dbine_driver::Cell;
    let mut pg = setup("textout").await;
    let def = "id int, d date, ts timestamp, tstz timestamptz, fa float8[], da date[], iv interval, f8 float8";
    run(&mut pg.src, &format!("CREATE TABLE src ({def})")).await;
    run(
        &mut pg.src,
        "INSERT INTO src VALUES
         (1, '10000-01-01', '10000-01-01 10:00:00', '294276-12-31 23:59:59+00', ARRAY[0.1::float8 + 0.2], ARRAY['2024-04-03'::date, '2024-12-31'],
          '-1 day +02:00:00', 0.1::float8 + 0.2),
         (2, '0044-03-15 BC', '9999-12-31 23:59:59', NULL, NULL, NULL, NULL, NULL)",
    )
    .await;
    run(&mut pg.dst, &format!("CREATE TABLE via_batches ({def})")).await;

    unportable_text_output(&mut pg.src).await;
    let read = ReadSpec { table: obj("src"), columns: None, filter: Some("id = 1".into()) };
    let sink = Arc::new(Mutex::new(Collect(Vec::new())));
    assert_eq!(pg.src.read_batches(&read, sink.clone()).await.expect("read_batches"), 1);
    let row = sink.lock().unwrap().0[0].rows[0].clone();
    eprintln!("row: {row:?}");
    assert_eq!(row[1], Cell::Text("10000-01-01".into()), "a date after 9999 stays AD");
    assert_eq!(row[2], Cell::Text("10000-01-01 10:00:00".into()));
    assert_eq!(row[3], Cell::Text("294276-12-31 23:59:59+00:00".into()));
    assert_eq!(row[4], Cell::Text("{0.30000000000000004}".into()), "every float digit");
    assert_eq!(row[5], Cell::Text("{2024-04-03,2024-12-31}".into()), "ISO dates");
    assert_eq!(row[6], Cell::Text("-1 days +02:00:00".into()), "postgres interval style");
    assert_eq!(row[7], Cell::Float(0.1 + 0.2));

    unportable_text_output(&mut pg.src).await;
    let cols: Vec<String> = "id d ts tstz fa da iv f8".split(' ').map(str::to_string).collect();
    let Pg { d, admin, src, dst, password, tag } = pg;
    let (n, src, dst) = read_and_load(src, dst, "src", "via_batches", cols).await;
    assert_eq!(n, 2);
    let mut pg = Pg { d, admin, src, dst, password, tag };
    let (a, b) = except_both_ways(
        &mut pg,
        "via_batches",
        "SELECT id, d, ts, tstz, fa, da, iv, f8",
        "id int, d date, ts timestamp, tstz timestamptz, fa float8[], da date[], iv interval, f8 float8",
    )
    .await;
    assert_eq!((a, b), (0, 0));
    teardown(pg).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_postgres_native_refuses_server_bound_types() {
    let mut pg = setup("bound").await;
    for (table, def) in [("m", "id int, m money"), ("ma", "id int, m money[]"), ("r", "id int, r regclass"), ("o", "id int, o oid")] {
        run(&mut pg.src, &format!("CREATE TABLE {table} ({def})")).await;
        run(&mut pg.dst, &format!("CREATE TABLE {table} ({def})")).await;
        let columns: Vec<String> = def.split(", ").map(|c| c.split(' ').next().unwrap().to_string()).collect();
        let spec = CopySpec {
            source: ReadSpec { table: obj(table), columns: Some(columns.clone()), filter: None },
            target: LoadSpec {
                table: obj(table),
                columns,
                table_lock: false,
                keep_identity: true,
                commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
            },
        };
        let r = pg.d.copy_native(pg.src.as_mut(), pg.dst.as_mut(), &spec, &|_| {}).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{def}: {r:?}");
    }
    teardown(pg).await;
}

/// copy_native from `src` (id, v) into `to`, in windows of `commit_rows`:
/// its result and every progress report.
async fn native_windows(pg: &mut Pg, to: &str, filter: Option<&str>, commit_rows: u64) -> (dbine_driver::Result<u64>, Vec<u64>) {
    let columns = vec!["id".to_string(), "v".to_string()];
    let spec = CopySpec {
        source: ReadSpec { table: obj("src"), columns: Some(columns.clone()), filter: filter.map(Into::into) },
        target: LoadSpec {
            table: obj(to),
            columns,
            table_lock: false,
            keep_identity: true,
            commit_rows,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        },
    };
    let seen = Mutex::new(Vec::new());
    let d = pg.d.clone();
    let r = d.copy_native(pg.src.as_mut(), pg.dst.as_mut(), &spec, &|n| seen.lock().unwrap().push(n)).await;
    (r, seen.into_inner().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_postgres_native_commit_windows() {
    let mut pg = setup("windows").await;
    run(&mut pg.src, "CREATE TABLE src (id int, v int)").await;
    run(&mut pg.src, "INSERT INTO src SELECT g, CASE WHEN g = 25000 THEN NULL ELSE g END FROM generate_series(1, 50000) g").await;
    for t in ["w21", "w20", "wall"] {
        run(&mut pg.dst, &format!("CREATE TABLE {t} (id int, v int)")).await;
    }
    run(&mut pg.dst, "CREATE TABLE strict (id int, v int NOT NULL)").await;

    // A last window that ends right at the trailer, and one that doesn't.
    let (r, seen) = native_windows(&mut pg, "w21", Some("id <= 21"), 7).await;
    assert_eq!((r.unwrap(), seen), (21, vec![7, 14, 21, 21]));
    let (r, seen) = native_windows(&mut pg, "w20", Some("id <= 20"), 7).await;
    assert_eq!((r.unwrap(), seen), (20, vec![7, 14, 20]));
    for (t, n) in [("w21", "21"), ("w20", "20")] {
        assert_eq!(scalar(&mut pg.dst, &format!("SELECT count(DISTINCT id)::text FROM {t}")).await, n);
    }
    let (r, seen) = native_windows(&mut pg, "wall", None, 1000).await;
    assert_eq!(r.unwrap(), 50_000);
    assert_eq!(seen.len(), 51);
    let (a, b) = except_both_ways(&mut pg, "wall", "SELECT id, v", "id int, v int").await;
    assert_eq!((a, b), (0, 0));

    // The target refuses row 25000: the copy stops at that window, with the
    // windows before it committed and reported (not the whole table sent).
    let (r, seen) = native_windows(&mut pg, "strict", None, 1000).await;
    eprintln!("strict: {r:?}, progress {seen:?}");
    assert!(r.is_err());
    assert_eq!(scalar(&mut pg.dst, "SELECT count(*)::text FROM strict").await, "24000");
    assert_eq!(seen.last(), Some(&24_000));
    teardown(pg).await;
}

/// Batches kept in memory.
struct Collect(Vec<RowBatch>);
impl BatchSink for Collect {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.0.push(b);
        Ok(())
    }
}

/// CockroachDB: binary COPY TO is refused, so the read falls back to rows
/// through the extended protocol (same binary values). It has no bulk load
/// here: its COPY FROM only works over the simple protocol.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_cockroach() {
    use dbine_driver::Cell;
    let url = std::env::var("DBINE_TEST_COCKROACH_URL").unwrap_or_else(|_| "postgres://root@localhost:26014/defaultdb".into());
    let cfg = parse_url("cockroachdb", &url);
    let d = driver("cockroachdb");
    assert!(!d.supports_bulk_load());
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, "DROP TABLE IF EXISTS dbine_xfer_src").await;
    run(
        &mut s,
        "CREATE TABLE dbine_xfer_src (id int PRIMARY KEY, b bool, i8 int8, f8 float8, n numeric(38,10), t text, by bytea, u uuid,
           d date, tm time, ts timestamp, tstz timestamptz, jb jsonb, arr int[], iv interval)",
    )
    .await;
    run(
        &mut s,
        "INSERT INTO dbine_xfer_src VALUES
         (1, true, -9223372036854775808, 1.5, 9999999999999999999999999999.9999999999, e'tab\\there', '\\x00ff', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',
          '2024-02-29', '23:59:59.999999', '2024-05-01 12:00:00.123456', '2024-05-01 12:00:00.123456-03', '{\"a\": 1}', ARRAY[1,2], '1 day 02:00:00'),
         (2, NULL, NULL, NULL, -0.0000000001, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO dbine_xfer_src (id, n, t) SELECT g, g * 1.5, 'r' || g::STRING FROM generate_series(3, 3000) g;",
    )
    .await;
    let read = ReadSpec {
        table: ObjectRef { kind: "table".into(), schema: Some("public".into()), name: "dbine_xfer_src".into() },
        columns: None,
        filter: Some("id <= 2 OR id > 1000".into()),
    };
    let sink = Arc::new(Mutex::new(Collect(Vec::new())));
    let n = s.read_batches(&read, sink.clone()).await.expect("read_batches");
    assert_eq!(n, 2002);
    let batches = std::mem::take(&mut sink.lock().unwrap().0);
    let mut rows: Vec<&Vec<Cell>> = batches.iter().flat_map(|b| b.rows.iter()).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    let expect = vec![
        Cell::Int(1),
        Cell::Bool(true),
        Cell::Int(i64::MIN),
        Cell::Float(1.5),
        Cell::Decimal("9999999999999999999999999999.9999999999".into()),
        Cell::Text("tab\there".into()),
        Cell::Bytes(vec![0, 255]),
        Cell::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
        Cell::Date("2024-02-29".into()),
        Cell::Time("23:59:59.999999".into()),
        Cell::DateTime("2024-05-01 12:00:00.123456".into()),
        Cell::DateTimeTz("2024-05-01 15:00:00.123456+00:00".into()),
        Cell::Json("{\"a\": 1}".into()),
        Cell::Text("{1,2}".into()),
        Cell::Text("1 day 02:00:00".into()),
    ];
    eprintln!("cockroach row 1: {:?}", rows[0]);
    assert_eq!(rows[0], &expect);
    assert_eq!(rows[1][4], Cell::Decimal("-0.0000000001".into()));
    assert_eq!(rows[1][1], Cell::Null);
    run(&mut s, "DROP TABLE dbine_xfer_src").await;
}

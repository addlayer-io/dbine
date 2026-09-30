//! Sync by rows against a real server (ignored by default):
//!
//! ```sh
//! cargo test --release -p dbine-driver-postgres --test delta -- --ignored delta --nocapture
//! ```
//!
//! `DBINE_TEST_POSTGRES_URL`, by default the `dbine-test-postgres` container
//! (`postgres://postgres:pw@localhost:25010/postgres`). It creates and drops
//! the database `dbine_delta`; source and target are the schemas `s` and
//! `d` in it, so `EXCEPT` can compare them.

use dbine_driver::transfer::{changed_buckets, BatchSink, BatchSource, Buckets, DeltaDepth, DeltaResult, DeltaSpec, ReadSpec, RowBatch, TransferColumn};
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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> i64 {
    let out = run(s, sql).await;
    let v = &out.results[0].rows[0][0];
    v.as_i64().unwrap_or_else(|| v.as_str().unwrap().parse().unwrap())
}

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

fn table(schema: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some(schema.into()), name: "t".into() }
}

const COLUMNS: [&str; 6] = ["id", "name", "amount", "at", "body", "flag"];

fn spec(schema: &str, buckets: Buckets, depth: DeltaDepth) -> DeltaSpec {
    DeltaSpec {
        table: table(schema),
        key: vec!["id".into()],
        columns: COLUMNS.iter().map(|c| c.to_string()).collect(),
        buckets,
        depth,
        max_cores: 0,
    }
}

/// summary → changed → filter → apply; the result and the buckets changed.
async fn sync(
    d: &Arc<dyn Driver>,
    src: &mut Box<dyn Session>,
    dst: Box<dyn Session>,
    buckets: Buckets,
    depth: DeltaDepth,
) -> (DeltaResult, usize, Box<dyn Session>) {
    let (s_spec, d_spec) = (spec("s", buckets.clone(), depth), spec("d", buckets, depth));
    let t = Instant::now();
    let mut dst = dst;
    let (a, b) = tokio::join!(src.delta_summary(&s_spec), dst.delta_summary(&d_spec));
    let (a, b) = (a.expect("summary s"), b.expect("summary d"));
    let changed = changed_buckets(&a, &b);
    println!("  resumen: {} + {} grupos en {:?}; cambiaron {}", a.len(), b.len(), t.elapsed(), changed.len());
    if changed.is_empty() {
        // The orchestrator stops here (an empty list would mean "every bucket").
        return (DeltaResult::default(), 0, dst);
    }
    let filter = d.delta_filter(&s_spec, &changed).expect("filter");
    println!("  filtro: {} caracteres", filter.len());

    let t = Instant::now();
    let (tx, rx) = mpsc::channel(16);
    let read = ReadSpec { table: table("s"), columns: Some(s_spec.columns.clone()), filter: Some(filter) };
    // The reader needs its own session (the apply holds the target's).
    let cfg_src = src.as_mut();
    let sink: dbine_driver::BatchSinkRef = Arc::new(Mutex::new(ChannelSink(tx)));
    let changed2 = changed.clone();
    let applier = tokio::spawn(async move {
        let mut source = ChannelSource(rx);
        let r = dst.delta_apply(&d_spec, &changed2, &[], &mut source, &|_| {}).await.expect("delta_apply");
        (r, dst)
    });
    let read_rows = cfg_src.read_batches(&read, sink).await.expect("read_batches");
    let (r, dst) = applier.await.unwrap();
    println!("  aplicado ({read_rows} filas leídas) en {:?}: {r:?}", t.elapsed());
    (r, changed.len(), dst)
}

async fn except_both_ways(s: &mut Box<dyn Session>) -> (i64, i64) {
    (
        scalar(s, "SELECT count(*) FROM (SELECT * FROM s.t EXCEPT SELECT * FROM d.t) x").await,
        scalar(s, "SELECT count(*) FROM (SELECT * FROM d.t EXCEPT SELECT * FROM s.t) x").await,
    )
}

async fn range_of(src: &mut Box<dyn Session>, dst: &mut Box<dyn Session>, n: u64) -> Buckets {
    let (a, b) = (src.key_range(&table("s"), "id").await.unwrap().unwrap(), dst.key_range(&table("d"), "id").await.unwrap().unwrap());
    let (lo, hi) = (a.0.min(b.0), a.1.max(b.1));
    let width = ((hi - lo) as u64 / n + 1) as i64;
    Buckets::Range { column: "id".into(), lo, hi, width, n }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn delta_postgres_1m() {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010/postgres".into());
    let cfg = parse_url("postgres", &url);
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "postgres").unwrap();
    assert!(d.supports_delta());
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let version = scalar(&mut admin, "SELECT current_setting('server_version_num')::int").await;
    println!("PostgreSQL {version}");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_delta WITH (FORCE)").await;
    run(&mut admin, "CREATE DATABASE dbine_delta").await;
    let mut src = d.connect(&cfg, Some("dbine_delta")).await.unwrap();
    let mut dst = d.connect(&cfg, Some("dbine_delta")).await.unwrap();

    let t = Instant::now();
    for sch in ["s", "d"] {
        run(
            &mut src,
            &format!(
                "CREATE SCHEMA {sch};
                 CREATE TABLE {sch}.t (id bigint PRIMARY KEY, name text, amount numeric(12,2), at timestamptz, body text, flag boolean);
                 INSERT INTO {sch}.t SELECT i, 'name ' || i, i * 1.25, timestamptz '2024-01-01 00:00+00' + make_interval(secs => i),
                        CASE WHEN i % 10 = 0 THEN NULL ELSE repeat(md5(i::text), 2) END, i % 2 = 0
                 FROM generate_series(1, 1000000) i;
                 ANALYZE {sch}.t;"
            ),
        )
        .await;
    }
    // A trigger on the target: it must not fire during the apply.
    run(
        &mut src,
        "CREATE TABLE d.fired (n int);
         CREATE FUNCTION d.log() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO d.fired VALUES (1); RETURN NULL; END $$;
         CREATE TRIGGER t_log AFTER INSERT OR UPDATE OR DELETE ON d.t FOR EACH ROW EXECUTE FUNCTION d.log();",
    )
    .await;
    println!("1.000.000 filas por lado en {:?}", t.elapsed());

    // 1k updated, 1k inserted, 1k deleted on the source.
    run(
        &mut src,
        "UPDATE s.t SET name = name || ' (editado)', amount = amount + 1 WHERE id % 1000 = 7;
         UPDATE s.t SET body = NULL WHERE id = 21;  -- NULL vs empty stays distinct
         UPDATE s.t SET body = '' WHERE id = 30;
         INSERT INTO s.t SELECT i, 'nuevo ' || i, 0, now(), '', true FROM generate_series(1000001, 1001000) i;
         DELETE FROM s.t WHERE id % 1000 = 3;",
    )
    .await;

    println!("Rangos:");
    let buckets = range_of(&mut src, &mut dst, 1000).await;
    let (r, changed, dst2) = sync(&d, &mut src, dst, buckets, DeltaDepth::Full).await;
    let mut dst = dst2;
    // The DELETE also took 1000003, one of the new rows.
    assert_eq!(r, DeltaResult { inserted: 999, updated: 1002, deleted: 1000, ..Default::default() });
    assert!(changed > 0);
    assert_eq!(except_both_ways(&mut src).await, (0, 0));
    let fired = scalar(&mut src, "SELECT count(*) FROM d.fired").await;
    println!("  triggers disparados: {fired}");
    assert_eq!(fired, 0, "superuser: triggers off during the apply");

    // A second pass finds nothing.
    let buckets = range_of(&mut src, &mut dst, 1000).await;
    let (r, changed, dst2) = sync(&d, &mut src, dst, buckets, DeltaDepth::Full).await;
    dst = dst2;
    assert_eq!((r, changed), (DeltaResult::default(), 0));

    // Few changes, hash buckets.
    run(
        &mut src,
        "UPDATE s.t SET amount = 1.50 WHERE id BETWEEN 100 AND 149;
         UPDATE s.t SET amount = amount WHERE id = 200;
         DELETE FROM s.t WHERE id BETWEEN 300 AND 309;
         INSERT INTO s.t VALUES (2000000, 'x', 1, now(), NULL, NULL);",
    )
    .await;
    println!("Hash (1009 grupos):");
    let (r, changed, dst2) = sync(&d, &mut src, dst, Buckets::Hash { n: 1009 }, DeltaDepth::Full).await;
    dst = dst2;
    assert_eq!(r, DeltaResult { inserted: 1, updated: 50, deleted: 10, ..Default::default() });
    assert!(changed <= 61);
    assert_eq!(except_both_ways(&mut src).await, (0, 0));

    // Scale-only change (1.5 → 1.50 in numeric(12,2) is the same text) and
    // a body length change with Sizes; Keys only sees inserts / deletes.
    run(&mut src, "UPDATE s.t SET body = body || 'z' WHERE id = 502; INSERT INTO s.t VALUES (2000001, 'y', 1, now(), NULL, NULL);").await;
    println!("Tamaños:");
    let buckets = range_of(&mut src, &mut dst, 1000).await;
    let (r, _, dst2) = sync(&d, &mut src, dst, buckets.clone(), DeltaDepth::Sizes).await;
    dst = dst2;
    assert_eq!(r, DeltaResult { inserted: 1, updated: 1, deleted: 0, ..Default::default() });
    run(&mut src, "UPDATE s.t SET name = 'otro' WHERE id = 600; DELETE FROM s.t WHERE id = 601;").await;
    println!("Claves:");
    let buckets = range_of(&mut src, &mut dst, 1000).await;
    let (r, _, dst2) = sync(&d, &mut src, dst, buckets, DeltaDepth::Keys).await;
    dst = dst2;
    // The deleted row's bucket is re-synced whole, so row 600 comes along
    // only if it shares that bucket (it does: width 1000).
    assert_eq!(r.deleted, 1);
    assert_eq!(except_both_ways(&mut src).await, (0, 0));

    drop(src);
    drop(dst);
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_delta WITH (FORCE)").await;
}

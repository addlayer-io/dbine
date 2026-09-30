//! Sync by rows against a real server: summaries on both sides, changed
//! buckets, filtered read, staged MERGE; checked with `EXCEPT` both ways.
//! Reads `DBINE_TEST_SQLSERVER_URL` (`mssql://user:pass@host:port`), by
//! default the `dbine-test-sqlserver` container:
//!
//! ```sh
//! cargo test --release -p dbine-driver-sqlserver --test delta -- --ignored delta --nocapture --test-threads=1
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::transfer::{changed_buckets, BatchSink, BatchSource, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, Buckets, ConnectionConfig, DeltaDepth, DeltaSpec, Driver, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

const DEFAULT_URL: &str = "mssql://sa:Pw_12345!@localhost:25013";
const SRC_DB: &str = "dbine_delta_src";
const DST_DB: &str = "dbine_delta_dst";

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = &out.error {
        panic!("{e}\n{sql}");
    }
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> i64 {
    let v = run(s, sql).await.results[0].rows[0][0].clone();
    v.as_i64().or_else(|| v.as_str().and_then(|x| x.parse().ok())).unwrap_or_else(|| panic!("{v:?}"))
}

/// `name` or `schema.name` (dbo by default).
fn table(name: &str) -> ObjectRef {
    let (schema, name) = name.split_once('.').unwrap_or(("dbo", name));
    ObjectRef { kind: "table".into(), schema: Some(schema.into()), name: name.into() }
}

struct Channel(mpsc::SyncSender<RowBatch>);
impl BatchSink for Channel {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.0.send(b).map_err(|_| io::Error::other("closed"))
    }
}
struct Receive(Arc<Mutex<mpsc::Receiver<RowBatch>>>);
#[async_trait]
impl BatchSource for Receive {
    async fn next(&mut self) -> Option<RowBatch> {
        let rx = self.0.clone();
        tokio::task::spawn_blocking(move || rx.lock().unwrap().recv().ok()).await.unwrap()
    }
}

const COLS: [&str; 6] = ["id", "a", "c", "d", "e", "big"];
// No identity: hash buckets refuse a synced identity column (see
// `delta_adversarial_identity_and_keys`), and this table runs both kinds.
const DDL: &str = "(id int NOT NULL PRIMARY KEY, a int NOT NULL, c nvarchar(50) NOT NULL, d varchar(100) NULL,
    e decimal(18,4) NULL, big nvarchar(max) NULL)";

/// Same bucket sizing as the orchestrator (dbine-transfer's delta).
fn range_buckets(column: &str, lo: i64, hi: i64, rows: u64) -> Buckets {
    let span = hi as i128 - lo as i128 + 1;
    let want = (rows / 1_000).clamp(1, 65_536) as i128;
    let width = ((span + want - 1) / want).clamp(1, i64::MAX as i128);
    let n = (span + width - 1) / width;
    Buckets::Range { column: column.into(), lo, hi, width: width as i64, n: n as u64 }
}

fn hash_buckets(rows: u64) -> Buckets {
    let is_prime = |n: u64| n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| !n.is_multiple_of(d));
    let want = (rows / 1_000).clamp(17, 65_537);
    Buckets::Hash { n: (want..).find(|&k| is_prime(k)).unwrap() }
}

fn bucket_total(b: &Buckets) -> u64 {
    match b {
        Buckets::Range { n, .. } => n + 2,
        Buckets::Hash { n } => *n,
    }
}

/// Reset the target to the original rows plus a few rogue changes.
async fn reset_target(dst: &mut Box<dyn Session>) {
    run(
        dst,
        "TRUNCATE TABLE dbo.t
         GO
         INSERT INTO dbo.t WITH (TABLOCK) (id, a, c, d, e, big) SELECT id, a, c, d, e, big FROM dbo.base;
         -- rogue rows: two only here, two edited, two missing
         INSERT INTO dbo.t (id, a, c, d, e, big) VALUES (5000000, 1, N'rogue', NULL, NULL, NULL), (-7, 2, N'rogue', 'x', 1, N'y');
         UPDATE dbo.t SET c = N'Rogue edit' WHERE id IN (250000, 750000);
         DELETE FROM dbo.t WHERE id IN (333333, 666666);",
    )
    .await;
}

struct Timing {
    label: String,
    changed: usize,
    total: u64,
    staged: u64,
    summary_s: f64,
    apply_s: f64,
    result: String,
}

async fn sync_once(buckets: Buckets, depth: DeltaDepth, label: &str) -> Timing {
    let d = driver();
    let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(SRC_DB)).await.unwrap()));
    let mut dst = d.connect(&config(), Some(DST_DB)).await.unwrap();
    let spec = DeltaSpec {
        table: table("t"),
        key: vec!["id".into()],
        columns: COLS.iter().map(|s| s.to_string()).collect(),
        buckets,
        depth,
        max_cores: 0,
    };
    let start = Instant::now();
    let (a, b) = tokio::join!(src.delta_summary(&spec), dst.delta_summary(&spec));
    let (a, b) = (a.unwrap(), b.unwrap());
    let summary_s = start.elapsed().as_secs_f64();
    let changed = changed_buckets(&a, &b);
    let total = bucket_total(&spec.buckets);
    let filtered = changed.len() <= 5_000 && changed.len() as u64 <= total / 2;
    let (apply, filter) = if filtered { (changed.clone(), Some(d.delta_filter(&spec, &changed).unwrap())) } else { (Vec::new(), None) };

    let start = Instant::now();
    let (tx, rx) = mpsc::sync_channel(16);
    let read = ReadSpec { table: table("t"), columns: Some(spec.columns.clone()), filter };
    let reader = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let s = driver().connect(&config(), Some(SRC_DB)).await.unwrap();
            let mut s = ReadOnlySession::new(s);
            s.read_batches(&read, Arc::new(Mutex::new(Channel(tx)))).await.unwrap()
        })
    });
    let r = dst.delta_apply(&spec, &apply, &[], &mut Receive(Arc::new(Mutex::new(rx))), &|_| {}).await.unwrap();
    let staged = reader.join().unwrap();
    let apply_s = start.elapsed().as_secs_f64();
    // Staging dropped.
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM sys.tables WHERE name LIKE '[_][_]dbine[_]delta[_]%'").await, 0);
    Timing {
        label: label.into(),
        changed: changed.len(),
        total,
        staged,
        summary_s,
        apply_s,
        result: format!("+{} ~{} -{}", r.inserted, r.updated, r.deleted),
    }
}

async fn except_both_ways(dst: &mut Box<dyn Session>) -> (i64, i64) {
    let list = COLS.join(", ");
    let q = |a: &str, b: &str| format!("SELECT COUNT_BIG(*) FROM (SELECT {list} FROM {a} EXCEPT SELECT {list} FROM {b}) x");
    let s = format!("[{SRC_DB}].dbo.t");
    let t = format!("[{DST_DB}].dbo.t");
    (scalar(dst, &q(&s, &t)).await, scalar(dst, &q(&t, &s)).await)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_end_to_end() {
    let rows: u64 = std::env::var("DBINE_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let mut admin = driver().connect(&config(), Some("master")).await.expect("connect");
    for db in [SRC_DB, DST_DB] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END
                 GO
                 CREATE DATABASE [{db}]
                 GO
                 ALTER DATABASE [{db}] SET RECOVERY SIMPLE"
            ),
        )
        .await;
    }
    let mut src = driver().connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut dst = driver().connect(&config(), Some(DST_DB)).await.unwrap();
    run(&mut src, &format!("CREATE TABLE dbo.t {DDL}")).await;
    let t = Instant::now();
    run(
        &mut src,
        &format!(
            "WITH n AS (SELECT TOP ({rows}) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS r FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN sys.all_columns c)
             INSERT INTO dbo.t WITH (TABLOCK) (id, a, c, d, e, big)
             SELECT CAST(r AS int), CAST(r AS int), CONCAT(N'nombre ', r), CASE WHEN r % 5 = 0 THEN NULL ELSE CONCAT('dato-', r) END,
                    CAST(r AS decimal(18,4)) / 7, CASE WHEN r % 100 = 0 THEN REPLICATE(CONVERT(nvarchar(max), N'x'), 3000) END
               FROM n"
        ),
    )
    .await;
    // The target: an identical copy, kept as `base` to reset between runs.
    run(&mut dst, &format!("CREATE TABLE dbo.t {DDL}\nGO\nCREATE TABLE dbo.base {DDL}")).await;
    run(
        &mut dst,
        &format!(
            "INSERT INTO dbo.base WITH (TABLOCK) (id, a, c, d, e, big) SELECT id, a, c, d, e, big FROM [{SRC_DB}].dbo.t;"
        ),
    )
    .await;
    eprintln!("generated {rows} rows (and the target copy) in {:.1?}", t.elapsed());
    // Source changes: 1k updated (contiguous), 1k deleted, 1k inserted.
    run(
        &mut src,
        "UPDATE dbo.t SET a = a + 1, c = CONCAT(c, N' v2') WHERE id BETWEEN 100001 AND 101000;
         DELETE FROM dbo.t WHERE id BETWEEN 500001 AND 501000;
         INSERT INTO dbo.t (id, a, c, d, e, big) SELECT TOP (1000) id + (SELECT MAX(id) FROM dbo.t), a, c, d, e, big FROM dbo.t ORDER BY id;",
    )
    .await;
    let src_rows = scalar(&mut src, "SELECT COUNT_BIG(*) FROM dbo.t").await;

    let d = driver();
    let (lo, hi, n) = {
        let mut ro: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(SRC_DB)).await.unwrap()));
        ro.key_range(&table("t"), "id").await.unwrap().unwrap()
    };
    assert_eq!(hi - lo + 1, rows as i64 + 1_000);
    let mut report = Vec::new();
    for (kind, buckets) in [("range", range_buckets("id", lo, hi, n)), ("hash", hash_buckets(n))] {
        for depth in [DeltaDepth::Full, DeltaDepth::Sizes, DeltaDepth::Keys] {
            reset_target(&mut dst).await;
            let t = sync_once(buckets.clone(), depth, &format!("{kind} {depth:?}")).await;
            let dst_rows = scalar(&mut dst, "SELECT COUNT_BIG(*) FROM dbo.t").await;
            assert_eq!(dst_rows, src_rows, "{}", t.label);
            let (a, b) = except_both_ways(&mut dst).await;
            if depth == DeltaDepth::Keys {
                // Keys only: updated rows in buckets without inserts or deletes stay.
                assert!(a == b && a <= 1_002, "{}: {a} / {b}", t.label);
            } else {
                assert_eq!((a, b), (0, 0), "{}", t.label);
            }
            report.push(format!(
                "{:<12} buckets {:>5}/{:<5} staged {:>8} summary {:>6.2} s apply {:>6.2} s  {} (except {a}/{b})",
                t.label, t.changed, t.total, t.staged, t.summary_s, t.apply_s, t.result
            ));
        }
    }
    // Already in sync: nothing changes.
    let t = sync_once(range_buckets("id", lo, hi, n), DeltaDepth::Full, "range again").await;
    assert_eq!(t.changed, 0, "{}", t.result);

    eprintln!("\n{}", report.join("\n"));
    drop((src, dst));
    for db in [SRC_DB, DST_DB] {
        run(&mut admin, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await;
    }
}

/// Collations, missing keys and refused tables surface as `Unsupported`,
/// triggers and foreign keys come back as they were.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_refusals_and_switches() {
    let mut admin = driver().connect(&config(), Some("master")).await.expect("connect");
    for db in [SRC_DB, DST_DB] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END
                 GO
                 CREATE DATABASE [{db}]"
            ),
        )
        .await;
    }
    let d = driver();
    let mut src = d.connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut dst = d.connect(&config(), Some(DST_DB)).await.unwrap();
    // String keys with different collations.
    run(&mut src, "CREATE TABLE dbo.s (k varchar(20) COLLATE Latin1_General_CS_AS PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.s VALUES ('a', 1), ('B', 2)").await;
    run(&mut dst, "CREATE TABLE dbo.s (k varchar(20) COLLATE Latin1_General_CI_AS PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.s VALUES ('a', 1), ('B', 2)").await;
    let spec = DeltaSpec {
        table: table("s"),
        key: vec!["k".into()],
        columns: vec!["k".into(), "v".into()],
        buckets: Buckets::Hash { n: 17 },
        depth: DeltaDepth::Full,
        max_cores: 1,
    };
    let (a, b) = (src.delta_summary(&spec).await.unwrap(), dst.delta_summary(&spec).await.unwrap());
    let changed = changed_buckets(&a, &b);
    // No sentinel: every bucket either side has differs, so even a filtered
    // apply covers every row on both sides.
    let mut all: Vec<i64> = a.iter().chain(b.iter()).map(|x| x.bucket).collect();
    all.sort_unstable();
    all.dedup();
    assert!(!all.is_empty() && a.iter().chain(b.iter()).all(|x| x.bucket >= 0), "no sentinel: {a:?} {b:?}");
    assert_eq!(changed, all, "different collations: every non-empty bucket");
    assert!(src.key_range(&table("s"), "k").await.is_err(), "a varchar key is not a range");

    // Float key, no unique key on the target.
    run(&mut src, "CREATE TABLE dbo.f (k float PRIMARY KEY, v int)").await;
    let mut fspec = spec.clone();
    fspec.table = table("f");
    assert!(matches!(src.delta_summary(&fspec).await, Err(dbine_driver::Error::Unsupported(_))));
    run(&mut dst, "CREATE TABLE dbo.nk (k int NOT NULL, v int)").await;
    let mut nk = spec.clone();
    nk.table = table("nk");
    let e = dst.delta_apply(&nk, &[], &[], &mut Receive(Arc::new(Mutex::new(mpsc::sync_channel(1).1))), &|_| {}).await.unwrap_err();
    assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e}");

    // Triggers and FKs: disabled during the merge, back after; identity reseeded.
    for s in [&mut src, &mut dst] {
        run(
            s,
            "CREATE TABLE dbo.p (id int IDENTITY PRIMARY KEY, v nvarchar(20) NOT NULL, parent int NULL REFERENCES dbo.p(id))
             GO
             CREATE TABLE dbo.child (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_child_p REFERENCES dbo.p(id))
             GO
             CREATE TABLE dbo.audit (n int)
             GO
             CREATE TRIGGER dbo.p_audit ON dbo.p AFTER INSERT, UPDATE, DELETE AS INSERT INTO dbo.audit VALUES (1)",
        )
        .await;
    }
    run(&mut src, "INSERT INTO dbo.p (v) VALUES (N'uno'), (N'dos'), (N'tres')").await;
    run(&mut dst, "INSERT INTO dbo.p (v) VALUES (N'uno'), (N'DOS')\nGO\nINSERT INTO dbo.child VALUES (1, 2)\nGO\nTRUNCATE TABLE dbo.audit").await;
    let pspec = DeltaSpec {
        table: table("p"),
        key: vec!["id".into()],
        columns: vec!["id".into(), "v".into(), "parent".into()],
        buckets: range_buckets("id", 1, 3, 3),
        depth: DeltaDepth::Full,
        max_cores: 0,
    };
    let (a, b) = (src.delta_summary(&pspec).await.unwrap(), dst.delta_summary(&pspec).await.unwrap());
    let changed = changed_buckets(&a, &b);
    let filter = d.delta_filter(&pspec, &changed).unwrap();
    let (tx, rx) = mpsc::sync_channel(16);
    let read = ReadSpec { table: table("p"), columns: Some(pspec.columns.clone()), filter: Some(filter) };
    let reader = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut s = ReadOnlySession::new(driver().connect(&config(), Some(SRC_DB)).await.unwrap());
            s.read_batches(&read, Arc::new(Mutex::new(Channel(tx)))).await.unwrap()
        })
    });
    let r = dst.delta_apply(&pspec, &changed, &[], &mut Receive(Arc::new(Mutex::new(rx))), &|_| {}).await.unwrap();
    reader.join().unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (1, 1, 0));
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.audit").await, 0, "trigger fired during the merge");
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM sys.triggers WHERE is_disabled = 1").await, 0);
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM sys.foreign_keys WHERE is_disabled = 1 OR is_not_trusted = 1").await, 0);
    assert_eq!(scalar(&mut dst, "SELECT CAST(IDENT_CURRENT('dbo.p') AS int)").await, 3);
    run(&mut dst, "INSERT INTO dbo.p (v) VALUES (N'cuatro')").await;
    assert_eq!(scalar(&mut dst, "SELECT MAX(id) FROM dbo.p").await, 4);

    drop((src, dst));
    for db in [SRC_DB, DST_DB] {
        run(&mut admin, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await;
    }
}

// ------------------------------------------------ adversarial cases
//
// Ported from the verifier's harness (verify-sqlserver-delta/tests/adv.rs):
//
// ```sh
// cargo test --release -p dbine-driver-sqlserver --test delta -- --ignored adversarial --nocapture --test-threads=1
// ```

const ADV_SRC: &str = "dbine_delta_adv_src";
const ADV_DST: &str = "dbine_delta_adv_dst";

async fn adv_setup() -> (Box<dyn Session>, Box<dyn Session>, Box<dyn Session>) {
    let mut admin = driver().connect(&config(), Some("master")).await.expect("connect");
    for db in [ADV_SRC, ADV_DST] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END
                 GO
                 CREATE DATABASE [{db}]"
            ),
        )
        .await;
    }
    let src = driver().connect(&config(), Some(ADV_SRC)).await.unwrap();
    let dst = driver().connect(&config(), Some(ADV_DST)).await.unwrap();
    (admin, src, dst)
}

async fn adv_teardown(mut admin: Box<dyn Session>) {
    for db in [ADV_SRC, ADV_DST] {
        run(&mut admin, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await;
    }
}

enum Plan {
    /// As the orchestrator: range buckets when `key_range` answers, else hash.
    Auto,
    Hash(u64),
    /// Hash buckets, applied as the whole table (an empty bucket list).
    WholeHash(u64),
    /// As `Auto`, comparing keys only.
    AutoKeys,
}

/// One orchestrator-like sync of `tbl`: `Ok("+i ~u -d")`, `Ok("nothing")`
/// or the error; the staging table is always gone afterwards.
async fn adv_sync(tbl: &str, key: &[&str], cols: &[&str], plan: Plan) -> (Vec<i64>, Result<String, dbine_driver::Error>) {
    let (changed, r) = adv_sync_notes(tbl, key, cols, plan).await;
    (changed, r.map(|(r, _)| r))
}

/// [`adv_sync`] with the result's notes.
async fn adv_sync_notes(
    tbl: &str,
    key: &[&str],
    cols: &[&str],
    plan: Plan,
) -> (Vec<i64>, Result<(String, Vec<String>), dbine_driver::Error>) {
    adv_sync_during(tbl, key, cols, plan, None).await
}

/// Runs `sql` on the target, on a connection of its own, when the apply
/// asks for its first batch: after its snapshot of the foreign keys and
/// before its merge, as a sync of another table running at once would.
struct During(Receive, Option<String>);
#[async_trait]
impl BatchSource for During {
    async fn next(&mut self) -> Option<RowBatch> {
        if let Some(sql) = self.1.take() {
            let mut s = driver().connect(&config(), Some(ADV_DST)).await.unwrap();
            run(&mut s, &sql).await;
        }
        self.0.next().await
    }
}

/// [`adv_sync_notes`], running `during` on the target mid-apply ([`During`]).
async fn adv_sync_during(
    tbl: &str,
    key: &[&str],
    cols: &[&str],
    plan: Plan,
    during: Option<&str>,
) -> (Vec<i64>, Result<(String, Vec<String>), dbine_driver::Error>) {
    let d = driver();
    let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(ADV_SRC)).await.unwrap()));
    let mut dst = d.connect(&config(), Some(ADV_DST)).await.unwrap();
    let depth = if matches!(plan, Plan::AutoKeys) { DeltaDepth::Keys } else { DeltaDepth::Full };
    let (buckets, whole) = match plan {
        Plan::Hash(n) => (Buckets::Hash { n }, false),
        Plan::WholeHash(n) => (Buckets::Hash { n }, true),
        Plan::Auto | Plan::AutoKeys => match src.key_range(&table(tbl), key[0]).await {
            Ok(Some((lo, hi, rows))) => (range_buckets(key[0], lo.min(hi), lo.max(hi), rows), false),
            Ok(None) => (range_buckets(key[0], 0, 0, 0), false),
            Err(_) => (Buckets::Hash { n: 17 }, false),
        },
    };
    let spec = DeltaSpec {
        table: table(tbl),
        key: key.iter().map(|s| s.to_string()).collect(),
        columns: cols.iter().map(|s| s.to_string()).collect(),
        buckets: buckets.clone(),
        depth,
        max_cores: 0,
    };
    let (a, b) = (src.delta_summary(&spec).await, dst.delta_summary(&spec).await);
    let (a, b) = match (a, b) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return (vec![], Err(e)),
    };
    let changed = changed_buckets(&a, &b);
    assert!(changed.iter().all(|b| *b >= -1), "no sentinel bucket: {changed:?}");
    if changed.is_empty() && !whole {
        return (changed, Ok(("nothing".into(), Vec::new())));
    }
    let filtered = !whole && changed.len() <= 5_000 && changed.len() as u64 <= bucket_total(&buckets) / 2;
    let (apply, filter) = if filtered { (changed.clone(), Some(d.delta_filter(&spec, &changed).unwrap())) } else { (Vec::new(), None) };
    let (tx, rx) = mpsc::sync_channel(16);
    let read = ReadSpec { table: table(tbl), columns: Some(spec.columns.clone()), filter };
    let reader = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut s = ReadOnlySession::new(driver().connect(&config(), Some(ADV_SRC)).await.unwrap());
            s.read_batches(&read, Arc::new(Mutex::new(Channel(tx)))).await
        })
    });
    let mut source = During(Receive(Arc::new(Mutex::new(rx))), during.map(str::to_string));
    let r = dst.delta_apply(&spec, &apply, &[], &mut source, &|_| {}).await;
    let _ = reader.join();
    // This table's staging only (other tables may be syncing at once).
    let (sch, name) = tbl.split_once('.').unwrap_or(("dbo", tbl));
    let like = format!("__dbine_delta_{name}_%").replace('_', "[_]");
    let staging = format!("SELECT COUNT(*) FROM sys.tables WHERE SCHEMA_NAME(schema_id) = '{sch}' AND name LIKE '{like}' AND LEN(name) = {}", 14 + name.len() + 17);
    assert_eq!(scalar(&mut dst, &staging).await, 0, "staging left");
    (changed, r.map(|r| (format!("+{} ~{} -{}", r.inserted, r.updated, r.deleted), r.notes)))
}

/// Rows that differ byte for byte (each column cast to varbinary), both ways.
async fn bin_diff(s: &mut Box<dyn Session>, tbl: &str, cols: &[&str]) -> (i64, i64) {
    let list: Vec<String> = cols.iter().map(|c| format!("CAST([{c}] AS varbinary(max)) AS [{c}]")).collect();
    let list = list.join(", ");
    let (sch, tbl) = tbl.split_once('.').unwrap_or(("dbo", tbl));
    let q = |a: &str, b: &str| format!("SELECT COUNT_BIG(*) FROM (SELECT {list} FROM [{a}].[{sch}].[{tbl}] EXCEPT SELECT {list} FROM [{b}].[{sch}].[{tbl}]) x");
    (scalar(s, &q(ADV_SRC, ADV_DST)).await, scalar(s, &q(ADV_DST, ADV_SRC)).await)
}

fn unsupported(r: &Result<String, dbine_driver::Error>, needle: &str) -> bool {
    matches!(r, Err(dbine_driver::Error::Unsupported(m)) if m.contains(needle))
}

/// NULL-heavy rows, collations and trailing spaces, key edges: all converge
/// byte for byte.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_content() {
    let (mut admin, mut src, mut dst) = adv_setup().await;

    // NULL vs '', 0x vs NULL, 'ab','c' vs 'a','bc'; range and hash.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.nh (id int NOT NULL PRIMARY KEY, a varchar(10) NULL, b varchar(10) NULL, c varbinary(10) NULL, d int NULL, e nvarchar(max) NULL)").await;
        run(s, "WITH n AS (SELECT TOP (3000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) r FROM sys.all_columns a CROSS JOIN sys.all_columns b)
                INSERT INTO dbo.nh SELECT r, CASE WHEN r%3=0 THEN 'x' END, NULL, NULL, CASE WHEN r%7=0 THEN r END, NULL FROM n").await;
    }
    run(&mut src, "UPDATE dbo.nh SET a = '' WHERE id = 10; UPDATE dbo.nh SET c = 0x WHERE id = 11;
                   UPDATE dbo.nh SET a='ab', b='c' WHERE id = 1500; UPDATE dbo.nh SET e = N'' WHERE id = 2999").await;
    let cols = ["id", "a", "b", "c", "d", "e"];
    for plan in [Plan::Auto, Plan::Hash(17)] {
        run(&mut dst, "UPDATE dbo.nh SET a = NULL WHERE id = 10; UPDATE dbo.nh SET c = NULL WHERE id = 11;
                       UPDATE dbo.nh SET a='a', b='bc' WHERE id = 1500; UPDATE dbo.nh SET e = NULL WHERE id = 2999;
                       INSERT INTO dbo.nh (id) VALUES (5000), (-3)").await;
        let (_, r) = adv_sync("nh", &["id"], &cols, plan).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(bin_diff(&mut admin, "nh", &cols).await, (0, 0), "NULL-heavy {r:?}");
    }

    // Case and trailing spaces under a CI collation, in the key and in values.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.ci (k varchar(20) COLLATE Latin1_General_CI_AS NOT NULL PRIMARY KEY, v varchar(20) COLLATE Latin1_General_CI_AS NULL)
                GO
                CREATE TABLE dbo.ci2 (id int PRIMARY KEY, v varchar(20) COLLATE Latin1_General_CI_AS NULL)").await;
    }
    run(&mut src, "INSERT INTO dbo.ci VALUES ('abc','x'),('def','y '),('Ghi','z'),('JKL','w')\nGO\nINSERT INTO dbo.ci2 VALUES (1,'a'),(2,'b '),(3,'C')").await;
    run(&mut dst, "INSERT INTO dbo.ci VALUES ('abc','x '),('def','y'),('ghi','z'),('jkl ','w')\nGO\nINSERT INTO dbo.ci2 VALUES (1,'a '),(2,'b'),(3,'c')").await;
    let (_, r) = adv_sync("ci", &["k"], &["k", "v"], Plan::Auto).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(bin_diff(&mut admin, "ci", &["k", "v"]).await, (0, 0), "CI key {r:?}");
    let (_, r) = adv_sync("ci2", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(bin_diff(&mut admin, "ci2", &["id", "v"]).await, (0, 0), "CI value {r:?}");

    // Keys below lo, above hi and at the bigint extremes.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.ed (id bigint PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.ed SELECT 999 + ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), 1 FROM (SELECT TOP (3000) 1 x FROM sys.all_columns a CROSS JOIN sys.all_columns b) q").await;
        run(s, "CREATE TABLE dbo.ex (id bigint PRIMARY KEY, v int)").await;
    }
    run(&mut dst, "INSERT INTO dbo.ed VALUES (-9223372036854775808, 1), (-5, 1), (0, 1), (999, 1), (4000, 1), (9223372036854775807, 1); UPDATE dbo.ed SET v = 7 WHERE id IN (1000, 3999)").await;
    let (_, r) = adv_sync("ed", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~2 -6"));
    assert_eq!(bin_diff(&mut admin, "ed", &["id", "v"]).await, (0, 0));
    run(&mut src, "INSERT INTO dbo.ex VALUES (-9223372036854775808, 1), (-1, 1), (0, 1), (1, 1), (9223372036854775806, 1), (9223372036854775807, 2)").await;
    run(&mut dst, "INSERT INTO dbo.ex VALUES (-1, 5), (0, 1), (1, 1), (9223372036854775806, 1), (9223372036854775807, 1), (42, 1)").await;
    let (_, r) = adv_sync("ex", &["id"], &["id", "v"], Plan::Auto).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(bin_diff(&mut admin, "ex", &["id", "v"]).await, (0, 0));

    drop((src, dst));
    adv_teardown(admin).await;
}

/// Nullable keys and non-key identities that can't converge are refused
/// with a reason, never synced halfway.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_refusals() {
    let (mut admin, mut src, mut dst) = adv_setup().await;

    // A nullable single-column unique key (the range column).
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.nk (k int NULL, v int NULL)\nGO\nCREATE UNIQUE INDEX ux_nk ON dbo.nk (k)").await;
    }
    run(&mut src, "INSERT INTO dbo.nk VALUES (NULL, 1), (1, 1), (2, 2), (3, 3)").await;
    run(&mut dst, "INSERT INTO dbo.nk VALUES (NULL, 999), (1, 1), (2, 2), (3, 3)").await;
    let (_, r) = adv_sync("nk", &["k"], &["k", "v"], Plan::Auto).await;
    assert!(unsupported(&r, "admite NULL"), "{r:?}");
    // Hash buckets too (CHECKSUM(NULL) is a number: it would fall in a bucket).
    let (_, r) = adv_sync("nk", &["k"], &["k", "v"], Plan::Hash(17)).await;
    assert!(unsupported(&r, "admite NULL"), "{r:?}");
    assert_eq!(scalar(&mut dst, "SELECT v FROM dbo.nk WHERE k IS NULL").await, 999, "the NULL-key row is never merged");
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.nk").await, 4);
    // Nullable on the source only: its summary refuses, whatever the target.
    run(&mut src, "CREATE TABLE dbo.nk2 (k int NULL, v int NULL)\nGO\nCREATE UNIQUE INDEX ux_nk2 ON dbo.nk2 (k)\nGO\nINSERT INTO dbo.nk2 VALUES (NULL, 1), (0, 2)").await;
    run(&mut dst, "CREATE TABLE dbo.nk2 (k int NOT NULL PRIMARY KEY, v int NULL)\nGO\nINSERT INTO dbo.nk2 VALUES (0, 5)").await;
    for plan in [Plan::Auto, Plan::Hash(17)] {
        let (_, r) = adv_sync("nk2", &["k"], &["k", "v"], plan).await;
        assert!(unsupported(&r, "admite NULL"), "{r:?}");
    }
    assert_eq!(scalar(&mut dst, "SELECT v FROM dbo.nk2 WHERE k = 0").await, 5, "bucket 0 untouched");

    // A composite key through a unique index with a nullable column.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.ck (a int NOT NULL, b varchar(10) NULL, v int NULL)\nGO\nCREATE UNIQUE INDEX ux_ck ON dbo.ck (b, a)").await;
    }
    run(&mut src, "INSERT INTO dbo.ck VALUES (1, NULL, 10), (1, 'x', 11), (2, NULL, 20), (3, 'y', 30)").await;
    run(&mut dst, "INSERT INTO dbo.ck VALUES (1, NULL, 10), (1, 'x', 99), (2, NULL, 21), (3, 'y', 30)").await;
    let (_, r) = adv_sync("ck", &["a", "b"], &["a", "b", "v"], Plan::Auto).await;
    assert!(unsupported(&r, "«b»"), "{r:?}");
    assert_eq!(scalar(&mut dst, "SELECT v FROM dbo.ck WHERE a = 1 AND b = 'x'").await, 99, "target untouched");

    // A non-key identity: only the range column carries the source's last
    // identity value, so it's refused, whatever its values.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.ni (k int PRIMARY KEY, n int IDENTITY, v int)").await;
    }
    run(&mut src, "INSERT INTO dbo.ni (k, v) VALUES (1, 1), (2, 2), (3, 3)").await;
    run(&mut dst, "SET IDENTITY_INSERT dbo.ni ON; INSERT INTO dbo.ni (k, n, v) VALUES (1, 1, 1), (2, 7, 2), (3, 3, 30); SET IDENTITY_INSERT dbo.ni OFF").await;
    let (_, r) = adv_sync("ni", &["k"], &["k", "n", "v"], Plan::Auto).await;
    assert!(unsupported(&r, "columna identidad «n» no es la primera columna de la clave"), "{r:?}");
    assert_eq!(scalar(&mut dst, "SELECT v FROM dbo.ni WHERE k = 3").await, 30, "target untouched");
    run(&mut dst, "DELETE FROM dbo.ni WHERE k = 2; SET IDENTITY_INSERT dbo.ni ON; INSERT INTO dbo.ni (k, n, v) VALUES (2, 2, 2); SET IDENTITY_INSERT dbo.ni OFF").await;
    let (_, r) = adv_sync("ni", &["k"], &["k", "n", "v"], Plan::Auto).await;
    assert!(unsupported(&r, "columna identidad «n»"), "{r:?}");
    // Left out of the synced columns, it's the target's own.
    let (_, r) = adv_sync("ni", &["k"], &["k", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~1 -0"));
    assert_eq!(bin_diff(&mut admin, "ni", &["k", "v"]).await, (0, 0));

    // Different key collations under hash buckets: every non-empty bucket of
    // either side differs, so a filtered apply covers every row too; merged
    // under the target's collation, byte-exact. Also as the whole table.
    run(&mut src, "CREATE TABLE dbo.cc (k varchar(20) COLLATE Latin1_General_CS_AS PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.cc VALUES ('a', 1), ('B', 2), ('c', 3)").await;
    run(&mut dst, "CREATE TABLE dbo.cc (k varchar(20) COLLATE Latin1_General_CI_AS PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.cc VALUES ('a', 1), ('b', 2), ('C', 9)").await;
    let (changed, r) = adv_sync("cc", &["k"], &["k", "v"], Plan::Hash(17)).await;
    assert!(changed.len() as u64 <= 17 / 2, "filtered: {changed:?}");
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(bin_diff(&mut admin, "cc", &["k", "v"]).await, (0, 0), "{r:?}");
    let (_, r) = adv_sync("cc", &["k"], &["k", "v"], Plan::Hash(17)).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~0 -0"), "still every row, nothing to change");
    let (_, r) = adv_sync("cc", &["k"], &["k", "v"], Plan::WholeHash(17)).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(bin_diff(&mut admin, "cc", &["k", "v"]).await, (0, 0), "{r:?}");
    // Two source keys the target's collation calls equal: refused.
    run(&mut src, "INSERT INTO dbo.cc VALUES ('A', 4)").await;
    let (_, r) = adv_sync("cc", &["k"], &["k", "v"], Plan::WholeHash(17)).await;
    assert!(unsupported(&r, "misma según la intercalación"), "{r:?}");
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.cc").await, 3, "target untouched");

    drop((src, dst));
    adv_teardown(admin).await;
}

/// Identity burned past the largest key, cascades, foreign keys trusted
/// again once the other table syncs, pre-untrusted keys and pre-disabled
/// triggers left as they were.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_identity_and_keys() {
    let (mut admin, mut src, mut dst) = adv_setup().await;

    // The source used 101 and 102, then deleted 102.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.idg (id int IDENTITY PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.idg (v) SELECT TOP (100) 1 FROM sys.all_columns").await;
    }
    run(&mut src, "DELETE FROM dbo.idg WHERE id > 90; INSERT INTO dbo.idg (v) VALUES (2); INSERT INTO dbo.idg (v) VALUES (3); DELETE FROM dbo.idg WHERE id = 102").await;
    let (_, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Auto).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(bin_diff(&mut admin, "idg", &["id", "v"]).await, (0, 0));
    let ident = "SELECT CAST(IDENT_CURRENT('dbo.idg') AS int)";
    assert_eq!((scalar(&mut src, ident).await, scalar(&mut dst, ident).await), (102, 102));
    run(&mut dst, "INSERT INTO dbo.idg (v) VALUES (9)").await;
    assert_eq!(scalar(&mut dst, "SELECT MAX(id) FROM dbo.idg").await, 103, "the next id skips the one the source burned");
    // An empty target that never had rows: the next id still follows the source.
    run(&mut dst, "CREATE TABLE dbo.idg2 (id int IDENTITY PRIMARY KEY, v int)").await;
    run(&mut src, "CREATE TABLE dbo.idg2 (id int IDENTITY PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.idg2 (v) VALUES (1), (2), (3)\nGO\nDELETE FROM dbo.idg2 WHERE id = 3").await;
    let (_, r) = adv_sync("idg2", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+2 ~0 -0"));
    run(&mut dst, "INSERT INTO dbo.idg2 (v) VALUES (9)").await;
    assert_eq!(scalar(&mut dst, "SELECT MAX(id) FROM dbo.idg2").await, 4);
    // Burned with no other change: the rows are equal, the forced bucket
    // alone makes it apply, and the target follows.
    let (_, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~0 -1"), "the target's own 103");
    // (The target already stands at 103, its own row's value.)
    run(&mut src, "INSERT INTO dbo.idg (v) VALUES (4), (5); DELETE FROM dbo.idg WHERE id > 101").await;
    let (changed, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!((changed.len(), r.as_deref().ok()), (1, Some("+0 ~0 -0")), "one real bucket, no sentinel: {changed:?}");
    assert_eq!((scalar(&mut src, ident).await, scalar(&mut dst, ident).await), (104, 104));
    // Burned far past the largest key: followed too.
    run(&mut src, "SET IDENTITY_INSERT dbo.idg ON; INSERT INTO dbo.idg (id, v) VALUES (5000000, 1); SET IDENTITY_INSERT dbo.idg OFF; DELETE FROM dbo.idg WHERE id = 5000000").await;
    let (_, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~0 -0"));
    assert_eq!((scalar(&mut src, ident).await, scalar(&mut dst, ident).await), (5_000_000, 5_000_000));
    let (changed, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")));
    assert_eq!(bin_diff(&mut admin, "idg", &["id", "v"]).await, (0, 0));
    // Hash buckets can't carry the source's value (verifier: IDENT_CURRENT
    // src=52, dst=51): refused, not silently behind.
    let (_, r) = adv_sync("idg", &["id"], &["id", "v"], Plan::Hash(17)).await;
    assert!(unsupported(&r, "columna identidad «id» se está agrupando por hash"), "{r:?}");

    // V1: the source's identity reseeded below its own rows. Equal tables
    // converge: nothing changed, every time; the target isn't reseeded back.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.ip (id int IDENTITY PRIMARY KEY, v int)\nGO\nINSERT INTO dbo.ip (v) VALUES (1), (2), (3), (4), (5)").await;
    }
    run(&mut src, "DBCC CHECKIDENT ('dbo.ip', RESEED, 2) WITH NO_INFOMSGS").await;
    let ident = "SELECT CAST(IDENT_CURRENT('dbo.ip') AS int)";
    for _ in 0..3 {
        let (changed, r) = adv_sync("ip", &["id"], &["id", "v"], Plan::Auto).await;
        assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")), "V1");
    }
    assert_eq!((scalar(&mut src, ident).await, scalar(&mut dst, ident).await), (2, 5));
    // A real change still applies, and the target stays past its rows.
    run(&mut src, "UPDATE dbo.ip SET v = 9 WHERE id = 4").await;
    let (_, r) = adv_sync("ip", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~1 -0"));
    assert_eq!(scalar(&mut dst, ident).await, 5);
    let (changed, r) = adv_sync("ip", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")));
    assert_eq!(bin_diff(&mut admin, "ip", &["id", "v"]).await, (0, 0));

    // Parent and child; the source deleted parent 2, cascading child 10.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v nvarchar(20) NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id) ON DELETE CASCADE)
                GO
                CREATE TABLE dbo.na (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_na_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, N'a'), (2, N'b'), (3, N'c'), (4, N'd')
                GO
                INSERT INTO dbo.ch VALUES (10, 2), (11, 1)
                GO
                INSERT INTO dbo.na VALUES (20, 4), (21, 1)").await;
    }
    run(&mut src, "DELETE FROM dbo.p WHERE id = 2; DELETE FROM dbo.na WHERE id = 20; DELETE FROM dbo.p WHERE id = 4").await;
    let fks = "SELECT COUNT(*) FROM sys.foreign_keys WHERE is_disabled = 1 OR is_not_trusted = 1";
    let marks = "SELECT COUNT(*) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted'";
    let (_, r) = adv_sync("p", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~0 -2"));
    // The cascade ran on the target as on the source; fk_ch_p never left.
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.ch").await, 1);
    assert_eq!(scalar(&mut dst, "SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE name = 'fk_ch_p'").await, 0);
    // fk_na_p (NO ACTION) now has an orphan: untrusted and marked until na syncs.
    assert_eq!(scalar(&mut dst, "SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE name = 'fk_na_p'").await, 1);
    assert_eq!(scalar(&mut dst, marks).await, 1);
    let (_, r) = adv_sync("na", &["id"], &["id", "p"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~0 -1"));
    assert_eq!(scalar(&mut dst, fks).await, 0, "every foreign key trusted again");
    assert_eq!(scalar(&mut dst, marks).await, 0, "mark removed");
    let (_, r) = adv_sync("ch", &["id"], &["id", "p"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("nothing"));
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"]), ("na", ["id", "p"])] {
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    // A foreign key untrusted before the sync stays so; a disabled trigger
    // stays disabled and doesn't fire.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.q (id int PRIMARY KEY, v int)
                GO
                CREATE TABLE dbo.aud (n int)
                GO
                CREATE TRIGGER dbo.q_trg ON dbo.q AFTER INSERT, UPDATE, DELETE AS INSERT INTO dbo.aud VALUES (1)
                GO
                CREATE TABLE dbo.q2 (id int PRIMARY KEY, q int NULL)
                GO
                INSERT INTO dbo.q VALUES (1, 1), (2, 2)").await;
    }
    run(&mut src, "UPDATE dbo.q SET v = 5 WHERE id = 2").await;
    run(&mut dst, "INSERT INTO dbo.q2 VALUES (20, 99)
                   GO
                   ALTER TABLE dbo.q2 WITH NOCHECK ADD CONSTRAINT fk_q2_q FOREIGN KEY (q) REFERENCES dbo.q(id)
                   GO
                   DISABLE TRIGGER q_trg ON dbo.q
                   GO
                   TRUNCATE TABLE dbo.aud").await;
    let (_, r) = adv_sync("q", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~1 -0"));
    assert_eq!(scalar(&mut dst, "SELECT CAST(is_disabled AS int) FROM sys.triggers WHERE name = 'q_trg'").await, 1);
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.aud").await, 0);
    assert_eq!(scalar(&mut dst, "SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE name = 'fk_q2_q'").await, 1);
    assert_eq!(scalar(&mut dst, marks).await, 0, "not marked: it was untrusted before");

    drop((src, dst));
    adv_teardown(admin).await;
}

/// Foreign keys whatever the order the tables sync in: an incoming
/// `ON DELETE` key a child-first sync left marked is checked again by the
/// parent's (N1); a parent update of a referenced non-key column doesn't
/// conflict and is checked again by the child's (N2); a key that fails is a
/// note naming it, both tables and what to do, never an error.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_fk_order() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    let trusted = |fk: &str| format!("SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE name = '{fk}'");
    let marks = "SELECT COUNT(*) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted'";

    // N1: child first.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v nvarchar(20) NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id) ON DELETE CASCADE)
                GO
                CREATE TABLE dbo.na (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_na_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, N'a'), (2, N'b'), (4, N'd')
                GO
                INSERT INTO dbo.ch VALUES (10, 1)
                GO
                INSERT INTO dbo.na VALUES (20, 2), (21, 4)").await;
    }
    run(&mut src, "INSERT INTO dbo.p VALUES (3, N'c'); INSERT INTO dbo.ch VALUES (30, 3)").await;
    let (_, r) = adv_sync_notes("ch", &["id"], &["id", "p"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(
        notes.len() == 1
            && notes[0].contains("[dbo].[fk_ch_p] de [dbo].[ch] a [dbo].[p] quedó sin verificar")
            && notes[0].contains("Sincronizá [dbo].[p] y la próxima sincronización la vuelve a validar"),
        "{notes:?}"
    );
    assert_eq!((scalar(&mut dst, &trusted("fk_ch_p")).await, scalar(&mut dst, marks).await), (1, 1), "waits for p");
    let (_, r) = adv_sync_notes("p", &["id"], &["id", "v"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_ch_p] de [dbo].[ch] a [dbo].[p] volvió a quedar verificada"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &trusted("fk_ch_p")).await, scalar(&mut dst, marks).await), (0, 0), "N1: trusted again");
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"])] {
        let (_, r) = adv_sync(t, &["id"], &c, Plan::Auto).await;
        assert_eq!(r.as_deref().ok(), Some("nothing"), "{t}");
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    // N2: the parent changes a referenced unique column, not the key.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p2 (id int PRIMARY KEY, code varchar(10) NOT NULL CONSTRAINT uq_p2_code UNIQUE)
                GO
                CREATE TABLE dbo.ch2 (id int PRIMARY KEY, code varchar(10) NULL
                                      CONSTRAINT fk_ch2_code REFERENCES dbo.p2(code) ON DELETE CASCADE ON UPDATE NO ACTION)
                GO
                INSERT INTO dbo.p2 VALUES (1, 'X'), (2, 'Z')
                GO
                INSERT INTO dbo.ch2 VALUES (10, 'X'), (11, 'Z')").await;
    }
    run(&mut src, "UPDATE dbo.ch2 SET code = NULL WHERE id = 10; UPDATE dbo.p2 SET code = 'Y' WHERE id = 1; UPDATE dbo.ch2 SET code = 'Y' WHERE id = 10").await;
    let (_, r) = adv_sync("p2", &["id"], &["id", "code"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~1 -0"), "N2: no REFERENCE conflict");
    assert_eq!((scalar(&mut dst, &trusted("fk_ch2_code")).await, scalar(&mut dst, marks).await), (1, 1), "waits for ch2");
    let (_, r) = adv_sync("ch2", &["id"], &["id", "code"], Plan::Auto).await;
    assert_eq!(r.as_deref().ok(), Some("+0 ~1 -0"));
    assert_eq!((scalar(&mut dst, &trusted("fk_ch2_code")).await, scalar(&mut dst, marks).await), (0, 0), "N2: trusted again");
    for (t, c) in [("p2", ["id", "code"]), ("ch2", ["id", "code"])] {
        let (_, r) = adv_sync(t, &["id"], &c, Plan::Auto).await;
        assert_eq!(r.as_deref().ok(), Some("nothing"), "{t}");
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    // Only p syncs: fk_na_p waits for na, marked by p, with a note.
    run(&mut src, "DELETE FROM dbo.na WHERE id = 20; DELETE FROM dbo.p WHERE id = 2").await;
    let (_, r) = adv_sync_notes("p", &["id"], &["id", "v"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+0 ~0 -1");
    assert!(notes.iter().any(|n| n.contains("[dbo].[fk_na_p] de [dbo].[na] a [dbo].[p]") && n.contains("Sincronizá [dbo].[na]")), "{notes:?}");
    assert_eq!(scalar(&mut dst, marks).await, 1);
    let by = run(&mut dst, "SELECT CAST(value AS nvarchar(100)) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted'").await;
    assert_eq!(by.results[0].rows[0][0].as_str(), Some("[dbo].[p]"), "marked by the table whose sync left it");
    // Equal rows: a mark forces no apply, so no repeated note.
    let (changed, r) = adv_sync("p", &["id"], &["id", "v"], Plan::Auto).await;
    assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")));
    assert_eq!(scalar(&mut dst, &trusted("fk_na_p")).await, 1);
    // The target's p loses row 4 behind the sync's back: after na syncs the
    // key still fails: a note naming both tables, never an error; still marked.
    run(&mut dst, "ALTER TABLE dbo.na NOCHECK CONSTRAINT fk_na_p; DELETE FROM dbo.p WHERE id = 4; ALTER TABLE dbo.na CHECK CONSTRAINT fk_na_p").await;
    let (_, r) = adv_sync_notes("na", &["id"], &["id", "p"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+0 ~0 -1");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_na_p] de [dbo].[na] a [dbo].[p]") && notes[0].contains("Sincronizá [dbo].[p]"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &trusted("fk_na_p")).await, scalar(&mut dst, marks).await), (1, 1));
    // p syncs its row 4 back: the marked key is checked again, trusted, mark gone.
    let (_, r) = adv_sync_notes("p", &["id"], &["id", "v"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_na_p] de [dbo].[na] a [dbo].[p] volvió a quedar verificada"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &trusted("fk_na_p")).await, scalar(&mut dst, marks).await), (0, 0));
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM sys.foreign_keys WHERE is_disabled = 1 OR is_not_trusted = 1").await, 0);
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"]), ("na", ["id", "p"])] {
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

/// The source's trust isn't mirrored (that's schema, for clone and
/// compare). An orphan the source has makes the target's key fail after
/// both tables synced: untrusted and marked, with a note, never an error;
/// equal tables stay quiet (no forced apply, no repeated note). A key the
/// target didn't trust before the sync is left as it is.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_source_untrusted_fk() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    let untrusted = |fk: &str| format!("SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE name = '{fk}'");
    let marks = |fk: &str| {
        format!(
            "SELECT COUNT(*) FROM sys.extended_properties ep JOIN sys.foreign_keys fk ON fk.object_id = ep.major_id
              WHERE ep.class = 1 AND ep.name = 'dbine_delta_untrusted' AND fk.name = '{fk}'"
        )
    };

    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, 1), (2, 2)
                GO
                INSERT INTO dbo.ch VALUES (10, 1)").await;
    }
    run(&mut src, "ALTER TABLE dbo.ch NOCHECK CONSTRAINT fk_ch_p
                   GO
                   INSERT INTO dbo.ch VALUES (20, 77)
                   GO
                   ALTER TABLE dbo.ch CHECK CONSTRAINT fk_ch_p").await;
    assert_eq!(scalar(&mut src, &untrusted("fk_ch_p")).await, 1);
    let (_, r) = adv_sync_notes("ch", &["id"], &["id", "p"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_ch_p]") && notes[0].contains("Sincronizá [dbo].[p]"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &untrusted("fk_ch_p")).await, scalar(&mut dst, &marks("fk_ch_p")).await), (1, 1));
    // p is equal: nothing to apply, no false error, no repeated note.
    for (t, c, plan) in [("p", ["id", "v"], Plan::Auto), ("ch", ["id", "p"], Plan::Auto), ("p", ["id", "v"], Plan::AutoKeys), ("ch", ["id", "p"], Plan::AutoKeys)] {
        let (changed, r) = adv_sync(t, &["id"], &c, plan).await;
        assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")), "{t}: later syncs stay quiet");
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }
    // p changes later: its apply checks the marked key again; the source's
    // orphan still breaks it: a note, not an error; the mark stays.
    run(&mut src, "UPDATE dbo.p SET v = 5 WHERE id = 2").await;
    let (_, r) = adv_sync_notes("p", &["id"], &["id", "v"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+0 ~1 -0");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_ch_p] de [dbo].[ch] a [dbo].[p]"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &untrusted("fk_ch_p")).await, scalar(&mut dst, &marks("fk_ch_p")).await), (1, 1));

    // Untrusted on the target before the sync: left as it is, no note.
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p4 (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch4 (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch4_p REFERENCES dbo.p4(id))
                GO
                INSERT INTO dbo.p4 SELECT TOP (3000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), 1 FROM sys.all_columns a CROSS JOIN sys.all_columns b
                GO
                INSERT INTO dbo.ch4 SELECT id, id FROM dbo.p4").await;
    }
    run(&mut dst, "ALTER TABLE dbo.ch4 NOCHECK CONSTRAINT fk_ch4_p
                   GO
                   UPDATE dbo.ch4 SET p = 99999 WHERE id = 7
                   GO
                   ALTER TABLE dbo.ch4 CHECK CONSTRAINT fk_ch4_p").await;
    let (changed, r) = adv_sync("ch4", &["id"], &["id", "p"], Plan::AutoKeys).await;
    assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")), "no trust sentinel");
    let (_, r) = adv_sync_notes("ch4", &["id"], &["id", "p"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!((res.as_str(), notes.len()), ("+0 ~1 -0", 0), "{notes:?}");
    assert_eq!((scalar(&mut dst, &untrusted("fk_ch4_p")).await, scalar(&mut dst, &marks("fk_ch4_p")).await), (1, 0), "not mirrored");
    for (t, c) in [("p4", ["id", "v"]), ("ch4", ["id", "p"])] {
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

/// dbo.ch and x.ch, each with a key named fk_ch_p into dbo.p: marks, notes
/// and checks never mix them up.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_schema_collision() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    for s in [&mut src, &mut dst] {
        run(s, "CREATE SCHEMA x
                GO
                CREATE TABLE dbo.p (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                CREATE TABLE x.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, 1)
                GO
                INSERT INTO dbo.ch VALUES (10, 1)
                GO
                INSERT INTO x.ch VALUES (10, 1)").await;
    }
    run(&mut src, "INSERT INTO dbo.p VALUES (2, 2); INSERT INTO x.ch VALUES (20, 2)").await;
    let state = |s: &str| {
        format!(
            "SELECT CAST(fk.is_not_trusted AS int) * 10 + (SELECT COUNT(*) FROM sys.extended_properties ep
                     WHERE ep.class = 1 AND ep.major_id = fk.object_id AND ep.name = 'dbine_delta_untrusted')
               FROM sys.foreign_keys fk WHERE fk.object_id = OBJECT_ID('{s}.fk_ch_p')"
        )
    };
    // x.ch first: only x.fk_ch_p waits for dbo.p.
    let (_, r) = adv_sync_notes("x.ch", &["id"], &["id", "p"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(
        notes.len() == 1 && notes[0].contains("[x].[fk_ch_p] de [x].[ch] a [dbo].[p]") && notes[0].contains("Sincronizá [dbo].[p]"),
        "{notes:?}"
    );
    assert_eq!((scalar(&mut dst, &state("x")).await, scalar(&mut dst, &state("dbo")).await), (11, 0));
    let by = run(&mut dst, "SELECT CAST(value AS nvarchar(100)) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted'").await;
    assert_eq!(by.results[0].rows[0][0].as_str(), Some("[x].[ch]"));
    // dbo.ch is equal: nothing; its key untouched.
    let (changed, r) = adv_sync("ch", &["id"], &["id", "p"], Plan::Auto).await;
    assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")));
    // dbo.p syncs: both keys into it checked; x's trusted again, dbo's never left.
    let (_, r) = adv_sync_notes("p", &["id"], &["id", "v"], Plan::Auto).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.len() == 1 && notes[0].contains("[x].[fk_ch_p] de [x].[ch] a [dbo].[p] volvió a quedar verificada"), "{notes:?}");
    assert_eq!((scalar(&mut dst, &state("x")).await, scalar(&mut dst, &state("dbo")).await), (0, 0));
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"]), ("x.ch", ["id", "p"])] {
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

/// A small parent (identity key) and child that change on every sync, in
/// both orders: never an error, one changed bucket each (no sentinel pushing
/// them into whole-table applies), notes only while the child waits for its
/// parent, trusted and unmarked after each round, identity following the
/// source.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_small_table_every_sync() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int IDENTITY PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p (v) VALUES (1), (2), (3)
                GO
                INSERT INTO dbo.ch VALUES (10, 1), (11, 2), (12, 3)").await;
    }
    let fk = "SELECT CAST(is_not_trusted AS int) * 10 + (SELECT COUNT(*) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted')
                FROM sys.foreign_keys WHERE name = 'fk_ch_p'";
    let ident = "SELECT CAST(IDENT_CURRENT('dbo.p') AS int)";
    for round in 0..4 {
        run(
            &mut src,
            &format!(
                "INSERT INTO dbo.p (v) VALUES ({round});
                 INSERT INTO dbo.ch VALUES ({id}, CAST(IDENT_CURRENT('dbo.p') AS int));
                 UPDATE dbo.ch SET p = 1 + {round} % 3 WHERE id = 10;",
                id = 100 + round
            ),
        )
        .await;
        let order: [(&str, [&str; 2]); 2] = if round % 2 == 0 {
            [("p", ["id", "v"]), ("ch", ["id", "p"])]
        } else {
            [("ch", ["id", "p"]), ("p", ["id", "v"])]
        };
        for (i, (t, c)) in order.iter().enumerate() {
            let (changed, r) = adv_sync_notes(t, &["id"], c, Plan::Auto).await;
            let (res, notes) = r.unwrap_or_else(|e| panic!("round {round} {t}: {e}"));
            assert_eq!(changed.len(), 1, "round {round} {t}: {changed:?} ({res})");
            match (round % 2, i) {
                (0, _) => assert!(notes.is_empty(), "round {round} {t}: {notes:?}"),
                (_, 0) => assert!(notes.len() == 1 && notes[0].contains("Sincronizá [dbo].[p]"), "round {round}: {notes:?}"),
                _ => assert!(notes.len() == 1 && notes[0].contains("volvió a quedar verificada"), "round {round}: {notes:?}"),
            }
        }
        assert_eq!(scalar(&mut dst, fk).await, 0, "round {round}: trusted, unmarked");
        assert_eq!(scalar(&mut src, ident).await, scalar(&mut dst, ident).await, "round {round}");
        for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"])] {
            let (changed, r) = adv_sync(t, &["id"], &c, Plan::Auto).await;
            assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")), "round {round} {t}");
            assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "round {round} {t}");
        }
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

/// Parent and child syncing at once (V2): the child's merge committed (the
/// key untrusted, not yet marked) before the parent's snapshot, and it
/// marks the key while the parent loads. The parent reads the marks again
/// after its commit and checks it, so it never waits for a further change.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_fk_concurrent_mark() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    let state = "SELECT CAST(is_not_trusted AS int) * 10 + (SELECT COUNT(*) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted')
                   FROM sys.foreign_keys WHERE name = 'fk_ch_p'";
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, 1), (2, 2)
                GO
                INSERT INTO dbo.ch VALUES (10, 1)").await;
    }
    run(&mut src, "INSERT INTO dbo.p VALUES (3, 3); INSERT INTO dbo.ch VALUES (30, 3)").await;
    // What the child's merge leaves on the target before its check.
    run(&mut dst, "ALTER TABLE dbo.ch NOCHECK CONSTRAINT fk_ch_p; INSERT INTO dbo.ch VALUES (30, 3); ALTER TABLE dbo.ch CHECK CONSTRAINT fk_ch_p").await;
    assert_eq!(scalar(&mut dst, state).await, 10, "untrusted, not marked yet");
    let mark = "EXEC sys.sp_addextendedproperty @name = N'dbine_delta_untrusted', @value = N'[dbo].[ch]',
                @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'ch',
                @level2type = N'CONSTRAINT', @level2name = N'fk_ch_p'";
    let (_, r) = adv_sync_during("p", &["id"], &["id", "v"], Plan::Auto, Some(mark)).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.len() == 1 && notes[0].contains("[dbo].[fk_ch_p] de [dbo].[ch] a [dbo].[p] volvió a quedar verificada"), "{notes:?}");
    assert_eq!(scalar(&mut dst, state).await, 0, "V2: trusted, unmarked without another change");
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"])] {
        let (changed, r) = adv_sync(t, &["id"], &c, Plan::Auto).await;
        assert_eq!((changed, r.as_deref().ok()), (vec![], Some("nothing")), "{t}");
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

/// The child's staging `SELECT … INTO` while the parent's merge switches the
/// child's foreign key (error 539, the staging table already created): a DDL
/// trigger on the target does the parent's `NOCHECK` / `CHECK` right when the
/// staging table is created, `fire` times. A few times: retried, applied,
/// staging gone. Every time: a Spanish error, nothing applied, staging gone.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_staging_schema_changed() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, 1), (2, 2)
                GO
                INSERT INTO dbo.ch VALUES (10, 1)").await;
    }
    run(&mut src, "INSERT INTO dbo.ch VALUES (20, 2)").await;
    run(&mut dst, "CREATE TABLE dbo.fire (n int NOT NULL)
                   GO
                   INSERT INTO dbo.fire VALUES (0)
                   GO
                   SET QUOTED_IDENTIFIER ON
                   GO
                   CREATE TRIGGER t539 ON DATABASE FOR CREATE_TABLE AS
                   BEGIN
                       SET NOCOUNT ON;
                       IF EVENTDATA().value('(/EVENT_INSTANCE/ObjectName)[1]', 'sysname') LIKE N'[_][_]dbine[_]delta[_]ch[_]%'
                          AND EXISTS (SELECT 1 FROM dbo.fire WHERE n > 0)
                       BEGIN
                           UPDATE dbo.fire SET n = n - 1;
                           ALTER TABLE dbo.ch NOCHECK CONSTRAINT fk_ch_p;
                           ALTER TABLE dbo.ch WITH CHECK CHECK CONSTRAINT fk_ch_p;
                       END
                   END").await;
    let cols = ["id", "p"];
    // Every try hits it.
    run(&mut dst, "UPDATE dbo.fire SET n = 100").await;
    let (_, r) = adv_sync("ch", &["id"], &cols, Plan::Auto).await;
    assert!(
        matches!(&r, Err(dbine_driver::Error::Query(m)) if m.contains("mientras se creaba la tabla de paso")
            && m.contains("No se cambió nada en el destino")),
        "{r:?}"
    );
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.ch").await, 1, "nothing applied");
    // A couple of tries: retried and applied (adv_sync checks the staging is gone).
    run(&mut dst, "UPDATE dbo.fire SET n = 2").await;
    let (_, r) = adv_sync_notes("ch", &["id"], &cols, Plan::Auto).await;
    assert_eq!(r.map(|(r, n)| (r, n.len())).ok(), Some(("+1 ~0 -0".to_string(), 0)));
    assert_eq!(scalar(&mut dst, "SELECT n FROM dbo.fire").await, 0, "the trigger fired");
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM sys.foreign_keys WHERE is_not_trusted = 1").await, 0);
    assert_eq!(bin_diff(&mut admin, "ch", &cols).await, (0, 0));
    run(&mut dst, "DROP TRIGGER t539 ON DATABASE").await;

    drop((src, dst));
    adv_teardown(admin).await;
}

/// Both syncs check the same marked key and pass: only the one that removes
/// the mark says it's verified. Here the child's sync (run mid-apply)
/// verifies and removes it first, so the parent's apply adds no note.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delta_adversarial_fk_verified_once() {
    let (mut admin, mut src, mut dst) = adv_setup().await;
    let state = "SELECT CAST(is_not_trusted AS int) * 10 + (SELECT COUNT(*) FROM sys.extended_properties WHERE name = 'dbine_delta_untrusted')
                   FROM sys.foreign_keys WHERE name = 'fk_ch_p'";
    for s in [&mut src, &mut dst] {
        run(s, "CREATE TABLE dbo.p (id int PRIMARY KEY, v int NOT NULL)
                GO
                CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))
                GO
                INSERT INTO dbo.p VALUES (1, 1), (2, 2)
                GO
                INSERT INTO dbo.ch VALUES (10, 1), (30, 2)").await;
    }
    run(&mut src, "INSERT INTO dbo.p VALUES (3, 3)").await;
    // What an earlier child-first sync left: untrusted and marked.
    let mark = "EXEC sys.sp_addextendedproperty @name = N'dbine_delta_untrusted', @value = N'[dbo].[ch]',
                @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'ch',
                @level2type = N'CONSTRAINT', @level2name = N'fk_ch_p'";
    run(&mut dst, &format!("ALTER TABLE dbo.ch NOCHECK CONSTRAINT fk_ch_p; ALTER TABLE dbo.ch CHECK CONSTRAINT fk_ch_p; {mark}")).await;
    assert_eq!(scalar(&mut dst, state).await, 11, "untrusted and marked");
    let child_verifies = "ALTER TABLE dbo.ch WITH CHECK CHECK CONSTRAINT fk_ch_p;
        EXEC sys.sp_dropextendedproperty @name = N'dbine_delta_untrusted', @level0type = N'SCHEMA', @level0name = N'dbo',
             @level1type = N'TABLE', @level1name = N'ch', @level2type = N'CONSTRAINT', @level2name = N'fk_ch_p'";
    let (_, r) = adv_sync_during("p", &["id"], &["id", "v"], Plan::Auto, Some(child_verifies)).await;
    let (res, notes) = r.unwrap();
    assert_eq!(res, "+1 ~0 -0");
    assert!(notes.is_empty(), "the child's sync already said it: {notes:?}");
    assert_eq!(scalar(&mut dst, state).await, 0, "trusted, unmarked");
    for (t, c) in [("p", ["id", "v"]), ("ch", ["id", "p"])] {
        assert_eq!(bin_diff(&mut admin, t, &c).await, (0, 0), "{t}");
    }

    drop((src, dst));
    adv_teardown(admin).await;
}

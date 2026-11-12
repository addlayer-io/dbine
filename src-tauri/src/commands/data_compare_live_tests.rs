//! Data compare over whole tables against real servers (issue #88): two
//! tables of 1,000,000 rows each built on the server, with known
//! differences (changed rows, rows only on each side, a gap in the keys on
//! one side), compared whole with the app's thresholds (so through the
//! scratch DuckDB); and SQL Server against PostgreSQL on 300,000 rows with
//! string keys of mixed case. Each test prints its time and the process's
//! peak memory while comparing.
//!
//! Ignored by default; the servers are the `dbine-test-*` containers on
//! their usual ports (`DBINE_TEST_SQLSERVER_URL`, `DBINE_TEST_POSTGRES_URL`
//! replace them):
//!
//! ```sh
//! SSL_CERT_FILE=/etc/ssl/cert.pem cargo test -p dbine --lib -- --ignored data_compare_live --test-threads=1 --nocapture
//! ```
//!
//! `data_compare_live_engine_*` compare 30,000 rows a side on other engines
//! (MySQL, ClickHouse, Cassandra, MongoDB, Elasticsearch, Solr: more than one
//! page or scroll of each), loaded with the driver's own bulk load and read
//! with its batched read, through the scratch DuckDB (a low threshold).
//! Their containers are `dbine-test-mysql`, `-clickhouse`, `-cassandra`,
//! `-mongodb`, `-elasticsearch` and `-solr` on their usual ports.
//!
//! Every test creates its `dbine_e2e_dcmp*` tables (or keyspace, database,
//! collections, indexes) and drops them at the end, also when an assertion
//! failed.

use super::whole::{self, Ctl, Input, Options, Progress, Side};
use super::{Counts, IN_MEMORY_BYTES, IN_MEMORY_ROWS, MAX_DIFF_ROWS};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Res<T> = Result<T, String>;

/// `user:pass@host:port` out of a URL (after an optional scheme).
fn parse(url: &str) -> (Option<String>, Option<String>, String, u16) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').map_or(("", rest), |(a, h)| (a, h));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, "0"), |(h, p)| (h, p));
    (
        (!user.is_empty()).then(|| user.to_string()),
        pass.map(str::to_string),
        host.to_string(),
        port.parse().unwrap_or(0),
    )
}

fn cfg(engine: &str, env: &str, default_url: &str, database: &str) -> ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default_url.to_string());
    let (username, password, host, port) = parse(&url);
    ConnectionConfig {
        driver: engine.into(),
        host,
        port,
        username,
        password,
        database: database.into(),
        trust_server_certificate: engine == "sqlserver",
        ..Default::default()
    }
}

fn sqlserver() -> ConnectionConfig {
    cfg(
        "sqlserver",
        "DBINE_TEST_SQLSERVER_URL",
        "mssql://sa:Pw_12345!@localhost:25013",
        "tempdb",
    )
}

fn postgres() -> ConnectionConfig {
    cfg(
        "postgres",
        "DBINE_TEST_POSTGRES_URL",
        "postgres://postgres:pw@localhost:25010",
        "postgres",
    )
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_drivers::find(&cfg.driver)
        .unwrap_or_else(|| panic!("no driver {}", cfg.driver))
        .clone();
    let db = (!cfg.database.is_empty()).then_some(cfg.database.as_str());
    d.connect(cfg, db)
        .await
        .unwrap_or_else(|e| panic!("connect {}: {e}", cfg.driver))
}

/// One `execute`, an error recorded in the outcome counting as an error.
async fn exec(s: &mut dyn Session, text: &str) -> Res<()> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out)
        .await
        .map_err(|e| format!("{e}\n--- {text}"))?;
    if let Some(e) = out.error.take() {
        return Err(format!("{e}\n--- {text}"));
    }
    Ok(())
}

fn table(schema: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some(schema.into()),
        name: name.into(),
    }
}

/// This process's resident memory, in bytes (`ps`).
fn rss() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// The peak of [`rss`] while `f` runs, sampled every 50 ms.
async fn peak_while<T>(f: impl std::future::Future<Output = T>) -> (T, u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(rss()));
    let sampler = {
        let (stop, peak) = (stop.clone(), peak.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                peak.fetch_max(rss(), Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let out = f.await;
    stop.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    (out, peak.load(Ordering::SeqCst))
}

/// Compare `lt` on `lcfg` with `rt` on `rcfg` whole, as the app does (its
/// thresholds, a scratch folder of the test's own), each side on its own
/// session; prints the time and peak memory.
async fn compare(
    label: &str,
    l: (&ConnectionConfig, &ObjectRef),
    r: (&ConnectionConfig, &ObjectRef),
    key: &[&str],
) -> Res<whole::Compared> {
    compare_with(label, l, r, key, &[], IN_MEMORY_ROWS).await
}

/// [`compare`] of the `wanted` columns (empty: all both sides have),
/// keeping at most `in_memory_rows` rows of a side in memory.
async fn compare_with(
    label: &str,
    (lcfg, lt): (&ConnectionConfig, &ObjectRef),
    (rcfg, rt): (&ConnectionConfig, &ObjectRef),
    key: &[&str],
    wanted: &[&str],
    in_memory_rows: usize,
) -> Res<whole::Compared> {
    let root = std::env::temp_dir().join(format!("dbine-dcmp-live-{}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut l = open(lcfg).await;
    let mut r = open(rcfg).await;
    let cols = [
        l.columns(lt).await.map_err(|e| e.to_string())?,
        r.columns(rt).await.map_err(|e| e.to_string())?,
    ];
    let ctl = Arc::new(Ctl::default());
    let opts = Options {
        in_memory_rows,
        in_memory_bytes: IN_MEMORY_BYTES,
        max_diff_rows: MAX_DIFF_ROWS,
        scratch: Some(root.clone()),
        duckdb: None,
    };
    let input = Input {
        key: key.iter().map(|k| k.to_string()).collect(),
        wanted: wanted.iter().map(|k| k.to_string()).collect(),
        cols,
    };
    let last = std::sync::Mutex::new(None::<Progress>);
    let started = Instant::now();
    // When each phase began.
    let phases = std::sync::Mutex::new(Vec::<(whole::Phase, Duration)>::new());
    let progress = |p: &Progress| {
        let mut ph = phases.lock().unwrap();
        if ph.last().map(|x| x.0) != Some(p.phase) {
            ph.push((p.phase, started.elapsed()));
        }
        *last.lock().unwrap() = Some(p.clone());
    };
    let before = rss();
    let (c, peak) = peak_while(whole::compare(
        Side {
            session: l.as_mut(),
            object: lt,
        },
        Side {
            session: r.as_mut(),
            object: rt,
        },
        input,
        &opts,
        &ctl,
        &progress,
    ))
    .await;
    let took = started.elapsed();
    let left_over: Vec<_> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    let _ = std::fs::remove_dir_all(&root);
    let c = c.map_err(|e| format!("{label}: {e}"))?;
    eprintln!(
        "{label}: {} + {} rows in {:.1} s ({:.0} rows/s), on disk: {}, peak RSS {:.0} MiB (before {:.0} MiB), phases {:?}, last progress {:?}",
        c.counts.left,
        c.counts.right,
        took.as_secs_f64(),
        (c.counts.left + c.counts.right) as f64 / took.as_secs_f64(),
        c.spilled,
        peak as f64 / 1048576.0,
        before as f64 / 1048576.0,
        phases.lock().unwrap(),
        last.lock().unwrap()
    );
    if !left_over.is_empty() {
        return Err(format!("{label}: scratch left behind: {left_over:?}"));
    }
    Ok(c)
}

fn check(label: &str, c: &whole::Compared, want: Counts) -> Res<()> {
    if c.counts != want {
        return Err(format!("{label}: counts {:?}, expected {want:?}", c.counts));
    }
    let k = &c.counts;
    if k.same + k.changed + k.only_left != k.left || k.same + k.changed + k.only_right != k.right {
        return Err(format!("{label}: counts don't add up: {k:?}"));
    }
    let listed = (c.only_left.len(), c.only_right.len(), c.changed.len());
    let counted = (
        k.only_left as usize,
        k.only_right as usize,
        k.changed as usize,
    );
    if listed != counted || c.diff_truncated || c.duplicate_keys != 0 {
        return Err(format!(
            "{label}: lists {listed:?} vs counts {counted:?}, truncated {}, duplicates {}",
            c.diff_truncated, c.duplicate_keys
        ));
    }
    if !c.spilled {
        return Err(format!("{label}: expected the scratch DuckDB"));
    }
    Ok(())
}

/// 1,000,000 rows on the left; on the right the same but for a gap
/// (600,001–650,000), 10,000 more past the end, and the amount of every
/// 1,000th row (outside the gap) plus one.
fn million() -> Counts {
    Counts {
        left: 1_000_000,
        right: 960_000,
        same: 949_050,
        only_left: 50_000,
        only_right: 10_000,
        changed: 950,
    }
}

const MSSQL_L: &str = "dbine_e2e_dcmp_l";
const MSSQL_R: &str = "dbine_e2e_dcmp_r";

fn mssql_build(name: &str, right: bool) -> String {
    let (top, filter, bump) = if right {
        (
            1_010_000,
            "WHERE i NOT BETWEEN 600001 AND 650000",
            "+ CASE WHEN i % 1000 = 0 THEN 1 ELSE 0 END",
        )
    } else {
        (1_000_000, "", "")
    };
    format!(
        "CREATE TABLE dbo.{name} (id int NOT NULL PRIMARY KEY, code nvarchar(20) NOT NULL, amount decimal(12,2) NOT NULL, created datetime2(0) NOT NULL, flag bit NOT NULL, note nvarchar(40) NULL);
WITH n AS (SELECT TOP ({top}) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c)
INSERT INTO dbo.{name} WITH (TABLOCK) (id, code, amount, created, flag, note)
SELECT i, CONCAT('C', i), i * 1.5 {bump}, DATEADD(second, i, CAST('2026-01-01' AS datetime2(0))), i % 2,
  CASE WHEN i % 10 = 0 THEN NULL ELSE CONCAT('note ', i) END FROM n {filter};"
    )
}

fn mssql_drop(name: &str) -> String {
    format!("IF OBJECT_ID('dbo.{name}') IS NOT NULL DROP TABLE dbo.{name}")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_sqlserver_million() {
    let c = sqlserver();
    let mut s = open(&c).await;
    let body = async {
        for t in [MSSQL_L, MSSQL_R] {
            exec(s.as_mut(), &mssql_drop(t)).await?;
        }
        exec(s.as_mut(), &mssql_build(MSSQL_L, false)).await?;
        exec(s.as_mut(), &mssql_build(MSSQL_R, true)).await?;
        let got = compare(
            "SQL Server 1M",
            (&c, &table("dbo", MSSQL_L)),
            (&c, &table("dbo", MSSQL_R)),
            &["id"],
        )
        .await?;
        check("SQL Server 1M", &got, million())?;
        // The changed rows differ in the amount only, and the gap is there.
        let amount = 2;
        if !got.changed.iter().all(|r| r.diff == [amount]) {
            return Err("SQL Server 1M: a changed row differs in more than the amount".into());
        }
        if got.only_left.first().and_then(|r| r[0].as_i64()) != Some(600_001) {
            return Err(format!(
                "SQL Server 1M: first only-left row {:?}",
                got.only_left.first()
            ));
        }
        Ok::<(), String>(())
    }
    .await;
    let mut cleanup = Ok(());
    for t in [MSSQL_L, MSSQL_R] {
        if let Err(e) = exec(s.as_mut(), &mssql_drop(t)).await {
            cleanup = Err(e);
        }
    }
    body.unwrap();
    cleanup.unwrap();
}

const PG_L: &str = "dbine_e2e_dcmp_l";
const PG_R: &str = "dbine_e2e_dcmp_r";

fn pg_build(name: &str, right: bool) -> String {
    let (last, filter, bump) = if right {
        (
            1_010_000,
            "WHERE i NOT BETWEEN 600001 AND 650000",
            "+ CASE WHEN i % 1000 = 0 THEN 1 ELSE 0 END",
        )
    } else {
        (1_000_000, "", "")
    };
    format!(
        "CREATE TABLE public.{name} (id int PRIMARY KEY, code text NOT NULL, amount numeric(12,2) NOT NULL, created timestamp NOT NULL, flag boolean NOT NULL, note text);
INSERT INTO public.{name} (id, code, amount, created, flag, note)
SELECT i, 'C' || i, i * 1.5 {bump}, timestamp '2026-01-01' + i * interval '1 second', i % 2 = 1,
  CASE WHEN i % 10 = 0 THEN NULL ELSE 'note ' || i END FROM generate_series(1, {last}) i {filter};"
    )
}

fn pg_drop(name: &str) -> String {
    format!("DROP TABLE IF EXISTS public.{name}")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_postgres_million() {
    let c = postgres();
    let mut s = open(&c).await;
    let body = async {
        for t in [PG_L, PG_R] {
            exec(s.as_mut(), &pg_drop(t)).await?;
        }
        exec(s.as_mut(), &pg_build(PG_L, false)).await?;
        exec(s.as_mut(), &pg_build(PG_R, true)).await?;
        let got = compare(
            "PostgreSQL 1M",
            (&c, &table("public", PG_L)),
            (&c, &table("public", PG_R)),
            &["id"],
        )
        .await?;
        check("PostgreSQL 1M", &got, million())?;
        let amount = 2;
        if !got.changed.iter().all(|r| r.diff == [amount]) {
            return Err("PostgreSQL 1M: a changed row differs in more than the amount".into());
        }
        Ok::<(), String>(())
    }
    .await;
    let mut cleanup = Ok(());
    for t in [PG_L, PG_R] {
        if let Err(e) = exec(s.as_mut(), &pg_drop(t)).await {
            cleanup = Err(e);
        }
    }
    body.unwrap();
    cleanup.unwrap();
}

const X_TABLE: &str = "dbine_e2e_dcmp_x";

/// 300,000 rows with keys like `Key-2` / `kEY-3` on SQL Server (whose
/// collation ignores case); PostgreSQL lacks 100,001–105,000, has 2,000
/// more, and a changed note on every 500th row outside the gap.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_sqlserver_against_postgres() {
    let (mc, pc) = (sqlserver(), postgres());
    let mut ms = open(&mc).await;
    let mut ps = open(&pc).await;
    let mssql = format!(
        "CREATE TABLE dbo.{X_TABLE} (k nvarchar(40) NOT NULL PRIMARY KEY, amount decimal(12,2) NOT NULL, flag bit NOT NULL, created datetime2(0) NOT NULL, note nvarchar(40) NULL);
WITH n AS (SELECT TOP (300000) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c)
INSERT INTO dbo.{X_TABLE} WITH (TABLOCK) (k, amount, flag, created, note)
SELECT CONCAT(CASE WHEN i % 2 = 0 THEN 'Key-' ELSE 'kEY-' END, i), i * 0.25, i % 3 / 2, DATEADD(minute, i, CAST('2026-03-01' AS datetime2(0))),
  CASE WHEN i % 7 = 0 THEN NULL ELSE CONCAT('n', i) END FROM n;"
    );
    let pg = format!(
        "CREATE TABLE public.{X_TABLE} (k text PRIMARY KEY, amount numeric(12,2) NOT NULL, flag boolean NOT NULL, created timestamp NOT NULL, note text);
INSERT INTO public.{X_TABLE} (k, amount, flag, created, note)
SELECT CASE WHEN i % 2 = 0 THEN 'Key-' ELSE 'kEY-' END || i, i * 0.25, i % 3 / 2 = 1, timestamp '2026-03-01' + i * interval '1 minute',
  CASE WHEN i % 7 = 0 THEN NULL WHEN i % 500 = 0 THEN 'changed ' || i ELSE 'n' || i END
  FROM generate_series(1, 302000) i WHERE i NOT BETWEEN 100001 AND 105000;"
    );
    let mssql_drop = mssql_drop(X_TABLE);
    let pg_drop = pg_drop(X_TABLE);
    let body = async {
        exec(ms.as_mut(), &mssql_drop).await?;
        exec(ps.as_mut(), &pg_drop).await?;
        exec(ms.as_mut(), &mssql).await?;
        exec(ps.as_mut(), &pg).await?;
        let got = compare(
            "SQL Server vs PostgreSQL 300k",
            (&mc, &table("dbo", X_TABLE)),
            (&pc, &table("public", X_TABLE)),
            &["k"],
        )
        .await?;
        // Multiples of 500 that are also of 7 have a NULL note on both.
        let changed = (1..=300_000u64)
            .filter(|i| i % 500 == 0 && i % 7 != 0 && !(100_001..=105_000).contains(i))
            .count() as u64;
        check(
            "SQL Server vs PostgreSQL 300k",
            &got,
            Counts {
                left: 300_000,
                right: 297_000,
                same: 300_000 - 5_000 - changed,
                only_left: 5_000,
                only_right: 2_000,
                changed,
            },
        )?;
        let note = 4;
        if !got.changed.iter().all(|r| r.diff == [note]) {
            return Err(format!(
                "cross-engine: a changed row differs in more than the note: {:?}",
                got.changed
                    .iter()
                    .find(|r| r.diff != [note])
                    .map(|r| (&r.left, &r.right))
            ));
        }
        Ok::<(), String>(())
    }
    .await;
    let c1 = exec(ms.as_mut(), &mssql_drop).await;
    let c2 = exec(ps.as_mut(), &pg_drop).await;
    body.unwrap();
    c1.unwrap();
    c2.unwrap();
}

// ---------------------------------------------------------------------------
// Other engines: 30,000 rows a side, each engine's own load and batched read

/// Rows on the left; the right has 1..=N+500 but for 10,001–11,000, and a
/// changed note on every 1,000th row.
const N: i64 = 30_000;

fn engine_counts() -> Counts {
    let changed = (1..=N)
        .filter(|i| i % 1000 == 0 && !(10_001..=11_000).contains(i))
        .count() as u64;
    Counts {
        left: N as u64,
        right: (N + 500 - 1000) as u64,
        same: N as u64 - 1000 - changed,
        only_left: 1000,
        only_right: 500,
        changed,
    }
}

/// The rows of a side as `id, code, amount, note` cells.
fn engine_rows(right: bool) -> Vec<Vec<dbine_driver::Cell>> {
    use dbine_driver::Cell;
    let last = if right { N + 500 } else { N };
    (1..=last)
        .filter(|i| !right || !(10_001..=11_000).contains(i))
        .map(|i| {
            let note = if right && i % 1000 == 0 {
                Cell::Text(format!("changed {i}"))
            } else if i % 10 == 0 {
                Cell::Null
            } else {
                Cell::Text(format!("note {i}"))
            };
            vec![
                Cell::Int(i),
                Cell::Text(format!("C{i}")),
                Cell::Decimal(format!("{:.2}", i as f64 * 1.5)),
                note,
            ]
        })
        .collect()
}

/// Batches of rows for a bulk load.
struct Rows(std::vec::IntoIter<Vec<dbine_driver::Cell>>);

#[dbine_driver::async_trait]
impl dbine_driver::BatchSource for Rows {
    async fn next(&mut self) -> Option<dbine_driver::RowBatch> {
        let rows: Vec<_> = self.0.by_ref().take(1_000).collect();
        (!rows.is_empty()).then(|| dbine_driver::RowBatch {
            bytes: rows.len() * 64,
            rows,
        })
    }
}

async fn load(s: &mut dyn Session, table: &ObjectRef, names: &[&str], right: bool) -> Res<()> {
    let columns: Vec<String> = names.iter().map(|c| c.to_string()).collect();
    let types = ["int", "varchar", "decimal(12,2)", "varchar"];
    let tcols: Vec<dbine_driver::TransferColumn> = columns
        .iter()
        .zip(types)
        .map(|(n, t)| dbine_driver::TransferColumn {
            name: n.clone(),
            type_name: t.into(),
            nullable: true,
        })
        .collect();
    let spec = dbine_driver::LoadSpec {
        table: table.clone(),
        columns,
        table_lock: false,
        keep_identity: false,
        commit_rows: dbine_driver::LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: dbine_driver::LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let mut rows = Rows(engine_rows(right).into_iter());
    s.bulk_load(&spec, &tcols, &mut rows, &|_| {})
        .await
        .map_err(|e| format!("load {}: {e}", table.name))?;
    Ok(())
}

struct Engine {
    label: &'static str,
    cfg: ConnectionConfig,
    /// Run before loading (errors fail the test) and at the end (errors
    /// ignored before, reported after).
    setup: Vec<String>,
    after_load: Vec<String>,
    teardown: Vec<String>,
    tables: [ObjectRef; 2],
    /// The columns compared (empty: all both sides have).
    columns: Vec<&'static str>,
    /// The four columns' names (`None`: id, code, amount, note).
    names: Option<[&'static str; 4]>,
}

fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: schema.map(str::to_string),
        name: name.into(),
    }
}

async fn engine_case(e: Engine) {
    let mut s = open(&e.cfg).await;
    for sql in &e.teardown {
        let _ = exec(s.as_mut(), sql).await;
    }
    let body = async {
        for sql in &e.setup {
            exec(s.as_mut(), sql).await?;
        }
        let names = e.names.unwrap_or(["id", "code", "amount", "note"]);
        load(s.as_mut(), &e.tables[0], &names, false).await?;
        load(s.as_mut(), &e.tables[1], &names, true).await?;
        for sql in &e.after_load {
            exec(s.as_mut(), sql).await?;
        }
        let got = compare_with(
            e.label,
            (&e.cfg, &e.tables[0]),
            (&e.cfg, &e.tables[1]),
            &["id"],
            &e.columns,
            2_000,
        )
        .await?;
        check(e.label, &got, engine_counts())?;
        let note = got
            .projection
            .columns
            .iter()
            .position(|c| c.0.to_ascii_lowercase().starts_with("note"))
            .ok_or("no note column")?;
        if !got.changed.iter().all(|r| r.diff == [note]) {
            return Err(format!(
                "{}: a changed row differs in more than the note: {:?}",
                e.label,
                got.changed
                    .iter()
                    .find(|r| r.diff != [note])
                    .map(|r| (&r.left, &r.right))
            ));
        }
        Ok::<(), String>(())
    }
    .await;
    let mut cleanup = Ok(());
    for sql in &e.teardown {
        if let Err(err) = exec(s.as_mut(), sql).await {
            cleanup = Err(err);
        }
    }
    body.unwrap();
    cleanup.unwrap();
}

fn plain(
    engine: &str,
    port: u16,
    user: Option<&str>,
    pass: Option<&str>,
    db: &str,
) -> ConnectionConfig {
    ConnectionConfig {
        driver: engine.into(),
        host: "localhost".into(),
        port,
        username: user.map(str::to_string),
        password: pass.map(str::to_string),
        database: db.into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_mysql() {
    let create = |t: &str| {
        format!("CREATE TABLE dbine_e2e_dcmp.{t} (id INT PRIMARY KEY, code VARCHAR(20) NOT NULL, amount DECIMAL(12,2) NOT NULL, note VARCHAR(40) NULL)")
    };
    engine_case(Engine {
        label: "MySQL 30k",
        cfg: plain("mysql", 25011, Some("root"), Some("pw"), ""),
        setup: vec![
            "CREATE DATABASE dbine_e2e_dcmp".into(),
            create("dcmp_l"),
            create("dcmp_r"),
        ],
        after_load: vec![],
        teardown: vec!["DROP DATABASE IF EXISTS dbine_e2e_dcmp".into()],
        tables: [
            obj("table", Some("dbine_e2e_dcmp"), "dcmp_l"),
            obj("table", Some("dbine_e2e_dcmp"), "dcmp_r"),
        ],
        columns: vec![],
        names: None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_clickhouse() {
    let create = |t: &str| {
        format!("CREATE TABLE default.{t} (id Int32, code String, amount Decimal(12,2), note Nullable(String)) ENGINE = MergeTree ORDER BY id")
    };
    engine_case(Engine {
        label: "ClickHouse 30k",
        cfg: plain("clickhouse", 25123, Some("dbine"), Some("dbine"), "default"),
        setup: vec![create("dbine_e2e_dcmp_l"), create("dbine_e2e_dcmp_r")],
        after_load: vec![],
        teardown: vec![
            "DROP TABLE IF EXISTS default.dbine_e2e_dcmp_l".into(),
            "DROP TABLE IF EXISTS default.dbine_e2e_dcmp_r".into(),
        ],
        tables: [
            obj("table", Some("default"), "dbine_e2e_dcmp_l"),
            obj("table", Some("default"), "dbine_e2e_dcmp_r"),
        ],
        columns: vec![],
        names: None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_cassandra() {
    let create = |t: &str| {
        format!("CREATE TABLE dbine_e2e_dcmp.{t} (id int PRIMARY KEY, code text, amount decimal, note text);")
    };
    engine_case(Engine {
        label: "Cassandra 30k",
        cfg: plain("cassandra", 25402, None, None, ""),
        setup: vec![
            "CREATE KEYSPACE dbine_e2e_dcmp WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};".into(),
            create("dcmp_l"),
            create("dcmp_r"),
        ],
        after_load: vec![],
        teardown: vec!["DROP KEYSPACE IF EXISTS dbine_e2e_dcmp;".into()],
        tables: [
            obj("table", Some("dbine_e2e_dcmp"), "dcmp_l"),
            obj("table", Some("dbine_e2e_dcmp"), "dcmp_r"),
        ],
        columns: vec![],
        names: None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_mongodb() {
    let mut cfg = plain(
        "mongodb",
        25201,
        Some("root"),
        Some("secret"),
        "dbine_e2e_dcmp",
    );
    cfg.options.insert("authSource".into(), "admin".into());
    engine_case(Engine {
        label: "MongoDB 30k",
        cfg,
        setup: vec![
            "db.createCollection(\"dcmp_l\")".into(),
            "db.createCollection(\"dcmp_r\")".into(),
        ],
        after_load: vec![],
        teardown: vec!["db.dropDatabase()".into()],
        tables: [
            obj("collection", None, "dcmp_l"),
            obj("collection", None, "dcmp_r"),
        ],
        // Each collection's `_id`s are its own.
        columns: vec!["id", "code", "amount", "note"],
        names: None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_elasticsearch() {
    let create = |t: &str| {
        format!("PUT /{t}\n{{\"mappings\": {{\"properties\": {{\"id\": {{\"type\": \"integer\"}}, \"code\": {{\"type\": \"keyword\"}}, \"amount\": {{\"type\": \"scaled_float\", \"scaling_factor\": 100}}, \"note\": {{\"type\": \"keyword\"}}}}}}}}")
    };
    engine_case(Engine {
        label: "Elasticsearch 30k",
        cfg: plain("elasticsearch", 25520, None, None, ""),
        setup: vec![create("dbine_e2e_dcmp_l"), create("dbine_e2e_dcmp_r")],
        after_load: vec![
            "POST /dbine_e2e_dcmp_l/_refresh".into(),
            "POST /dbine_e2e_dcmp_r/_refresh".into(),
        ],
        teardown: vec![
            "DELETE /dbine_e2e_dcmp_l".into(),
            "DELETE /dbine_e2e_dcmp_r".into(),
        ],
        tables: [
            obj("index", None, "dbine_e2e_dcmp_l"),
            obj("index", None, "dbine_e2e_dcmp_r"),
        ],
        columns: vec![],
        names: None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_live_engine_solr() {
    // The `_default` configset's dynamic fields (`*_s`, `*_d`): a standalone
    // server shares that configset between cores, so no field is added.
    engine_case(Engine {
        label: "Solr 30k",
        cfg: plain("solr", 25522, None, None, ""),
        setup: vec![
            "PUT /solr/dbine_e2e_dcmp_l".into(),
            "PUT /solr/dbine_e2e_dcmp_r".into(),
        ],
        after_load: vec![
            "POST /solr/dbine_e2e_dcmp_l/update?commit=true\n[]".into(),
            "POST /solr/dbine_e2e_dcmp_r/update?commit=true\n[]".into(),
        ],
        teardown: vec![
            "DELETE /solr/dbine_e2e_dcmp_l".into(),
            "DELETE /solr/dbine_e2e_dcmp_r".into(),
        ],
        tables: [
            obj("collection", None, "dbine_e2e_dcmp_l"),
            obj("collection", None, "dbine_e2e_dcmp_r"),
        ],
        // Each core's `_version_` is its own.
        columns: vec!["id", "code_s", "amount_d", "note_s"],
        names: Some(["id", "code_s", "amount_d", "note_s"]),
    })
    .await;
}

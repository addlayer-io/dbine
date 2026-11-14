//! The query compare against real servers (docs/data-compare.md, "Comparing
//! two queries"): each side a query on a `dbine-test-*` container, its
//! columns read by the probe as the UI reads them, then compared whole on
//! read-only sessions as `data_compare_query` opens them.
//!
//! - PostgreSQL against SQL Server: an aggregate by day and region (dates,
//!   decimals, booleans, timestamps) with and without a tolerance, and
//!   50,000 detail rows through the scratch DuckDB.
//! - MongoDB's `find` (columns as the union of the documents' keys) against
//!   PostgreSQL, with NULL as BLANK on a missing field and on the rows one
//!   side has all blank.
//! - Redis (`HGETALL`'s two columns, `SMEMBERS`'s one) against PostgreSQL.
//! - The refusals: a T-SQL batch with two results, and writes (that return
//!   rows) on read-only PostgreSQL, SQL Server and MongoDB sessions, which
//!   write nothing.
//!
//! The PostgreSQL side runs through a real driver host when
//! `DBINE_TEST_POSTGRES_HOST` names one: its rows then reach the compare
//! through `execute`'s row sink on the host's reader thread. Ignored by
//! default; the servers are the `dbine-test-postgres`, `-sqlserver`,
//! `-mongodb` and `-redis` containers on their usual ports
//! (`DBINE_TEST_POSTGRES_URL` and `DBINE_TEST_SQLSERVER_URL` replace them):
//!
//! ```sh
//! cargo build -p dbine-plugin-host --no-default-features --features postgres
//! DBINE_TEST_POSTGRES_HOST=$PWD/target/debug/dbine-plugin-host \
//!   cargo test -p dbine --lib -- --ignored data_compare_query_live --test-threads=1 --nocapture
//! ```
//!
//! Each test creates its `dbine_e2e_dcmpq*` tables, database or keys and
//! drops them at the end, also when an assertion failed.

use super::*;
use crate::commands::data_compare::whole::{
    Compared, Ctl, How, Input, Options, Progress, Read, Source,
};
use crate::commands::data_compare::{Counts, IN_MEMORY_BYTES, IN_MEMORY_ROWS, MAX_DIFF_ROWS};
use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Driver, Language, Session};
use std::collections::HashMap;
use std::path::PathBuf;

type Res<T> = Result<T, String>;

/// `user:pass@host:port` out of a URL (after an optional scheme).
fn parse(url: &str) -> (Option<String>, Option<String>, String, u16) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    (
        (!user.is_empty()).then(|| user.to_string()),
        pass.map(str::to_string),
        host.to_string(),
        port.parse().unwrap_or(0),
    )
}

fn from_url(driver: &str, env: &str, default_url: &str, database: &str) -> ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default_url.to_string());
    let (username, password, host, port) = parse(&url);
    ConnectionConfig {
        driver: driver.into(),
        host,
        port,
        username,
        password,
        database: database.into(),
        trust_server_certificate: driver == "sqlserver",
        ..Default::default()
    }
}

fn postgres() -> ConnectionConfig {
    from_url(
        "postgres",
        "DBINE_TEST_POSTGRES_URL",
        "postgres://postgres:pw@localhost:25010",
        "postgres",
    )
}

fn sqlserver() -> ConnectionConfig {
    from_url(
        "sqlserver",
        "DBINE_TEST_SQLSERVER_URL",
        "mssql://sa:Pw_12345!@localhost:25013",
        "tempdb",
    )
}

fn mongodb() -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "mongodb".into(),
        host: "localhost".into(),
        port: 25201,
        username: Some("root".into()),
        password: Some("secret".into()),
        database: "dbine_e2e_dcmpq".into(),
        ..Default::default()
    };
    c.options.insert("authSource".into(), "admin".into());
    c
}

fn redis() -> ConnectionConfig {
    ConnectionConfig {
        driver: "redis".into(),
        host: "localhost".into(),
        port: 25400,
        ..Default::default()
    }
}

/// `cfg`'s driver: served by its host process when `host_env` names one
/// (`true`), else built in.
fn driver(cfg: &ConnectionConfig, host_env: Option<&str>) -> (Arc<dyn Driver>, bool) {
    let local = dbine_drivers::find(&cfg.driver)
        .unwrap_or_else(|| panic!("no driver {}", cfg.driver))
        .clone();
    match host_env.and_then(|e| std::env::var(e).ok()) {
        Some(exe) if !exe.is_empty() => {
            let launcher = dbine_plugin::Launcher::at(
                &cfg.driver,
                PathBuf::from(exe),
                Vec::new(),
                Some(dbine_driver::runtime::components_dir()),
            );
            let remote = dbine_plugin::RemoteDriver::new(
                dbine_plugin::DriverMeta::of(&cfg.driver, local.as_ref()),
                launcher,
            );
            (Arc::new(remote), true)
        }
        _ => (local, false),
    }
}

/// A session to set up with, or (`read_only`) one as the compare opens it:
/// SQL sessions wrapped read-only as `dbine_drivers::open_session` does,
/// the others enforcing it themselves.
async fn open(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, read_only: bool) -> Box<dyn Session> {
    let mut cfg = cfg.clone();
    cfg.read_only = read_only;
    let db = (!cfg.database.is_empty()).then_some(cfg.database.as_str());
    let s = d
        .connect(&cfg, db)
        .await
        .unwrap_or_else(|e| panic!("connect {}: {e}", cfg.driver));
    if read_only && d.info().language == Language::Sql {
        Box::new(ReadOnlySession::with_dialect(s, d.script_dialect()))
    } else {
        s
    }
}

/// One `execute`, an error recorded in the outcome counting as an error.
async fn exec(s: &mut dyn Session, text: &str) -> Res<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100_000, &mut out)
        .await
        .map_err(|e| format!("{e}\n--- {}", head(text)))?;
    if let Some(e) = out.error.take() {
        return Err(format!("{e}\n--- {}", head(text)));
    }
    Ok(out)
}

fn head(text: &str) -> &str {
    &text[..text.len().min(300)]
}

/// The rows of a run's first result with columns.
fn first_rows(out: &QueryOutcome) -> Vec<Vec<Value>> {
    out.results
        .iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

/// A query's columns as the UI reads them: the probe on a read-only
/// session of its own, stopped at the first row, then closed.
async fn columns_of(
    d: &Arc<dyn Driver>,
    cfg: &ConnectionConfig,
    query: &str,
) -> CommandResult<QueryColumns> {
    let mut s = open(d, cfg, true).await;
    let stop = s.interrupter();
    let ctl = Ctl::default();
    probe(s.as_mut(), query, &ctl, || {
        if let Some(i) = stop {
            i();
        }
    })
    .await
}

/// A side of a live compare: its driver, connection and query.
struct LiveSide<'a> {
    driver: &'a Arc<dyn Driver>,
    cfg: &'a ConnectionConfig,
    query: &'a str,
}

/// Both queries' columns read, `pairs` (left, right, key) checked against
/// them, and compared whole on read-only sessions, at most
/// `in_memory_rows` rows a side in memory.
async fn compare_live(
    label: &str,
    left: LiveSide<'_>,
    right: LiveSide<'_>,
    pairs: &[(&str, &str, bool)],
    options: QueryOptions,
    in_memory_rows: usize,
) -> Res<Compared> {
    let lc = columns_of(left.driver, left.cfg, left.query)
        .await
        .map_err(|e| format!("{label}: left columns: {e}"))?;
    let rc = columns_of(right.driver, right.cfg, right.query)
        .await
        .map_err(|e| format!("{label}: right columns: {e}"))?;
    let has = |cols: &[ResultColumn], n: &str| cols.iter().any(|c| c.name.eq_ignore_ascii_case(n));
    for (l, r, _) in pairs {
        if !has(&lc.columns, l) || !has(&rc.columns, r) {
            return Err(format!(
                "{label}: «{l}» / «{r}» not among {:?} / {:?}",
                lc.columns, rc.columns
            ));
        }
    }
    compare_only(label, left, right, pairs, options, in_memory_rows).await
}

/// [`compare_live`] without reading the columns first (as a query changed
/// since they were read).
async fn compare_only(
    label: &str,
    left: LiveSide<'_>,
    right: LiveSide<'_>,
    pairs: &[(&str, &str, bool)],
    options: QueryOptions,
    in_memory_rows: usize,
) -> Res<Compared> {
    let columns: Vec<ColumnPair> = pairs
        .iter()
        .map(|(l, r, key)| ColumnPair {
            left: l.to_string(),
            right: r.to_string(),
            key: *key,
        })
        .collect();
    let root = std::env::temp_dir().join(format!("dbine-dcmpq-live-{}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut ls = open(left.driver, left.cfg, true).await;
    let mut rs = open(right.driver, right.cfg, true).await;
    let p = pairs_of(&columns).map_err(|e| e.to_string())?;
    let input = Input {
        key: Vec::new(),
        wanted: Vec::new(),
        cols: [named(&p, 0), named(&p, 1)],
    };
    let how = How {
        pairs: Some(p),
        rules: Arc::new(QueryRules::new(options).map_err(|e| e.to_string())?),
    };
    let opts = Options {
        in_memory_rows,
        in_memory_bytes: IN_MEMORY_BYTES,
        max_diff_rows: MAX_DIFF_ROWS,
        scratch: Some(root.clone()),
        duckdb: None,
    };
    let ctl = Arc::new(Ctl::default());
    let started = std::time::Instant::now();
    let c = whole::compare_with(
        Source {
            session: ls.as_mut(),
            read: Read::Query(left.query),
            hold: holds_its_reader(Some(&left.cfg.driver)),
        },
        Source {
            session: rs.as_mut(),
            read: Read::Query(right.query),
            hold: holds_its_reader(Some(&right.cfg.driver)),
        },
        input,
        how,
        &opts,
        &ctl,
        &|_: &Progress| {},
    )
    .await;
    let left_over: Vec<_> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    let _ = std::fs::remove_dir_all(&root);
    let c = c.map_err(|e| format!("{label}: {e}"))?;
    eprintln!(
        "{label}: {:?} in {:.1} s, on disk: {}; types {:?} / {:?}; first changed: {:?}",
        c.counts,
        started.elapsed().as_secs_f64(),
        c.spilled,
        c.query_columns[0]
            .iter()
            .map(|x| x.type_name.as_str())
            .collect::<Vec<_>>(),
        c.query_columns[1]
            .iter()
            .map(|x| x.type_name.as_str())
            .collect::<Vec<_>>(),
        c.changed.first().map(|r| (&r.diff, &r.left, &r.right))
    );
    if !left_over.is_empty() {
        return Err(format!("{label}: scratch left behind: {left_over:?}"));
    }
    Ok(c)
}

fn check(label: &str, got: &Counts, want: &Counts) -> Res<()> {
    if got != want {
        return Err(format!("{label}: counts {got:?}, expected {want:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PostgreSQL against SQL Server

const SALES: &str = "dbine_e2e_dcmpq_sales";
const ROWS: u64 = 50_000;

/// A row SQL Server doesn't have: one day's Norte.
fn excluded(i: u64) -> bool {
    i % 365 == 364 && i.is_multiple_of(4)
}

/// What SQL Server adds to a row's amount, in thousandths: rounding on
/// every 997th row, a real difference on every 4,999th.
fn extra_milli(i: u64) -> u64 {
    (if i.is_multiple_of(997) { 4 } else { 0 }) + (if i.is_multiple_of(4999) { 5_000 } else { 0 })
}

fn pg_sales() -> String {
    format!(
        "DROP TABLE IF EXISTS public.{SALES};
CREATE TABLE public.{SALES} (id int PRIMARY KEY, day date NOT NULL, region text NOT NULL, amount numeric(12,3) NOT NULL, active boolean NOT NULL, ts timestamp NOT NULL);
INSERT INTO public.{SALES}
SELECT i, date '2024-01-01' + (i % 365), (ARRAY['Norte','Sur','Este','Oeste'])[i % 4 + 1], (i % 1000) * 0.125, i % 5 <> 0, timestamp '2024-01-01' + i * interval '1 minute'
  FROM generate_series(1, {ROWS}) i;"
    )
}

fn mssql_sales() -> String {
    format!(
        "IF OBJECT_ID('dbo.{SALES}') IS NOT NULL DROP TABLE dbo.{SALES};
CREATE TABLE dbo.{SALES} (id int NOT NULL PRIMARY KEY, day date NOT NULL, region nvarchar(20) NOT NULL, amount decimal(12,3) NOT NULL, active bit NOT NULL, ts datetime2(0) NOT NULL);
WITH n AS (SELECT TOP ({ROWS}) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO dbo.{SALES} (id, day, region, amount, active, ts)
SELECT i, DATEADD(day, i % 365, CAST('2024-01-01' AS date)), CHOOSE(i % 4 + 1, 'Norte', 'Sur', 'Este', 'Oeste'),
  (i % 1000) * 0.125 + CASE WHEN i % 997 = 0 THEN 0.004 ELSE 0 END + CASE WHEN i % 4999 = 0 THEN 5 ELSE 0 END,
  CASE WHEN i % 5 <> 0 THEN 1 ELSE 0 END, DATEADD(minute, i, CAST('2024-01-01' AS datetime2(0)))
FROM n WHERE NOT (i % 365 = 364 AND i % 4 = 0);"
    )
}

/// The aggregate's counts, from the same rows: by (day, region), what SQL
/// Server adds to each group's total, in thousandths.
fn aggregate_counts(tolerance_milli: u64) -> Counts {
    let mut extra: HashMap<(u64, u64), u64> = HashMap::new();
    let mut groups = std::collections::HashSet::new();
    for i in 1..=ROWS {
        let g = (i % 365, i % 4);
        groups.insert(g);
        if !excluded(i) {
            *extra.entry(g).or_default() += extra_milli(i);
        }
    }
    let gone = groups.iter().filter(|g| !extra.contains_key(g)).count() as u64;
    let changed = extra.values().filter(|&&x| x > tolerance_milli).count() as u64;
    let left = groups.len() as u64;
    let right = left - gone;
    Counts {
        left,
        right,
        same: right - changed,
        only_left: gone,
        only_right: 0,
        changed,
    }
}

/// The detail's counts: by id.
fn detail_counts(tolerance_milli: u64) -> Counts {
    let gone = (1..=ROWS).filter(|&i| excluded(i)).count() as u64;
    let changed = (1..=ROWS)
        .filter(|&i| !excluded(i) && extra_milli(i) > tolerance_milli)
        .count() as u64;
    Counts {
        left: ROWS,
        right: ROWS - gone,
        same: ROWS - gone - changed,
        only_left: gone,
        only_right: 0,
        changed,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_query_live_postgres_against_sqlserver() {
    let (pc, mc) = (postgres(), sqlserver());
    let (pd, via_host) = driver(&pc, Some("DBINE_TEST_POSTGRES_HOST"));
    let (md, _) = driver(&mc, None);
    eprintln!("PostgreSQL through a driver host: {via_host}");
    let mut ps = open(&pd, &pc, false).await;
    let mut ms = open(&md, &mc, false).await;
    let pg_drop = format!("DROP TABLE IF EXISTS public.{SALES}");
    let mssql_drop = format!("IF OBJECT_ID('dbo.{SALES}') IS NOT NULL DROP TABLE dbo.{SALES}");
    let body = async {
        exec(ps.as_mut(), &pg_sales()).await?;
        exec(ms.as_mut(), &mssql_sales()).await?;
        let pg_agg = format!(
            "SELECT day, region, SUM(amount) AS total, bool_and(active) AS all_active, MAX(ts) AS last_ts FROM public.{SALES} GROUP BY day, region"
        );
        let mssql_agg = format!(
            "SELECT day AS [Day], region AS Region, SUM(amount) AS Total, CAST(MIN(CAST(active AS int)) AS bit) AS AllActive, MAX(ts) AS LastTs FROM dbo.{SALES} GROUP BY day, region"
        );
        let agg_pairs = [
            ("day", "Day", true),
            ("region", "Region", true),
            ("total", "Total", false),
            ("all_active", "AllActive", false),
            ("last_ts", "LastTs", false),
        ];
        let pg = LiveSide {
            driver: &pd,
            cfg: &pc,
            query: &pg_agg,
        };
        let mssql = LiveSide {
            driver: &md,
            cfg: &mc,
            query: &mssql_agg,
        };
        // Exactly: every group with a rounding or a real difference.
        let c = compare_live(
            "aggregate, exact",
            pg,
            mssql,
            &agg_pairs,
            QueryOptions::default(),
            IN_MEMORY_ROWS,
        )
        .await?;
        check("aggregate, exact", &c.counts, &aggregate_counts(0))?;
        let total = 2;
        if !c.changed.iter().all(|r| r.diff == [total]) {
            return Err(format!(
                "aggregate: a group differs in more than its total: {:?}",
                c.changed.iter().find(|r| r.diff != [total]).map(|r| (&r.left, &r.right))
            ));
        }
        if c.query_columns[0][3].type_name.is_empty() || c.query_columns[1][3].type_name.is_empty() {
            return Err("aggregate: the booleans' types are unknown".into());
        }
        // Within 0.01: only the real differences.
        let pg = LiveSide {
            driver: &pd,
            cfg: &pc,
            query: &pg_agg,
        };
        let mssql = LiveSide {
            driver: &md,
            cfg: &mc,
            query: &mssql_agg,
        };
        let tolerant = QueryOptions {
            absolute: 0.01,
            ..Default::default()
        };
        let c = compare_live("aggregate, ±0.01", pg, mssql, &agg_pairs, tolerant, IN_MEMORY_ROWS).await?;
        check("aggregate, ±0.01", &c.counts, &aggregate_counts(10))?;
        // The rows themselves, past what stays in memory: the scratch DuckDB.
        let pg_rows = format!("SELECT id, amount, active, ts FROM public.{SALES}");
        let mssql_rows = format!("SELECT id, amount, active, ts FROM dbo.{SALES}");
        let detail_pairs = [
            ("id", "id", true),
            ("amount", "amount", false),
            ("active", "active", false),
            ("ts", "ts", false),
        ];
        for (label, options, tolerance) in [
            ("detail, exact", QueryOptions::default(), 0),
            ("detail, ±0.01", tolerant, 10),
        ] {
            let pg = LiveSide {
                driver: &pd,
                cfg: &pc,
                query: &pg_rows,
            };
            let mssql = LiveSide {
                driver: &md,
                cfg: &mc,
                query: &mssql_rows,
            };
            let c = compare_live(label, pg, mssql, &detail_pairs, options, 5_000).await?;
            if !c.spilled {
                return Err(format!("{label}: stayed in memory"));
            }
            check(label, &c.counts, &detail_counts(tolerance))?;
        }
        Ok::<(), String>(())
    }
    .await;
    let c1 = exec(ps.as_mut(), &pg_drop).await;
    let c2 = exec(ms.as_mut(), &mssql_drop).await;
    body.unwrap();
    c1.unwrap();
    c2.unwrap();
}

// ---------------------------------------------------------------------------
// MongoDB against PostgreSQL

const ITEMS: &str = "dbine_e2e_dcmpq_items";
const DOCS: u64 = 2_000;

/// A document's fields: quantity, price (none on every 250th) and active;
/// MongoDB's quantity is one more on every 333rd.
fn item(i: u64, mongo: bool) -> (u64, Option<f64>, bool) {
    let qty = i % 50 + u64::from(mongo && i.is_multiple_of(333));
    let price = (!i.is_multiple_of(250)).then_some((i % 100) as f64 * 0.5);
    (qty, price, !i.is_multiple_of(3))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_query_live_mongodb_against_postgres() {
    let (gc, pc) = (mongodb(), postgres());
    let (gd, _) = driver(&gc, None);
    let (pd, via_host) = driver(&pc, Some("DBINE_TEST_POSTGRES_HOST"));
    eprintln!("PostgreSQL through a driver host: {via_host}");
    let mut gs = open(&gd, &gc, false).await;
    let mut ps = open(&pd, &pc, false).await;
    let pg_drop = format!("DROP TABLE IF EXISTS public.{ITEMS}");
    let _ = exec(gs.as_mut(), "db.dropDatabase()").await;
    let body = async {
        // MongoDB: the items, and one all blank that PostgreSQL lacks.
        let mut docs: Vec<String> = (1..=DOCS)
            .map(|i| {
                let (qty, price, active) = item(i, true);
                let price = price.map_or(String::new(), |p| format!(", price: {p}"));
                format!("{{code: 'C{i:05}', qty: {qty}{price}, active: {active}}}")
            })
            .collect();
        docs.push("{code: 'M00001', qty: 0, active: false}".into());
        exec(
            gs.as_mut(),
            &format!("db.items.insertMany([{}])", docs.join(", ")),
        )
        .await?;
        // PostgreSQL: a missing price as 0, plus two rows all blank and
        // one with a value.
        let values: Vec<String> = (1..=DOCS)
            .map(|i| {
                let (qty, price, active) = item(i, false);
                format!(
                    "('C{i:05}', {qty}, {}, {active})",
                    price.map_or("0".to_string(), |p| format!("{p}"))
                )
            })
            .chain([
                "('Z00001', 0, NULL, false)".to_string(),
                "('Z00002', NULL, 0, NULL)".to_string(),
                "('Z00003', 5, NULL, true)".to_string(),
            ])
            .collect();
        exec(
            ps.as_mut(),
            &format!(
                "DROP TABLE IF EXISTS public.{ITEMS};
CREATE TABLE public.{ITEMS} (code text PRIMARY KEY, qty int, price numeric(10,2), active boolean);
INSERT INTO public.{ITEMS} VALUES {};",
                values.join(", ")
            ),
        )
        .await?;
        let mongo_q = "db.items.find({}, {_id: 0})";
        let pg_q = format!("SELECT code, qty, price, active FROM public.{ITEMS}");
        let pairs = [
            ("code", "code", true),
            ("qty", "qty", false),
            ("price", "price", false),
            ("active", "active", false),
        ];
        let no_price = (1..=DOCS).filter(|i| i % 250 == 0).count() as u64;
        let qty_off = (1..=DOCS).filter(|i| i % 333 == 0).count() as u64;
        let both = (1..=DOCS).filter(|i| i % 250 == 0 && i % 333 == 0).count() as u64;
        for (label, null_as_blank) in [("MongoDB, exact", false), ("MongoDB, NULL as BLANK", true)]
        {
            let options = QueryOptions {
                null_as_blank,
                ..Default::default()
            };
            let c = compare_live(
                label,
                LiveSide {
                    driver: &gd,
                    cfg: &gc,
                    query: mongo_q,
                },
                LiveSide {
                    driver: &pd,
                    cfg: &pc,
                    query: &pg_q,
                },
                &pairs,
                options,
                IN_MEMORY_ROWS,
            )
            .await?;
            // A missing price is NULL against 0: changed unless NULL is
            // BLANK; the rows only one side has all blank (M00001, Z00001,
            // Z00002) are the same then.
            let changed = if null_as_blank {
                qty_off
            } else {
                no_price + qty_off - both
            };
            let lone_blank = if null_as_blank { 3 } else { 0 };
            let want = Counts {
                left: DOCS + 1,
                right: DOCS + 3,
                same: DOCS - changed + lone_blank,
                only_left: 1 - u64::from(null_as_blank),
                only_right: 3 - (lone_blank - u64::from(null_as_blank)),
                changed,
            };
            check(label, &c.counts, &want)?;
        }
        Ok::<(), String>(())
    }
    .await;
    let c1 = exec(gs.as_mut(), "db.dropDatabase()").await;
    let c2 = exec(ps.as_mut(), &pg_drop).await;
    body.unwrap();
    c1.unwrap();
    c2.unwrap();
}

// ---------------------------------------------------------------------------
// Redis against PostgreSQL

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_query_live_redis_against_postgres() {
    let (rc, pc) = (redis(), postgres());
    let (rd, _) = driver(&rc, None);
    let (pd, via_host) = driver(&pc, Some("DBINE_TEST_POSTGRES_HOST"));
    eprintln!("PostgreSQL through a driver host: {via_host}");
    let mut rs = open(&rd, &rc, false).await;
    let cleanup = "DEL dbine_e2e_dcmpq:totals dbine_e2e_dcmpq:regions";
    let body = async {
        exec(rs.as_mut(), cleanup).await?;
        exec(
            rs.as_mut(),
            "HSET dbine_e2e_dcmpq:totals Norte 10.5 Sur 20 Este 7.25",
        )
        .await?;
        exec(rs.as_mut(), "SADD dbine_e2e_dcmpq:regions Norte Sur Este").await?;
        let pg_totals = "SELECT region AS name, SUM(x) AS total FROM (VALUES ('Norte', 10.50), ('Sur', 20.00), ('Este', 7.30), ('Oeste', 1.00)) v(region, x) GROUP BY region";
        let pairs = [("field", "name", true), ("value", "total", false)];
        for (label, absolute, want) in [
            // Este is 7.25 against 7.30; Oeste only in PostgreSQL.
            ("Redis hash, exact", 0.0, (3, 4, 2, 0, 1, 1)),
            ("Redis hash, ±0.1", 0.1, (3, 4, 3, 0, 1, 0)),
        ] {
            let c = compare_live(
                label,
                LiveSide {
                    driver: &rd,
                    cfg: &rc,
                    query: "HGETALL dbine_e2e_dcmpq:totals",
                },
                LiveSide {
                    driver: &pd,
                    cfg: &pc,
                    query: pg_totals,
                },
                &pairs,
                QueryOptions {
                    absolute,
                    ..Default::default()
                },
                IN_MEMORY_ROWS,
            )
            .await?;
            let (left, right, same, only_left, only_right, changed) = want;
            check(
                label,
                &c.counts,
                &Counts {
                    left,
                    right,
                    same,
                    only_left,
                    only_right,
                    changed,
                },
            )?;
        }
        // A one-column answer: compared by its key alone.
        let c = compare_live(
            "Redis set",
            LiveSide {
                driver: &rd,
                cfg: &rc,
                query: "SMEMBERS dbine_e2e_dcmpq:regions",
            },
            LiveSide {
                driver: &pd,
                cfg: &pc,
                query: "SELECT region FROM (VALUES ('Norte'), ('Sur'), ('Oeste')) v(region)",
            },
            &[("value", "region", true)],
            QueryOptions::default(),
            IN_MEMORY_ROWS,
        )
        .await?;
        check(
            "Redis set",
            &c.counts,
            &Counts {
                left: 3,
                right: 3,
                same: 2,
                only_left: 1,
                only_right: 1,
                changed: 0,
            },
        )?;
        Ok::<(), String>(())
    }
    .await;
    let c1 = exec(rs.as_mut(), cleanup).await;
    body.unwrap();
    c1.unwrap();
}

// ---------------------------------------------------------------------------
// Refusals

const WRITES: &str = "dbine_e2e_dcmpq_w";

/// The rows `query` returns on a session to check with.
async fn count(s: &mut dyn Session, query: &str) -> Res<usize> {
    Ok(first_rows(&exec(s, query).await?).len())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn data_compare_query_live_refusals() {
    let (pc, mc, gc) = (postgres(), sqlserver(), mongodb());
    let (pd, via_host) = driver(&pc, Some("DBINE_TEST_POSTGRES_HOST"));
    let (md, _) = driver(&mc, None);
    let (gd, _) = driver(&gc, None);
    eprintln!("PostgreSQL through a driver host: {via_host}");
    let mut ps = open(&pd, &pc, false).await;
    let mut ms = open(&md, &mc, false).await;
    let mut gs = open(&gd, &gc, false).await;
    let pg_drop = format!("DROP TABLE IF EXISTS public.{WRITES}");
    let mssql_drop = format!("IF OBJECT_ID('dbo.{WRITES}') IS NOT NULL DROP TABLE dbo.{WRITES}");
    let _ = exec(gs.as_mut(), "db.dropDatabase()").await;
    let body = async {
        exec(
            ps.as_mut(),
            &format!("{pg_drop}; CREATE TABLE public.{WRITES} (id int); INSERT INTO public.{WRITES} VALUES (1)"),
        )
        .await?;
        exec(
            ms.as_mut(),
            &format!("{mssql_drop}; CREATE TABLE dbo.{WRITES} (id int); INSERT INTO dbo.{WRITES} VALUES (1)"),
        )
        .await?;
        exec(gs.as_mut(), "db.w.insertOne({id: 1})").await?;
        // A batch with two results: which one would be compared?
        let two = "SELECT 1 AS a; SELECT 2 AS b";
        let e = columns_of(&md, &mc, two)
            .await
            .err()
            .ok_or("SQL Server: two results read as one")?
            .to_string();
        if !e.contains("más de un resultado") {
            return Err(format!("SQL Server, two results (probe): {e}"));
        }
        let e = compare_only(
            "two results",
            LiveSide {
                driver: &md,
                cfg: &mc,
                query: two,
            },
            LiveSide {
                driver: &md,
                cfg: &mc,
                query: "SELECT 1 AS a",
            },
            &[("a", "a", true)],
            QueryOptions::default(),
            IN_MEMORY_ROWS,
        )
        .await;
        match e {
            Err(e) if e.contains("más de un resultado") && e.contains("izquierda") => {}
            other => {
                return Err(format!(
                    "SQL Server, two results (compare): {:?}",
                    other.map(|c| c.counts)
                ))
            }
        }
        // Writes that return rows, on read-only sessions: refused, nothing
        // written.
        let writes = [
            (&pd, &pc, format!("INSERT INTO public.{WRITES} VALUES (2) RETURNING id")),
            (
                &pd,
                &pc,
                format!("WITH d AS (DELETE FROM public.{WRITES} RETURNING id) SELECT id FROM d"),
            ),
            (&md, &mc, format!("INSERT INTO dbo.{WRITES} OUTPUT inserted.id VALUES (2)")),
            (&md, &mc, format!("DELETE FROM dbo.{WRITES} OUTPUT deleted.id")),
            (&gd, &gc, "db.w.insertOne({id: 2})".to_string()),
            (&gd, &gc, "db.w.deleteMany({})".to_string()),
        ];
        for (d, cfg, write) in &writes {
            if columns_of(d, cfg, write).await.is_ok() {
                return Err(format!("{}: a read-only session ran «{write}»", cfg.driver));
            }
            let e = compare_only(
                "a write",
                LiveSide {
                    driver: d,
                    cfg,
                    query: write,
                },
                LiveSide {
                    driver: d,
                    cfg,
                    query: write,
                },
                &[("id", "id", true)],
                QueryOptions::default(),
                IN_MEMORY_ROWS,
            )
            .await;
            if e.is_ok() {
                return Err(format!("{}: a read-only compare ran «{write}»", cfg.driver));
            }
        }
        let left = [
            count(ps.as_mut(), &format!("SELECT id FROM public.{WRITES}")).await?,
            count(ms.as_mut(), &format!("SELECT id FROM dbo.{WRITES}")).await?,
            count(gs.as_mut(), "db.w.find({})").await?,
        ];
        if left != [1, 1, 1] {
            return Err(format!("rows after the refused writes: {left:?}"));
        }
        Ok::<(), String>(())
    }
    .await;
    let c1 = exec(ps.as_mut(), &pg_drop).await;
    let c2 = exec(ms.as_mut(), &mssql_drop).await;
    let c3 = exec(gs.as_mut(), "db.dropDatabase()").await;
    body.unwrap();
    c1.unwrap();
    c2.unwrap();
    c3.unwrap();
}

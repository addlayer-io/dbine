//! "Uso de índices" against real servers: a table with a primary key, a
//! foreign key and two secondary indexes; one is read with targeted
//! lookups, the other never. Then the unread one is dropped with the
//! schema-sync script, as "Eliminar índice…" does. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! # TiDB 8 (TIDB_INDEX_USAGE) and OceanBase, besides the containers in integration.rs:
//! docker run -d --name dbine-test-tidb8 -p 25044:4000 pingcap/tidb:v8.5.3
//! docker run -d --name dbine-test-oceanbase -p 25035:2881 -e MODE=slim -e OB_TENANT_PASSWORD=pw oceanbase/oceanbase-ce:4.4.2-lts
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017 \
//!   cargo test -p dbine-driver-mysql --test index_usage -- --ignored --nocapture --test-threads 1
//! ```
//!
//! MariaDB: the test reads with `userstat` off (no counters, a note), then
//! turns it on for the run and puts it back. TiDB: counters from 8.0 on
//! (`TIDB_INDEX_USAGE`); against an older server the indexes come without
//! counters and with a note. OceanBase flushes its counts to
//! `DBA_INDEX_USAGE` in the background (not within a test run): the test
//! checks the catalog, that the counters are readable, and that the primary
//! key is never "unused".

use dbine_driver::{ConnectionConfig, Driver, IndexUsageReport, ObjectRef, QueryOutcome, Session, TableChange};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.trim_end_matches('/').parse().unwrap()));
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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let out = run(s, sql).await;
    let v = out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or_default();
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

const DB: &str = "dbine_ixu";

fn table() -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: "t".into() }
}

async fn report(s: &mut Box<dyn Session>) -> IndexUsageReport {
    s.index_usage(&table()).await.expect("index_usage").expect("supported").derived()
}

/// The table, its rows and statistics.
async fn setup(id: &str, cfg: &ConnectionConfig) -> (Box<dyn Session>, Box<dyn Session>) {
    let d = driver(id);
    assert!(d.supports_index_usage(), "{id}");
    let mut admin = d.connect(cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    run(&mut admin, &format!("CREATE DATABASE {DB}")).await;
    let mut s = d.connect(cfg, Some(DB)).await.expect("connect to the test database");
    run(&mut s, "CREATE TABLE p (id INT PRIMARY KEY)").await;
    run(&mut s, "INSERT INTO p VALUES (1), (2)").await;
    run(
        &mut s,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, pid INT, KEY ix_seeked (a), KEY ix_untouched (b DESC), CONSTRAINT fk_t_p FOREIGN KEY (pid) REFERENCES p (id))",
    )
    .await;
    run(
        &mut s,
        "INSERT INTO t (id, a, b, pid) WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) SELECT i, i, i, 1 FROM n",
    )
    .await;
    run(&mut s, "ANALYZE TABLE p").await;
    run(&mut s, "ANALYZE TABLE t").await;
    (admin, s)
}

async fn lookups(s: &mut Box<dyn Session>) {
    for i in 1..=5 {
        run(s, &format!("SELECT a, id FROM t FORCE INDEX (ix_seeked) WHERE a = {i}")).await;
    }
}

/// A full scan of the table, without its secondary indexes.
async fn table_scan(s: &mut Box<dyn Session>) {
    run(s, "SELECT COUNT(*) FROM t IGNORE INDEX (ix_seeked, ix_untouched, fk_t_p) WHERE b + 0 > 0").await;
}

/// InnoDB: the table scan read the clustered primary key.
fn check_pk_scans(r: &IndexUsageReport) {
    let pk = r.indexes.iter().find(|i| i.name == "PRIMARY").unwrap();
    assert!(pk.scans >= 300 && !pk.unused, "{pk:?}");
}

/// What every engine reports the same: the catalog part.
fn check_catalog(r: &IndexUsageReport) {
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    for n in ["PRIMARY", "ix_seeked", "ix_untouched"] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    let pk = r.indexes.iter().find(|i| i.name == "PRIMARY").unwrap();
    assert!(pk.primary_key && pk.unique && pk.key_columns == ["id"], "{pk:?}");
    let seeked = r.indexes.iter().find(|i| i.name == "ix_seeked").unwrap();
    assert!(!seeked.primary_key && !seeked.unique && seeked.key_columns == ["a"], "{seeked:?}");
    assert_eq!(r.foreign_keys.len(), 1, "{:?}", r.foreign_keys);
    let fk = &r.foreign_keys[0];
    assert_eq!((fk.name.as_deref(), fk.columns.clone(), fk.ref_table.as_str(), fk.ref_columns.clone(), fk.ref_schema.clone()), (Some("fk_t_p"), vec!["pid".to_string()], "p", vec!["id".to_string()], None));
}

/// The counters: the looked-up index read, the other unused.
fn check_usage(r: &IndexUsageReport) {
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} in {r:?}"));
    assert!(r.stats_available, "{r:?}");
    assert!(r.since.as_deref().is_some_and(|s| s.len() == 19), "{:?}", r.since);
    let seeked = get("ix_seeked");
    assert!(seeked.seeks >= 5, "{seeked:?}");
    assert!(seeked.read_share.is_some_and(|v| v > 0.0), "{seeked:?}");
    let untouched = get("ix_untouched");
    assert_eq!((untouched.seeks, untouched.scans, untouched.lookups), (0, 0, 0), "{untouched:?}");
    assert!(untouched.updates > 0 && untouched.unused, "written, never read: {untouched:?}");
    assert_eq!(untouched.read_share, Some(0.0));
}

/// "Eliminar índice…": the sync script of the table without the index.
async fn drop_index(id: &str, s: &mut Box<dyn Session>, name: &str) {
    let tables = s.database_schema().await.unwrap();
    let old = tables.into_iter().find(|t| t.name == "t").expect("t");
    assert!(old.indexes.iter().any(|i| i.name == name), "{:?}", old.indexes);
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != name);
    let script = driver(id).sync_script(&[TableChange::Alter { old, new }]).unwrap();
    eprintln!("{id}: drop script {:?} warnings {:?}", script.statements, script.warnings);
    assert_eq!(script.statements.len(), 1, "{script:?}");
    assert!(script.statements[0].contains(name), "{script:?}");
    for stmt in &script.statements {
        run(s, stmt).await;
    }
    let r = report(s).await;
    assert!(!r.indexes.iter().any(|i| i.name == name), "{r:?}");
}

#[tokio::test]
#[ignore]
async fn mysql_live() {
    let Ok(url) = std::env::var("DBINE_TEST_MYSQL_URL") else {
        eprintln!("DBINE_TEST_MYSQL_URL not set; skipping");
        return;
    };
    let (mut admin, mut s) = setup("mysql", &parse_url("mysql", &url)).await;
    lookups(&mut s).await;
    table_scan(&mut s).await;
    let r = report(&mut s).await;
    eprintln!("{r:#?}");
    check_catalog(&r);
    check_usage(&r);
    check_pk_scans(&r);
    assert!(!r.seek_scan_split, "performance_schema doesn't split seeks from scans");
    assert!(r.indexes.iter().all(|i| i.seek_health.is_none()));
    let untouched = r.indexes.iter().find(|i| i.name == "ix_untouched").unwrap();
    assert_eq!(untouched.key_columns, ["b DESC"]);
    assert!(r.indexes.iter().find(|i| i.name == "ix_seeked").unwrap().size_kb.is_some_and(|k| k > 0), "{r:?}");
    // A login that can't read performance_schema: the indexes, no counters, the privilege named.
    let cfg = parse_url("mysql", &url);
    run(&mut admin, "DROP USER IF EXISTS dbine_ixu_ro").await;
    run(&mut admin, "CREATE USER dbine_ixu_ro IDENTIFIED BY 'Pw_ixu_1'").await;
    run(&mut admin, &format!("GRANT SELECT ON {DB}.* TO dbine_ixu_ro")).await;
    let ro = ConnectionConfig { username: Some("dbine_ixu_ro".into()), password: Some("Pw_ixu_1".into()), ..cfg };
    let mut limited = driver("mysql").connect(&ro, Some(DB)).await.expect("connect as dbine_ixu_ro");
    let r = report(&mut limited).await;
    drop(limited);
    run(&mut admin, "DROP USER dbine_ixu_ro").await;
    eprintln!("{r:#?}");
    check_catalog(&r);
    assert!(!r.stats_available && r.note.as_deref().is_some_and(|n| n.contains("SELECT sobre performance_schema")), "{r:?}");
    assert!(r.indexes.iter().all(|i| !i.unused && i.read_share.is_none()));

    drop_index("mysql", &mut s, "ix_untouched").await;
    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

#[tokio::test]
#[ignore]
async fn mariadb_live() {
    let Ok(url) = std::env::var("DBINE_TEST_MARIADB_URL") else {
        eprintln!("DBINE_TEST_MARIADB_URL not set; skipping");
        return;
    };
    let (mut admin, mut s) = setup("mariadb", &parse_url("mariadb", &url)).await;
    let userstat = scalar(&mut admin, "SELECT @@userstat").await;
    let perf = scalar(&mut admin, "SELECT @@performance_schema").await;
    if perf == "0" {
        // Neither on: the indexes without counters, and how to turn them on.
        run(&mut admin, "SET GLOBAL userstat = 0").await;
        let r = report(&mut s).await;
        check_catalog(&r);
        assert!(!r.stats_available && r.note.as_deref().is_some_and(|n| n.contains("userstat")), "{r:?}");
        assert!(r.indexes.iter().all(|i| !i.unused && i.read_share.is_none()));
    }
    run(&mut admin, "SET GLOBAL userstat = 1").await;
    run(&mut s, "UPDATE t SET b = b + 1 WHERE id <= 20").await;
    lookups(&mut s).await;
    table_scan(&mut s).await;
    let r = report(&mut s).await;
    run(&mut admin, &format!("SET GLOBAL userstat = {userstat}")).await;
    eprintln!("{r:#?}");
    check_catalog(&r);
    check_usage(&r);
    check_pk_scans(&r);
    assert!(r.note.as_deref().is_some_and(|n| n.contains("userstat")), "{r:?}");
    assert!(!r.seek_scan_split);
    drop_index("mariadb", &mut s, "ix_untouched").await;
    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

#[tokio::test]
#[ignore]
async fn tidb_live() {
    let Ok(url) = std::env::var("DBINE_TEST_TIDB_URL") else {
        eprintln!("DBINE_TEST_TIDB_URL not set; skipping");
        return;
    };
    let (mut admin, mut s) = setup("tidb", &parse_url("tidb", &url)).await;
    let version = s.server_version().await.unwrap();
    let major: u32 = version.split("-v").nth(1).and_then(|v| v.split('.').next()).and_then(|m| m.parse().ok()).unwrap_or(0);
    lookups(&mut s).await;
    if major < 8 {
        let r = report(&mut s).await;
        eprintln!("{r:#?}");
        check_catalog(&r);
        assert!(!r.stats_available && r.note.as_deref().is_some_and(|n| n.contains("8.0")), "{r:?}");
    } else {
        // Rows changed after the ANALYZE: what `updates` reads.
        run(&mut s, "UPDATE t SET b = b + 1 WHERE id <= 20").await;
        // TiDB collects both in the background.
        let start = Instant::now();
        let r = loop {
            let r = report(&mut s).await;
            let get = |n: &str| r.indexes.iter().find(|i| i.name == n).cloned().unwrap();
            if (get("ix_seeked").seeks >= 5 && get("ix_untouched").updates > 0) || start.elapsed() > Duration::from_secs(90) {
                break r;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        };
        eprintln!("{r:#?}");
        check_catalog(&r);
        check_usage(&r);
        assert!(r.seek_scan_split);
        let seeked = r.indexes.iter().find(|i| i.name == "ix_seeked").unwrap();
        assert!(seeked.last_read.is_some() && seeked.seek_ratio == Some(1.0), "point lookups are seeks: {seeked:?}");
        let pk = r.indexes.iter().find(|i| i.name == "PRIMARY").unwrap();
        assert!(!pk.unused, "the clustered key isn't counted: {pk:?}");
    }
    drop_index("tidb", &mut s, "ix_untouched").await;
    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

#[tokio::test]
#[ignore]
async fn oceanbase_live() {
    let Ok(url) = std::env::var("DBINE_TEST_OCEANBASE_URL") else {
        eprintln!("DBINE_TEST_OCEANBASE_URL not set; skipping");
        return;
    };
    let (mut admin, mut s) = setup("oceanbase", &parse_url("oceanbase", &url)).await;
    // OceanBase moves its DML counts (DBA_TAB_MODIFICATIONS) and index
    // accesses (DBA_INDEX_USAGE) to the views in the background, minutes
    // later: the counters may still be zero here.
    run(&mut s, "INSERT INTO t (id, a, b, pid) VALUES (1001, 1, 1, 1), (1002, 2, 2, 1)").await;
    lookups(&mut s).await;
    let r = report(&mut s).await;
    eprintln!("{r:#?}");
    check_catalog(&r);
    assert!(r.stats_available && r.note.as_deref().is_some_and(|n| n.contains("DBA_INDEX_USAGE")), "{r:?}");
    assert!(!r.seek_scan_split && r.since.is_none());
    let pk = r.indexes.iter().find(|i| i.name == "PRIMARY").unwrap();
    assert!(pk.updates == 0 && !pk.unused, "{pk:?}");
    let untouched = r.indexes.iter().find(|i| i.name == "ix_untouched").unwrap();
    assert_eq!(untouched.reads, 0, "{untouched:?}");
    assert_eq!(untouched.unused, untouched.updates > 0, "{untouched:?}");
    drop_index("oceanbase", &mut s, "ix_untouched").await;
    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

/// GreptimeDB: its primary key and time index listed, no counters.
#[tokio::test]
#[ignore]
async fn greptimedb_live() {
    let Ok(url) = std::env::var("DBINE_TEST_GREPTIMEDB_URL") else {
        eprintln!("DBINE_TEST_GREPTIMEDB_URL not set; skipping");
        return;
    };
    let cfg = parse_url("greptimedb", &url);
    let d = driver("greptimedb");
    assert!(d.supports_index_usage());
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {DB}")).await;
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS t").await;
    run(&mut s, "CREATE TABLE t (ts TIMESTAMP TIME INDEX, host STRING, v DOUBLE, PRIMARY KEY (host))").await;
    let r = report(&mut s).await;
    eprintln!("{r:#?}");
    run(&mut s, "DROP TABLE t").await;
    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
    assert!(!r.stats_available && r.note.as_deref().is_some_and(|n| n.starts_with("GreptimeDB")), "{r:?}");
    let pk = r.indexes.iter().find(|i| i.name == "PRIMARY").expect("PRIMARY");
    assert!(pk.primary_key && pk.key_columns == ["host"], "{pk:?}");
    assert!(r.indexes.iter().any(|i| i.name == "TIME INDEX" && i.key_columns == ["ts"]), "{r:?}");
}

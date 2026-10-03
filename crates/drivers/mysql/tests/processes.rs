//! The process list and cancelling another session's statement, against
//! real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`mysql://user:pass@host:port`, see tests/integration.rs) and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//!   cargo test -p dbine-driver-mysql --test processes -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::{Duration, Instant};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let o = run(s, sql).await.unwrap();
    match &o.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

async fn connect(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    d.connect(cfg, None).await.unwrap()
}

/// A session sleeping in a statement shows up active with its text, the
/// lister's own row is flagged, an idle session holding a transaction shows
/// its last statement, and cancelling stops the statement but leaves the
/// session usable.
async fn list_and_cancel(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut admin = connect(driver, &cfg).await;
    let own = scalar(&mut admin, "SELECT CONNECTION_ID()").await;

    // Idle with an open transaction.
    let mut holder = connect(driver, &cfg).await;
    let holder_id = scalar(&mut holder, "SELECT CONNECTION_ID()").await;
    run(&mut holder, "CREATE DATABASE IF NOT EXISTS dbine_processes").await.unwrap();
    run(&mut holder, "CREATE TABLE IF NOT EXISTS dbine_processes.t (id INT PRIMARY KEY, v INT)").await.unwrap();
    run(&mut holder, "REPLACE INTO dbine_processes.t VALUES (1, 0)").await.unwrap();
    run(&mut holder, "BEGIN").await.unwrap();
    run(&mut holder, "UPDATE dbine_processes.t SET v = v + 1 WHERE id = 1").await.unwrap();

    let mut worker = connect(driver, &cfg).await;
    let worker_id = scalar(&mut worker, "SELECT CONNECTION_ID()").await;
    let sleeping = tokio::spawn(async move {
        let started = Instant::now();
        let r = run(&mut worker, "SELECT SLEEP(30) AS dbine_processes_test").await;
        (r, started.elapsed(), worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(!w.own && !w.system);
    assert!(list.iter().find(|p| p.id == own).expect("its own session is listed").own);
    let h = list.iter().find(|p| p.id == holder_id).expect("the holder is listed");
    eprintln!("{driver}: {h:#?}");
    assert!(!h.active, "{h:?}");
    assert!(h.status.as_deref().unwrap_or("").contains("transacción abierta"), "{h:?}");
    // MariaDB keeps the last statement only with performance_schema on.
    if driver != "mariadb" {
        assert!(h.sql.as_deref().unwrap_or("").to_ascii_lowercase().contains("update"), "the open transaction's last statement: {h:?}");
    }

    admin.cancel_query(&worker_id).await.unwrap();
    let (_, took, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the statement stopped").unwrap();
    assert!(took < Duration::from_secs(10), "the sleep was cancelled ({took:?})");
    assert_eq!(scalar(&mut worker, "SELECT 1").await, "1", "the session is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");

    // A writer waiting on the holder's row lock names it as its blocker.
    let mut waiter = connect(driver, &cfg).await;
    let waiter_id = scalar(&mut waiter, "SELECT CONNECTION_ID()").await;
    let blocked = tokio::spawn(async move {
        let _ = run(&mut waiter, "BEGIN").await;
        let r = run(&mut waiter, "UPDATE dbine_processes.t SET v = v + 1 WHERE id = 1").await;
        let _ = run(&mut waiter, "ROLLBACK").await;
        r
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == waiter_id).expect("the waiter is listed");
    eprintln!("{driver}: {w:#?}");
    assert_eq!(w.blocked_by.as_deref(), Some(holder_id.as_str()), "{w:?}");
    assert!(w.wait.is_some(), "{w:?}");

    // Ending the holder lets the waiter through (TiDB's pessimistic locks
    // outlive the connection until their TTL, ~20 s).
    admin.kill_session(&holder_id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), blocked).await.expect("the waiter went on").unwrap().unwrap();
    let _ = run(&mut admin, "DROP DATABASE IF EXISTS dbine_processes").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql() {
    list_and_cancel("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb() {
    list_and_cancel("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb() {
    list_and_cancel("tidb", "DBINE_TEST_TIDB_URL").await;
}

/// Engines without SLEEP or transactions: a long statement is listed and
/// cancelled, and the session goes on.
async fn long_query(driver: &str, env: &str, long_sql: &str, marker: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut admin = connect(driver, &cfg).await;
    let list = admin.processes().await.unwrap();
    assert!(list.iter().any(|p| p.own), "its own row is flagged: {list:#?}");

    let mut worker = connect(driver, &cfg).await;
    let sql = long_sql.to_string();
    let running = tokio::spawn(async move {
        let started = Instant::now();
        let r = run(&mut worker, &sql).await;
        (r, started.elapsed(), worker)
    });
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| !p.own && p.sql.as_deref().is_some_and(|q| q.contains(marker))).expect("the long query is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active && !w.system, "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    admin.cancel_query(&w.id).await.unwrap();
    let (r, took, mut worker) = tokio::time::timeout(Duration::from_secs(20), running).await.expect("the query stopped").unwrap();
    eprintln!("{driver}: cancelled after {took:?}: {r:?}");
    assert!(r.is_err(), "the query was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1").await, "1", "the session is still open");
    assert!(admin.cancel_query("x'; DROP TABLE t").await.is_err(), "the id is validated");
}

/// `DBINE_TEST_DATABEND_URL=mysql://root:pw@localhost:3307`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databend() {
    long_query(
        "databend",
        "DBINE_TEST_DATABEND_URL",
        "SELECT COUNT(*) AS dbine_long FROM numbers(1000000000000) WHERE number % 7 = 3",
        "dbine_long",
    )
    .await;
}

/// `DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greptimedb() {
    long_query(
        "greptimedb",
        "DBINE_TEST_GREPTIMEDB_URL",
        "SELECT COUNT(*) AS dbine_long FROM generate_series(1, 100000000000) a",
        "dbine_long",
    )
    .await;
}

/// Manticore lists running queries only, and has nothing that runs long
/// on an empty server: the own row, the ids and an unknown id.
/// `DBINE_TEST_MANTICORE_URL=mysql://localhost:25016`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn manticore() {
    let Some(cfg) = cfg("manticore", "DBINE_TEST_MANTICORE_URL") else {
        eprintln!("DBINE_TEST_MANTICORE_URL not set; skipping");
        return;
    };
    let mut admin = connect("manticore", &cfg).await;
    let own = scalar(&mut admin, "SELECT CONNECTION_ID()").await;
    let list = admin.processes().await.unwrap();
    eprintln!("{list:#?}");
    let me = list.iter().find(|p| p.id == own).expect("its own query is listed");
    assert!(me.own && me.active);
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query("999999").await.is_err(), "an unknown id is an error");
    assert!(admin.kill_session(&own).await.is_err(), "no sessions to end");
}

/// StarRocks / Doris: `SHOW FULL PROCESSLIST`, `KILL QUERY`, no transactions.
/// `DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks() {
    olap("starrocks", "DBINE_TEST_STARROCKS_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris() {
    olap("doris", "DBINE_TEST_DORIS_URL").await;
}

/// Same path (`SHOW FULL PROCESSLIST`, `KILL QUERY`).
/// `DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oceanbase() {
    olap("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}

async fn olap(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut admin = connect(driver, &cfg).await;
    let own = scalar(&mut admin, "SELECT CONNECTION_ID()").await;
    let mut worker = connect(driver, &cfg).await;
    let worker_id = scalar(&mut worker, "SELECT CONNECTION_ID()").await;
    let sleeping = tokio::spawn(async move {
        let started = Instant::now();
        let r = run(&mut worker, "SELECT SLEEP(30) AS dbine_processes_test").await;
        (r, started.elapsed(), worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active && !w.own, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(list.iter().find(|p| p.id == own).expect("its own session is listed").own);
    admin.cancel_query(&worker_id).await.unwrap();
    let (_, took, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the statement stopped").unwrap();
    assert!(took < Duration::from_secs(10), "the sleep was cancelled ({took:?})");
    assert_eq!(scalar(&mut worker, "SELECT 1").await, "1", "the session is still open");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");
}

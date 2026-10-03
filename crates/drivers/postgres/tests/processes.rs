//! The process list and cancelling another session's statement, against
//! real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`postgres://user:pass@host:port/db`, see tests/integration.rs) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
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

/// A session sleeping in a statement shows up active with its text, the
/// lister's own row is flagged, and cancelling stops the statement but
/// leaves the session usable.
async fn list_and_cancel(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own = scalar(&mut admin, "SELECT pg_backend_pid()::text").await;

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_id = scalar(&mut worker, "SELECT pg_backend_pid()::text").await;
    let sleeping = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT pg_sleep(30) AS dbine_processes_test").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(!w.own && !w.system);
    assert!(list.iter().find(|p| p.id == own).expect("its own session is listed").own);

    admin.cancel_query(&worker_id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the sleep was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1::text").await, "1", "the session is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    list_and_cancel("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb() {
    list_and_cancel("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb() {
    list_and_cancel("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    list_and_cancel("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

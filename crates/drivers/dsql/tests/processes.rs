//! The process list and cancelling another session's statement. There's no
//! DSQL emulator: locally it runs against PostgreSQL 16 (as
//! tests/compare.rs), which proves the SQL; a real cluster is reached with
//! the usual connection settings.
//!
//! ```sh
//! docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//! DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test processes -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

async fn open(url: &str) -> Box<dyn Session> {
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "dsql".into(), host: host.into(), port: port.parse().unwrap(), username: Some("postgres".into()), ..Default::default() };
    dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap()
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

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn list_and_cancel() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
        eprintln!("DBINE_TEST_DSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = open(&url).await;
    let own = scalar(&mut admin, "SELECT pg_backend_pid()::text").await;
    let mut worker = open(&url).await;
    let worker_id = scalar(&mut worker, "SELECT pg_backend_pid()::text").await;
    let sleeping = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT pg_sleep(30) AS dbine_processes_test").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(list.iter().find(|p| p.id == own).expect("its own session is listed").own);

    admin.cancel_query(&worker_id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the sleep was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1::text").await, "1", "the session is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");
}

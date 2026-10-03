//! The process list (running requests) and cancelling another request,
//! against a real Couchbase Server already initialized by
//! tests/integration.rs:
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        read_only,
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_couchbase::drivers()[0].connect(c, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

/// A long cross join shows up running with its text, the listing is
/// DBine's own, and cancelling stops the statement.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_and_cancels() {
    let Some(c) = cfg(false) else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_couchbase::drivers()[0];
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = open(&c).await;
    let mut worker = open(&c).await;
    let slow = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT COUNT(1) AS dbine_processes_test FROM ARRAY_RANGE(0,6000) a, ARRAY_RANGE(0,6000) b").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().is_some_and(|s| s.contains("dbine_processes_test")))
        .unwrap_or_else(|| panic!("the worker in {list:#?}"));
    eprintln!("{w:#?}");
    assert!(w.active && !w.own, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(list.iter().any(|p| p.own), "the listing is DBine's own");

    let mut ro = open(&cfg(true).unwrap()).await;
    assert!(ro.cancel_query(&w.id).await.is_err(), "read-only");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), slow).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the statement was cancelled: {r:?}");
    assert!(run(&mut worker, "SELECT RAW 1").await.is_ok(), "the session still works");
    assert!(admin.cancel_query(&w.id).await.is_err(), "it already ended");
    assert!(admin.cancel_query("1\" OR true OR \"").await.is_err(), "the id is validated");
}

//! The process list (queries in flight) and cancelling one, against a real
//! coordinator. Skipped without `DBINE_TEST_TRINO_URL`:
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//!   cargo test -p dbine-driver-trino --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "tpch".into(),
        ..Default::default()
    })
}

/// A long scan of the `tpch` catalog shows up with its text, and
/// cancelling it stops it while the session keeps working.
async fn list_and_cancel(env: &str, driver: &str) {
    let Some(cfg) = cfg(env, driver) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        let r = worker
            .execute("SELECT count(*) AS dbine_processes_test FROM tpch.sf100000.lineitem WHERE comment LIKE '%zz%'", 100, &mut out)
            .await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test"))
        .expect("the running query is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert_eq!(w.user.as_deref(), Some("dbine"));
    assert!(!list.iter().any(|p| p.sql.as_deref().unwrap_or("").contains("dbine-processes")), "the list leaves itself out");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(15), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    let mut out = QueryOutcome::default();
    worker.execute("SELECT 1", 10, &mut out).await.expect("the session still works");
    assert!(admin.cancel_query("x'); DROP").await.is_err(), "the id is validated");
    assert!(admin.cancel_query("20200101_000000_00000_aaaaa").await.is_err(), "an unknown query");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trino() {
    list_and_cancel("DBINE_TEST_TRINO_URL", "trino").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn presto() {
    list_and_cancel("DBINE_TEST_PRESTO_URL", "presto").await;
}

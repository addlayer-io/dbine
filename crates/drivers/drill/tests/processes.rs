//! The process list (running queries) and cancelling one, against a real
//! Drill. Skipped without `DBINE_TEST_DRILL_URL`:
//!
//! ```sh
//! DBINE_TEST_DRILL_URL=http://localhost:25847 \
//!   cargo test -p dbine-driver-drill --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "drill".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

/// A three-way join of the sample data runs for a while: it shows up in the
/// list, and cancelling it stops it while the session keeps working.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn drill() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DRILL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_drill::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    worker.execute("ALTER SESSION SET `planner.enable_nljoin_for_scalar_only` = false", 10, &mut out).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        let r = worker
            .execute(
                "SELECT count(*) AS dbine_processes_test FROM cp.`employee.json` a, cp.`employee.json` b, cp.`employee.json` c
                 WHERE a.employee_id + b.employee_id + c.employee_id > -1",
                100,
                &mut out,
            )
            .await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test"))
        .expect("the running query is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(20), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    let mut out = QueryOutcome::default();
    worker.execute("SELECT 1 FROM (VALUES(1))", 10, &mut out).await.expect("the session still works");
    assert!(admin.cancel_query("../status").await.is_err(), "the id is validated");
    assert!(admin.cancel_query("00000000-0000-0000-0000-000000000000").await.is_err(), "an unknown query");
}

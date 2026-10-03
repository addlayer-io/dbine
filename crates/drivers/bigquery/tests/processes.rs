//! The process list (running and pending jobs) and cancelling a job.
//! Skipped without `DBINE_TEST_BIGQUERY_URL`. The emulator runs every job
//! to the end before answering, so against it this only checks that the
//! list is read and that ids are checked; a real project shows the running
//! job and cancels it:
//!
//! ```sh
//! docker start dbine-test-bigquery
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test processes -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn emulator() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut s = d.connect(&c, Some("ds1")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT 1 AS dbine_processes_test", 10, &mut out).await.unwrap();

    let list = s.processes().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.iter().all(|p| p.active && !p.id.is_empty()), "only jobs in flight");
    assert!(!list.iter().any(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test")), "a finished job isn't listed");
    assert!(s.cancel_query("../projects").await.is_err(), "the id is validated");
}

//! Server jobs (scheduled queries) against the emulator, which doesn't have
//! the Data Transfer Service: the list is empty with a note saying so, the
//! other calls are refused, and ids are checked. Skipped without
//! `DBINE_TEST_BIGQUERY_URL`:
//!
//! ```sh
//! docker start dbine-test-bigquery
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test jobs -- --ignored --nocapture
//! ```
//!
//! A real project (creating its own scheduled query) is
//! `jobs::tests::live_scheduled_queries`, in the crate.

use dbine_driver::{ConnectionConfig, Error};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn emulator() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    assert!(d.capabilities().jobs);
    let mut s = d.connect(&c, Some("ds1")).await.unwrap();

    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.jobs.is_empty());
    assert_eq!(list.notes.len(), 1);
    assert!(list.notes[0].contains("emulador"));

    let id = "projects/1/locations/us/transferConfigs/dbine_test_jobs_x";
    assert!(matches!(s.job_detail(id).await, Err(Error::Unsupported(_))));
    assert!(matches!(s.run_job(id).await, Err(Error::Unsupported(_))));
    assert!(s
        .job_detail("projects/1/locations/us/transferConfigs/../../x")
        .await
        .is_err());
    assert!(s.stop_job("x'; DROP").await.is_err());
}

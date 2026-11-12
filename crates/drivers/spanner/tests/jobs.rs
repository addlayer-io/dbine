//! Server jobs (backup schedules) against the Cloud Spanner emulator
//! (ignored by default), which answers UNIMPLEMENTED to every backup call:
//! the list is empty with a note, and ids are still checked.
//!
//! ```sh
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test jobs -- --ignored
//! ```
//!
//! Against a real instance, the list, the detail, the history (the backups
//! a schedule made) and the script need a schedule made with `gcloud
//! spanner backup-schedules create`; there is none in the emulator.

use dbine_driver::ConnectionConfig;
use serde_json::json;

#[tokio::test]
#[ignore]
async fn the_emulator_has_no_backup_schedules() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    // The instance and database the other tests use (made if missing).
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;

    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: "db1".into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url.as_str()),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let driver = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(driver.capabilities().jobs);
    let mut s = driver.connect(&cfg, None).await.unwrap();

    let list = s.jobs().await.expect("jobs");
    assert!(list.jobs.is_empty());
    assert!(
        list.notes.iter().any(|n| n.contains("emulador")),
        "{:?}",
        list.notes
    );

    // A well-formed id reaches the API (which refuses it); a malformed one
    // never leaves the driver.
    assert!(s.job_detail("db1/diario").await.is_err());
    for bad in ["db1", "db1/../../backups", "db1/a b", "DB1/x", ""] {
        let e = s.job_detail(bad).await.unwrap_err().to_string();
        assert!(e.contains("no es un id"), "{bad:?}: {e}");
        assert!(s.job_runs(bad, 5).await.is_err());
        assert!(s.job_script(bad).await.is_err());
    }
    assert!(s.run_job("db1/diario").await.is_err());
    assert!(s.stop_job("db1/diario").await.is_err());
    assert!(s.set_job_enabled("db1/diario", false).await.is_err());
}

//! Backups against the Cloud Spanner emulator (ignored by default), which
//! answers UNIMPLEMENTED to every backup call: this checks that DBine's
//! statements reach the admin API and fail with a clear message.
//!   DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test backup -- --ignored

use dbine_driver::{BackupAction, ConnectionConfig, Error, QueryOutcome};
use serde_json::json;
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn emulator_has_no_backups() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `db1`" })).send().await;

    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let driver = dbine_driver_spanner::drivers().pop().unwrap();
    let mut s = driver.connect(&cfg, None).await.unwrap();

    assert!(matches!(s.backups(Some("db1")).await, Err(Error::Unsupported(_))));
    let script = driver.backup_script(&BackupAction::Backup { database: Some("db1".into()), options: BTreeMap::new() }).unwrap();
    let mut out = QueryOutcome::default();
    let r = s.execute(&script, 10, &mut out).await;
    assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("DROP BACKUP `nope`", 10, &mut out).await, Err(Error::Unsupported(_))));
    // Malformed DBine statements are refused before reaching the server.
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("CREATE BACKUP b FROM DATABASE db1", 10, &mut out).await, Err(Error::Query(m)) if m.contains("Sintaxis")));
}

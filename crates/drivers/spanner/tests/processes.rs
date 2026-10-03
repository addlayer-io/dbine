//! The process list (`SPANNER_SYS.OLDEST_ACTIVE_QUERIES`) and cancelling a
//! query. The emulator implements neither, so against it this checks that
//! the refusal is an error (not a panic or an empty list taken as real) and
//! that ids are checked:
//!
//! ```sh
//! docker start dbine-test-spanner
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test processes -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn emulator() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
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
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let list = s.processes().await;
    eprintln!("{list:?}");
    assert!(matches!(list, Err(dbine_driver::Error::Unsupported(_))), "the emulator has no SPANNER_SYS");
    assert!(s.cancel_query("1') --").await.is_err(), "the id is validated");
}

//! The process list (`_active_tasks`) and stopping a replication, against
//! a real server (see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test processes -- --ignored
//! ```
//!
//! The replication runs inside the server, so its URLs point at the
//! server's own port 5984.

use dbine_driver::{ConnectionConfig, Session};
use serde_json::json;
use std::time::Duration;

fn cfg(read_only: bool) -> Option<(ConnectionConfig, String)> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    let c = ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    };
    Some((c, url.trim_end_matches('/').to_string()))
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_couchdb::drivers()[0].connect(c, None).await.expect("connect")
}

/// A continuous replication shows up as a user task (indexers and
/// compactions would be system ones) and cancelling stops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_and_stops_a_replication() {
    let Some((c, url)) = cfg(false) else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_couchdb::drivers()[0];
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let auth = url.strip_prefix("http://").and_then(|r| r.split_once('@')).map(|(a, _)| a.to_string()).unwrap();
    let http = reqwest::Client::new();
    http.delete(format!("{url}/dbine_proc_b")).send().await.unwrap();
    http.put(format!("{url}/dbine_proc_a")).send().await.unwrap();
    let inside = |db: &str| format!("http://{auth}@127.0.0.1:5984/{db}");
    let r: serde_json::Value = http
        .post(format!("{url}/_replicate"))
        .json(&json!({ "source": inside("dbine_proc_a"), "target": inside("dbine_proc_b"), "continuous": true, "create_target": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rid = r["_local_id"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();

    let mut s = open(&c).await;
    let mut found = None;
    for _ in 0..20 {
        let list = s.processes().await.unwrap();
        if let Some(p) = list.into_iter().find(|p| p.id == rid) {
            found = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let p = found.expect("the replication is listed");
    eprintln!("{p:#?}");
    assert!(p.active && !p.system && !p.own);
    assert_eq!(p.command.as_deref(), Some("replication"));
    assert!(p.sql.as_deref().unwrap_or("").contains("dbine_proc_b"), "{p:?}");

    assert!(open(&cfg(true).unwrap().0).await.cancel_query(&rid).await.is_err(), "read-only");
    s.cancel_query(&rid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!s.processes().await.unwrap().iter().any(|p| p.id == rid), "the replication stopped");
    assert!(s.cancel_query(&rid).await.is_err(), "it already ended");
    assert!(s.cancel_query("<0.1.0>").await.is_err(), "not a running task");

    http.delete(format!("{url}/dbine_proc_a")).send().await.unwrap();
    http.delete(format!("{url}/dbine_proc_b")).send().await.unwrap();
}

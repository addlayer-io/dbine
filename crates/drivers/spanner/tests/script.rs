//! Editor scripts against the Cloud Spanner emulator (see
//! `integration.rs`): statements one by one, errors placed, and read-write
//! transactions across statements.
//!
//! ```sh
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, ScriptMode, Session, TxState};
use serde_json::json;

async fn connect(url: &str) -> Box<dyn Session> {
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `db1`" })).send().await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url)] {
        cfg.options.insert(k.into(), v.into());
    }
    dbine_driver_spanner::drivers().pop().unwrap().connect(&cfg, None).await.unwrap()
}

async fn count(s: &mut Box<dyn Session>) -> serde_json::Value {
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM script_t", 10, &mut out).await.unwrap();
    out.results[0].rows[0][0].clone()
}

#[tokio::test]
#[ignore]
async fn scripts_and_transactions() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    assert!(d.supports_manual_transactions());
    let mut s = connect(&url).await;
    let _ = s.execute("DROP TABLE script_t", 10, &mut QueryOutcome::default()).await;
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE script_t (id INT64 NOT NULL) PRIMARY KEY (id)", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].tag.as_deref(), Some("CREATE TABLE"));

    // An error stops the script; placed by its line and column when the
    // server gives them (the emulator's REST gateway doesn't: "failed to
    // marshal error message").
    let script = "SELECT 1;\n# comment\nSELECT x\nFROM nope";
    let mut out = QueryOutcome::default();
    let err = s.execute(script, 10, &mut out).await.unwrap_err();
    assert!(err.is_query(), "{err:?}");
    assert_eq!(out.results.len(), 1);
    // The gRPC status is the code, with or without a position.
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!(e.line, Some(3), "{}", e.message);
    assert!(e.code.as_deref().is_some_and(|c| c.chars().all(|c| c.is_ascii_uppercase() || c == '_')), "{e:?}");

    // BEGIN … COMMIT across statements (and runs).
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    let mut out = QueryOutcome::default();
    s.execute("BEGIN; INSERT INTO script_t (id) VALUES (1); INSERT INTO script_t (id) VALUES (2)", 10, &mut out).await.unwrap();
    assert_eq!(out.results[1].rows_affected, Some(1));
    assert_eq!(out.results[1].tag.as_deref(), Some("INSERT"));
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    // Reads see the transaction's own writes.
    assert_eq!(count(&mut s).await, json!(2));
    // DDL isn't allowed inside it.
    assert!(s.execute("CREATE INDEX script_i ON script_t (id)", 10, &mut QueryOutcome::default()).await.is_err());
    s.execute("ROLLBACK", 10, &mut QueryOutcome::default()).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut s).await, json!(0));

    // Manual mode: the first DML opens it, commit() ends it.
    s.set_autocommit(false).await.unwrap();
    s.execute("INSERT INTO script_t (id) VALUES (3)", 10, &mut QueryOutcome::default()).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.execute("INSERT INTO script_t (id) VALUES (4)", 10, &mut QueryOutcome::default()).await.unwrap();
    s.commit().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    s.execute("INSERT INTO script_t (id) VALUES (5)", 10, &mut QueryOutcome::default()).await.unwrap();
    s.rollback().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    assert_eq!(count(&mut s).await, json!(2));
    // Autocommit again: each DML commits by itself.
    s.execute("INSERT INTO script_t (id) VALUES (6)", 10, &mut QueryOutcome::default()).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut s).await, json!(3));
    s.execute("DROP TABLE script_t", 10, &mut QueryOutcome::default()).await.unwrap();
}

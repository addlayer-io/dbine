//! Editor scripts against a real Couchbase Server (provisioned as in
//! tests/integration.rs): statement positions, errors with their code and
//! place, and transactions (`BEGIN WORK`'s txid carried across statements
//! and runs, manual mode, commit / rollback).
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test script -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, ScriptMode, Session, TxState};
use std::time::Duration;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";
const BUCKET: &str = "dbine_script0";
const KS: &str = "dbine_script0._default._default";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn value(s: &mut Box<dyn Session>, key: &str) -> Option<serde_json::Value> {
    let out = run(s, &format!("SELECT RAW v FROM {KS} USE KEYS '{key}'")).await.unwrap();
    out.results[0].rows.first().map(|r| r[0].clone())
}

#[tokio::test]
#[ignore]
async fn scripts_and_transactions() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    assert!(d.script_defaults().continue_on_error && d.supports_manual_transactions());
    // `;` inside a backslash-escaped string doesn't split.
    let units = d.split_script("SELECT 'a\\';b';\nBEGIN WORK;\nCOMMIT");
    assert_eq!(units.iter().map(|u| u.text.as_str()).collect::<Vec<_>>(), ["SELECT 'a\\';b'", "BEGIN WORK", "COMMIT"]);

    let mut s = d.connect(&c, None).await.unwrap();
    if !s.list_databases().await.unwrap().contains(&BUCKET.to_string()) {
        // No replicas: a one-node cluster can't give a transaction's
        // commit the durability a replicated bucket asks for.
        let r = reqwest::Client::new()
            .post(format!("http://{}:{}/pools/default/buckets", c.host, c.options["mgmt_port"]))
            .basic_auth(USER, Some(PASS))
            .form(&[("name", BUCKET), ("ramQuota", "100"), ("bucketType", "couchbase"), ("replicaNumber", "0")])
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap_or_default());
    }
    let mut s = d.connect(&c, Some(BUCKET)).await.unwrap();
    let mut ready = false;
    for _ in 0..60 {
        if run(&mut s, &format!("UPSERT INTO {KS} (KEY, VALUE) VALUES ('k0', {{'v': 0}})")).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ready, "the bucket never took writes");
    run(&mut s, &format!("DELETE FROM {KS} USE KEYS ['k1', 'k2', 'k3']")).await.unwrap();

    // Each statement's place; the failing one's code, line and offset.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1;\nSELECT 2;\n  SELEC 3", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((out.results[0].statement, out.results[1].statement, out.results[1].line), (Some(0), Some(1), Some(2)));
    assert_eq!((e.code.as_deref(), e.line, e.statement), (Some("3000"), Some(3), None), "{e:?}");
    assert!(e.offset.is_some_and(|o| (22..=28).contains(&o)), "{e:?}");

    // BEGIN WORK's txid goes with the next statements, also in later runs.
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    let out = run(&mut s, &format!("BEGIN WORK;\nUPSERT INTO {KS} (KEY, VALUE) VALUES ('k1', {{'v': 1}})")).await.unwrap();
    assert!(out.log.iter().any(|m| m.text == "Transacción iniciada."), "{:?}", out.log);
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    // A failed statement leaves the transaction open (as cbq).
    assert!(run(&mut s, "SELECT * FROM nope_nada").await.is_err());
    assert_eq!(value(&mut s, "k1").await, Some(1.into()), "the transaction sees its own write");
    run(&mut s, "ROLLBACK").await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(value(&mut s, "k1").await, None, "rolled back");

    // Manual mode: the first write opens the transaction, Commit ends it.
    s.set_autocommit(false).await.unwrap();
    run(&mut s, &format!("UPSERT INTO {KS} (KEY, VALUE) VALUES ('k2', {{'v': 2}})")).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    // A second write joins it.
    run(&mut s, &format!("UPSERT INTO {KS} (KEY, VALUE) VALUES ('k3', {{'v': 3}})")).await.unwrap();
    let mut other = d.connect(&c, Some(BUCKET)).await.unwrap();
    assert_eq!(value(&mut other, "k2").await, None, "not visible before the commit");
    s.commit().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(value(&mut other, "k2").await, Some(2.into()));
    assert_eq!(value(&mut other, "k3").await, Some(3.into()));
    // Rollback in manual mode.
    run(&mut s, &format!("DELETE FROM {KS} USE KEYS 'k2'")).await.unwrap();
    s.rollback().await.unwrap();
    assert_eq!(value(&mut other, "k2").await, Some(2.into()));
    // Commit with nothing open is a no-op.
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    run(&mut s, &format!("DELETE FROM {KS} USE KEYS ['k0', 'k1', 'k2', 'k3']")).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
}

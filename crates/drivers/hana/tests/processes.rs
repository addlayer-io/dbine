//! The process list and cancelling another connection's statement, against
//! a real SAP HANA (no Docker image: see tests/integration.rs). The user
//! needs the MONITORING role and SESSION ADMIN. Skipped without the URL:
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:39041 \
//!   cargo test -p dbine-driver-hana --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_HANA_URL").ok()?;
    let rest = url.strip_prefix("hana://")?;
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (host, port) = addr.split_once(':')?;
    Some(ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let o = run(s, sql).await.unwrap();
    match &o.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

/// A connection sleeping in a block shows up active with its text, the
/// lister's own row is flagged, and cancelling stops the statement but
/// leaves the connection usable.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_and_cancels() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_HANA_URL not set; skipping");
        return;
    };
    let d = dbine_driver_hana::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own = scalar(&mut admin, "SELECT CURRENT_CONNECTION FROM DUMMY").await;

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_id = scalar(&mut worker, "SELECT CURRENT_CONNECTION FROM DUMMY").await;
    let sleeping = tokio::spawn(async move {
        let r = run(&mut worker, "DO BEGIN USING SQLSCRIPT_SYNC AS S; /* dbine_processes_test */ CALL S:SLEEP_SECONDS(30); END").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{w:#?}");
    assert!(w.active, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(!w.own);
    assert!(list.iter().find(|p| p.id == own).expect("its own connection is listed").own);

    admin.cancel_query(&worker_id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the sleep was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1 FROM DUMMY").await, "1", "the connection is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own connection");
}

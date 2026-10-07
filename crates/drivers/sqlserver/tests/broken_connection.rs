//! A connection left broken by a cancelled write (the schema compare
//! report: "connection was left in an inconsistent state by a cancelled
//! write and can no longer be used"): the next catalog read reconnects on
//! its own. Against a real server (`DBINE_TEST_SQLSERVER_URL`,
//! `mssql://user:pass@host:port`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test broken_connection -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_cancelled_write_is_survived() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut s = d.connect(&cfg, Some("master")).await.unwrap();
    // A batch of many packets, dropped while it's still being written.
    let big = format!("SELECT 1 /* {} */", "x".repeat(4 * 1024 * 1024));
    let mut broke = false;
    for wait in [0u64, 1, 2, 5, 10, 20] {
        let mut out = QueryOutcome::default();
        let _ = tokio::time::timeout(Duration::from_millis(wait), s.execute(&big, 1, &mut out)).await;
        let mut out = QueryOutcome::default();
        if let Err(e) = s.execute("SELECT 1", 1, &mut out).await {
            eprintln!("after {wait} ms: {e}");
            broke = true;
            break;
        }
        if out.error.as_ref().is_some_and(|e| e.contains("can no longer be used") || e.contains("se perdió la conexión")) {
            eprintln!("after {wait} ms: {:?}", out.error);
            broke = true;
            break;
        }
    }
    if !broke {
        eprintln!("the write finished before every cancel; nothing to check");
        return;
    }
    // The catalog read reconnects instead of failing again and again.
    let objects = s.list_databases().await.expect("a new connection");
    assert!(objects.iter().any(|d| d == "master"), "{objects:?}");
}

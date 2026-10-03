//! The running-query list and `KILL QUERY`, against real servers (see
//! tests/integration.rs for the containers; user root / root). Each test
//! is skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 \
//!   cargo test -p dbine-driver-iotdb --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn cfg(env: &str, read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    Some(ConnectionConfig {
        driver: "iotdb".into(),
        host: url,
        username: Some("root".into()),
        password: Some("root".into()),
        read_only,
        ..Default::default()
    })
}

/// A slow aggregation (two billion empty windows, filtered by `HAVING`)
/// shows up running with its text, the list's own statement is flagged,
/// and `KILL QUERY` stops it.
async fn list_and_kill(env: &str) {
    let Some(rw) = cfg(env, false) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query && !d.capabilities().kill_session);
    let mut admin = d.connect(&rw, None).await.unwrap();
    let mut o = QueryOutcome::default();
    admin.execute("INSERT INTO root.dbineproc.d(time, s) VALUES(1, 1.0)", 10, &mut o).await.unwrap();

    let mut worker = d.connect(&rw, None).await.unwrap();
    let slow = tokio::spawn(async move {
        let mut o = QueryOutcome::default();
        let r = worker
            .execute("SELECT count(s) FROM root.dbineproc.d GROUP BY ([0, 2000000000), 1ms) HAVING count(s) > 5", 10, &mut o)
            .await;
        r.is_err() || o.error.is_some() || !o.errors.is_empty()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.sql.as_deref().unwrap_or("").contains("HAVING count(s) > 5")).unwrap_or_else(|| panic!("the query in {list:#?}"));
    eprintln!("{w:#?}");
    assert!(w.active && !w.own);
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(list.iter().any(|p| p.own), "its own SHOW QUERIES is flagged");

    let mut ro = d.connect(&cfg(env, true).unwrap(), None).await.unwrap();
    assert!(ro.cancel_query(&w.id).await.is_err(), "a read-only connection can't kill");
    admin.cancel_query(&w.id).await.unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(10), slow).await.expect("the query stopped").unwrap();
    assert!(failed, "the query was killed");
    assert!(admin.cancel_query(&w.id).await.is_err(), "nothing left to kill");
    assert!(admin.cancel_query("x'; DELETE DATABASE root.**").await.is_err(), "the id is validated");
    admin.execute("DELETE DATABASE root.dbineproc", 10, &mut o).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn iotdb() {
    list_and_kill("DBINE_TEST_IOTDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn iotdb2() {
    list_and_kill("DBINE_TEST_IOTDB2_URL").await;
}

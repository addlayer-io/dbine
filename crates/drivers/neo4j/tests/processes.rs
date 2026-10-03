//! The process list and cancelling another client's query, against real
//! servers. Each test reads `DBINE_TEST_<ENGINE>_URL` (`user:pass@host:port`
//! or `host:port`, see tests/integration.rs) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let (auth, hp) = url.rsplit_once('@').map_or((None, url.as_str()), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':')?;
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    Some(ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, username: user, password: pass, ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

/// A client busy in a long query shows up running with its text, the
/// lister's own transaction is flagged, and cancelling stops the query but
/// keeps the connection usable.
async fn list_and_cancel(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let busy = tokio::spawn(async move {
        let r = run(&mut worker, "UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*) AS dbine_processes_test").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test"))
        .unwrap_or_else(|| panic!("the worker in {list:#?}"));
    eprintln!("{driver}: {w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("UNWIND"));
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(list.iter().any(|p| p.own), "its own transaction is listed: {list:#?}");
    let id = w.id.clone();

    admin.cancel_query(&id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(15), busy).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    let o = run(&mut worker, "RETURN 1 AS one").await.unwrap();
    assert_eq!(o.results.last().unwrap().rows[0][0], Value::from(1), "the connection is still usable");
    assert!(admin.cancel_query(&id).await.is_err(), "nothing left to cancel");
    assert!(admin.cancel_query("1' OR true //").await.is_err(), "the id is validated");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j() {
    list_and_cancel("neo4j", "DBINE_TEST_NEO4J_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph() {
    list_and_cancel("memgraph", "DBINE_TEST_MEMGRAPH_URL").await;
}

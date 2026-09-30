//! Blocking chains and ending a transaction, against a real Neo4j
//! (`DBINE_TEST_NEO4J_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-neo4j --test blocking -- --ignored`

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: user,
        password: pass,
        read_only,
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reports_the_chain_and_terminates_the_head() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    assert!(d.capabilities().blocking && d.capabilities().kill_session);
    let mut admin = d.connect(&cfg(&url, false), None).await.unwrap();
    run(&mut admin, "MERGE (n:DbineLock {id: 1}) SET n.v = 0").await.unwrap();
    assert!(admin.blocking().await.unwrap().is_empty(), "nothing blocked yet");

    // The head: takes the node's write lock, then keeps its transaction busy.
    let mut head = d.connect(&cfg(&url, false), None).await.unwrap();
    let holding = tokio::spawn(async move {
        run(&mut head, "MATCH (n:DbineLock {id: 1}) SET n.v = 1 WITH n UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)")
            .await
            .is_ok()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The waiter: wants the same node.
    let mut waiter = d.connect(&cfg(&url, false), None).await.unwrap();
    let waiting = tokio::spawn(async move { run(&mut waiter, "MATCH (n:DbineLock {id: 1}) SET n.v = 2").await.is_ok() });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let chain = admin.blocking().await.unwrap();
    let w = chain.iter().find(|s| s.blocked_by.is_some()).unwrap_or_else(|| panic!("a waiter in {chain:?}"));
    assert!(w.sql.as_deref().unwrap_or("").contains("SET n.v = 2"), "{w:?}");
    assert!(w.waited_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(w.object.as_deref().unwrap_or("").starts_with("NODE"), "{w:?}");
    let head_id = w.blocked_by.clone().unwrap();
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert!(h.sql.as_deref().unwrap_or("").contains("SET n.v = 1"), "{h:?}");

    // A read-only connection can look but not kill.
    let mut ro = d.connect(&cfg(&url, true), None).await.unwrap();
    assert!(!ro.blocking().await.unwrap().is_empty());
    assert!(ro.kill_session(&head_id).await.is_err());

    // Terminating the head frees the waiter.
    admin.kill_session(&head_id).await.unwrap();
    let head_ok = tokio::time::timeout(Duration::from_secs(10), holding).await.expect("the head ended").unwrap();
    assert!(!head_ok, "the head was terminated");
    let ok = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the waiter got through").unwrap();
    assert!(ok);
    assert!(admin.blocking().await.unwrap().is_empty());
    assert!(admin.kill_session("neo4j-transaction-1' OR true //").await.is_err(), "the id is validated");
    run(&mut admin, "MATCH (n:DbineLock) DELETE n").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn memgraph_has_no_lock_waits() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "memgraph").unwrap();
    assert!(!d.capabilities().blocking && !d.capabilities().kill_session);
    let mut c = cfg(&url, false);
    c.driver = "memgraph".into();
    let mut s = d.connect(&c, None).await.unwrap();
    assert!(matches!(s.blocking().await, Err(dbine_driver::Error::Unsupported(_))));
    assert!(matches!(s.kill_session("1").await, Err(dbine_driver::Error::Unsupported(_))));
}

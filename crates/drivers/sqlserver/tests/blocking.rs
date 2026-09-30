//! Blocking chains and killing a session, against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test blocking -- --ignored`

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn spid(s: &mut Box<dyn Session>) -> String {
    let o = run(s, "SELECT CAST(@@SPID AS nvarchar(10))").await.unwrap();
    o.results[0].rows[0][0].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reports_the_chain_and_kills_the_head() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF OBJECT_ID('tempdb..##dbine_lock') IS NULL CREATE TABLE ##dbine_lock (id int PRIMARY KEY, v int); DELETE FROM ##dbine_lock; INSERT INTO ##dbine_lock VALUES (1, 0);").await.unwrap();

    // The head: a transaction left open holding the row.
    let mut head = d.connect(&cfg, Some("master")).await.unwrap();
    let head_id = spid(&mut head).await;
    run(&mut head, "BEGIN TRAN; UPDATE ##dbine_lock SET v = 1 WHERE id = 1;").await.unwrap();

    // The waiter: blocks on the same row.
    let mut waiter = d.connect(&cfg, Some("master")).await.unwrap();
    let waiter_id = spid(&mut waiter).await;
    let waiting = tokio::spawn(async move {
        let r = run(&mut waiter, "UPDATE ##dbine_lock SET v = 2 WHERE id = 1;").await;
        (r.is_ok(), waiter)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let chain = admin.blocking().await.unwrap();
    let w = chain.iter().find(|s| s.id == waiter_id).expect("the waiter is in the chain");
    assert_eq!(w.blocked_by.as_deref(), Some(head_id.as_str()));
    assert!(w.sql.as_deref().unwrap_or("").contains("SET v = 2"), "{w:?}");
    assert!(w.waited_ms.unwrap_or(0) >= 500, "{w:?}");
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert!(h.wait.as_deref().unwrap_or("").contains("transacción abierta"), "{h:?}");

    // Killing the head frees the waiter.
    admin.kill_session(&head_id).await.unwrap();
    let (ok, _) = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the waiter got through").unwrap();
    assert!(ok);
    assert!(admin.blocking().await.unwrap().iter().all(|s| s.id != waiter_id));
    assert!(admin.kill_session("1; DROP TABLE x").await.is_err(), "the id is a number");
}

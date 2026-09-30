//! Blocking chains and killing a session, against real servers
//! (`DBINE_TEST_MYSQL_URL`, `DBINE_TEST_MARIADB_URL`, `DBINE_TEST_TIDB_URL`,
//! see tests/integration.rs):
//! `cargo test -p dbine-driver-mysql --test blocking -- --ignored --test-threads=1`

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn conn_id(s: &mut Box<dyn Session>) -> String {
    let o = run(s, "SELECT CONNECTION_ID()").await.unwrap();
    match &o.results[0].rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

async fn chain_and_kill(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.capabilities().blocking && d.capabilities().kill_session);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    run(
        &mut admin,
        "CREATE DATABASE IF NOT EXISTS dbine_lock; DROP TABLE IF EXISTS dbine_lock.t;
         CREATE TABLE dbine_lock.t (id INT PRIMARY KEY, v INT); INSERT INTO dbine_lock.t VALUES (1, 0);",
    )
    .await
    .unwrap();
    assert!(admin.blocking().await.unwrap().is_empty());

    // The head: a transaction left open holding the row.
    let mut head = d.connect(&cfg, Some("dbine_lock")).await.unwrap();
    let head_id = conn_id(&mut head).await;
    run(&mut head, "BEGIN").await.unwrap();
    run(&mut head, "UPDATE t SET v = 1 WHERE id = 1").await.unwrap();

    // The waiter: blocks on the same row.
    let mut waiter = d.connect(&cfg, Some("dbine_lock")).await.unwrap();
    let waiter_id = conn_id(&mut waiter).await;
    let waiting = tokio::spawn(async move {
        let r = run(&mut waiter, "UPDATE t SET v = 2 WHERE id = 1").await;
        (r.map_err(|e| e.to_string()), waiter)
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let chain = admin.blocking().await.unwrap();
    eprintln!("{id}: {chain:#?}");
    let w = chain.iter().find(|s| s.id == waiter_id).expect("the waiter is in the chain");
    assert_eq!(w.blocked_by.as_deref(), Some(head_id.as_str()));
    assert!(w.sql.as_deref().unwrap_or("").to_lowercase().contains("set v = 2") || w.sql.as_deref().unwrap_or("").contains("set `v`"), "{w:?}");
    assert!(w.waited_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(w.object.as_deref().unwrap_or("").contains('t'), "{w:?}");
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert!(h.wait.as_deref().unwrap_or("").contains("transacción abierta"), "{h:?}");

    // Killing the head frees the waiter. TiDB closes the connection at
    // once but its pessimistic locks stay until their TTL runs out (~20 s).
    admin.kill_session(&head_id).await.unwrap();
    let t0 = std::time::Instant::now();
    let (res, _) = tokio::time::timeout(Duration::from_secs(60), waiting).await.expect("the waiter got through").unwrap();
    eprintln!("{id}: freed after {:?}", t0.elapsed());
    assert!(res.is_ok(), "{res:?}");
    // MariaDB refreshes its InnoDB information_schema tables every 100 ms.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(admin.blocking().await.unwrap().iter().all(|s| s.id != waiter_id));
    assert!(admin.kill_session("1; DROP TABLE x").await.is_err(), "the id is a number");
    let _ = run(&mut admin, "DROP DATABASE dbine_lock").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql() {
    chain_and_kill("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb() {
    chain_and_kill("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb() {
    chain_and_kill("tidb", "DBINE_TEST_TIDB_URL").await;
}

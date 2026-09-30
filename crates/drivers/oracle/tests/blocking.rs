//! Blocking chains and killing a session, against a real server. The user
//! needs to read V$SESSION / V$SQL and ALTER SYSTEM, so this takes its own
//! URL (SYSTEM on the test container):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test blocking -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn config(autocommit: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.rsplit_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg.options.insert("autocommit".into(), autocommit.to_string());
    Some(cfg)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn session_id(s: &mut Box<dyn Session>) -> String {
    let o = run(s, "SELECT SYS_CONTEXT('USERENV', 'SID') || ',' || (SELECT serial# FROM v$session WHERE sid = SYS_CONTEXT('USERENV', 'SID')) FROM dual").await.unwrap();
    o.results[0].rows[0][0].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reports_the_chain_and_kills_the_head() {
    let Some(cfg) = config(true) else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(d.capabilities().blocking && d.capabilities().kill_session);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let _ = run(&mut admin, "DROP TABLE dbine_lock PURGE").await;
    run(&mut admin, "CREATE TABLE dbine_lock (id NUMBER PRIMARY KEY, v NUMBER)").await.unwrap();
    run(&mut admin, "INSERT INTO dbine_lock VALUES (1, 0)").await.unwrap();

    // The head: a transaction left open holding the row.
    let manual = config(false).unwrap();
    let mut head = d.connect(&manual, None).await.unwrap();
    let head_id = session_id(&mut head).await;
    run(&mut head, "UPDATE dbine_lock SET v = 1 WHERE id = 1").await.unwrap();

    // The waiter: blocks on the same row.
    let mut waiter = d.connect(&manual, None).await.unwrap();
    let waiter_id = session_id(&mut waiter).await;
    let waiting = tokio::spawn(async move {
        let r = run(&mut waiter, "UPDATE dbine_lock SET v = 2 WHERE id = 1").await;
        (r.map_err(|e| e.to_string()), waiter)
    });
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let chain = admin.blocking().await.unwrap();
    eprintln!("{chain:#?}");
    let w = chain.iter().find(|s| s.id == waiter_id).expect("the waiter is in the chain");
    assert_eq!(w.blocked_by.as_deref(), Some(head_id.as_str()));
    assert!(w.sql.as_deref().unwrap_or("").contains("SET v = 2"), "{w:?}");
    assert!(w.waited_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(w.object.as_deref().unwrap_or("").contains("DBINE_LOCK"), "{w:?}");
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert!(h.wait.as_deref().unwrap_or("").contains("transacción abierta"), "{h:?}");
    assert!(h.sql.as_deref().unwrap_or("").contains("SET v = 1"), "{h:?}");

    // Killing the head frees the waiter.
    admin.kill_session(&head_id).await.unwrap();
    let (res, mut waiter) =
        tokio::time::timeout(Duration::from_secs(20), waiting).await.expect("the waiter got through").unwrap();
    assert!(res.is_ok(), "{res:?}");
    let _ = run(&mut waiter, "ROLLBACK").await;
    assert!(admin.blocking().await.unwrap().iter().all(|s| s.id != waiter_id));
    assert!(admin.kill_session("1; DROP TABLE x").await.is_err(), "the id is sid,serial#");
    assert!(admin.kill_session("1,2' IMMEDIATE --").await.is_err());
    let _ = run(&mut admin, "DROP TABLE dbine_lock PURGE").await;
}

//! Blocking chains and killing a session, against real servers. Each test
//! reads `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`,
//! see tests/integration.rs) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test blocking -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
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

/// Session A holds a row in an open transaction, B waits on it: the
/// chain shows B blocked by A and A idle in its transaction; killing A
/// frees B.
async fn chain(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().blocking && d.capabilities().kill_session);
    let (own_id, begin) = match driver {
        "cockroachdb" => ("SHOW session_id", "BEGIN"),
        "h2" => ("SELECT CAST(SESSION_ID() AS VARCHAR)", "SET AUTOCOMMIT OFF"),
        _ => ("SELECT pg_backend_pid()::text", "BEGIN"),
    };
    let mut admin = d.connect(&cfg, None).await.unwrap();
    run(&mut admin, "DROP TABLE IF EXISTS dbine_lock").await.unwrap();
    run(&mut admin, "CREATE TABLE dbine_lock (id int PRIMARY KEY, v int)").await.unwrap();
    run(&mut admin, "INSERT INTO dbine_lock VALUES (1, 0)").await.unwrap();

    // The head: a transaction left open holding the row.
    let mut head = d.connect(&cfg, None).await.unwrap();
    let head_id = scalar(&mut head, own_id).await;
    run(&mut head, begin).await.unwrap();
    run(&mut head, "UPDATE dbine_lock SET v = 1 WHERE id = 1").await.unwrap();

    // The waiter: blocks on the same row.
    let mut waiter = d.connect(&cfg, None).await.unwrap();
    let waiter_id = scalar(&mut waiter, own_id).await;
    let waiting = tokio::spawn(async move {
        let r = run(&mut waiter, "UPDATE dbine_lock SET v = 2 WHERE id = 1").await;
        (r, waiter)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // YugabyteDB finds a single-statement waiter in its Active Session
    // History, sampled every second: give it a few tries.
    let mut chain = admin.blocking().await.unwrap();
    for _ in 0..10 {
        if chain.iter().any(|s| s.id == waiter_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        chain = admin.blocking().await.unwrap();
    }
    eprintln!("{driver}: {chain:#?}");
    let w = chain.iter().find(|s| s.id == waiter_id).expect("the waiter is in the chain");
    assert_eq!(w.blocked_by.as_deref(), Some(head_id.as_str()));
    assert!(w.sql.as_deref().unwrap_or("").contains("SET v = 2"), "{w:?}");
    assert!(w.waited_ms.unwrap_or(0) >= 500, "{w:?}");
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert!(h.wait.as_deref().unwrap_or("").contains("transacción abierta"), "{h:?}");
    assert!(h.waited_ms.unwrap_or(0) >= 1000, "{h:?}");

    // Killing the head frees the waiter.
    admin.kill_session(&head_id).await.unwrap();
    let (r, _) = tokio::time::timeout(Duration::from_secs(15), waiting).await.expect("the waiter got through").unwrap();
    r.unwrap();
    assert!(admin.blocking().await.unwrap().iter().all(|s| s.id != waiter_id));
    assert!(admin.kill_session("1; DROP TABLE x").await.is_err(), "the id is validated");
    let own = scalar(&mut admin, own_id).await;
    assert!(admin.kill_session(&own).await.is_err(), "not its own session");
    let _ = run(&mut admin, "DROP TABLE IF EXISTS dbine_lock").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    chain("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb() {
    chain("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb() {
    chain("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    chain("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb() {
    chain("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greengage() {
    chain("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cloudberry() {
    chain("cloudberry", "DBINE_TEST_CLOUDBERRY_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn h2() {
    chain("h2", "DBINE_TEST_H2_URL").await;
}

/// The engines without locks between sessions say so.
#[tokio::test]
async fn lockless_engines_are_unsupported() {
    for id in ["materialize", "risingwave", "cratedb", "denodo"] {
        let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap();
        assert!(!d.capabilities().blocking && !d.capabilities().kill_session, "{id}");
    }
}

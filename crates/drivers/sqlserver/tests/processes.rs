//! The process list against real servers (`DBINE_TEST_SQLSERVER_URL`,
//! `DBINE_TEST_BABELFISH_URL`, `mssql://user:pass@host:port`, see
//! tests/integration.rs); skipped without them:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, Session};
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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn spid(s: &mut Box<dyn Session>) -> String {
    let o = run(s, "SELECT CAST(@@SPID AS varchar(10))").await.unwrap();
    o.results[0].rows[0][0].as_str().unwrap().to_string()
}

/// A session in a WAITFOR shows up active with its statement, an idle one
/// holding a transaction shows its last batch, a blocked one names its
/// blocker; cancelling is refused (KILL would end the session) and ending
/// the session works.
async fn list_and_kill(driver: &str, env: &str, lock_table: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().kill_session && !d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own = spid(&mut admin).await;
    run(&mut admin, &format!("IF OBJECT_ID('{lock_table}') IS NULL CREATE TABLE {lock_table} (id int PRIMARY KEY, v int)")).await.unwrap();
    run(&mut admin, &format!("DELETE FROM {lock_table}; INSERT INTO {lock_table} VALUES (1, 0)")).await.unwrap();

    // Idle, holding a row in an open transaction.
    let mut holder = d.connect(&cfg, None).await.unwrap();
    let holder_id = spid(&mut holder).await;
    run(&mut holder, &format!("BEGIN TRAN; UPDATE {lock_table} SET v = 1 WHERE id = 1")).await.unwrap();

    // Running a long statement.
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_id = spid(&mut worker).await;
    let busy = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT 1 AS dbine_processes_test; WAITFOR DELAY '00:00:20'").await;
        (r, worker)
    });

    // Blocked on the holder's row.
    let mut waiter = d.connect(&cfg, None).await.unwrap();
    let waiter_id = spid(&mut waiter).await;
    let table = lock_table.to_string();
    let blocked = tokio::spawn(async move { run(&mut waiter, &format!("UPDATE {table} SET v = 2 WHERE id = 1")).await });
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(!w.own && !w.system, "{w:?}");
    // Babelfish shows a session in WAITFOR as idle.
    if driver == "sqlserver" {
        assert!(w.active && w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
        assert!(w.sql.as_deref().unwrap_or("").to_ascii_uppercase().contains("WAITFOR"), "{w:?}");
    }
    let h = list.iter().find(|p| p.id == holder_id).expect("the holder is listed");
    eprintln!("{driver}: {h:#?}");
    assert!(!h.active, "{h:?}");
    assert_eq!(h.status.as_deref(), Some("inactiva con transacción abierta"));
    assert!(h.sql.as_deref().unwrap_or("").to_ascii_uppercase().contains("UPDATE"), "{h:?}");
    let b = list.iter().find(|p| p.id == waiter_id).expect("the waiter is listed");
    eprintln!("{driver}: {b:#?}");
    assert_eq!(b.blocked_by.as_deref(), Some(holder_id.as_str()), "{b:?}");
    assert!(b.wait.is_some() && b.active, "{b:?}");
    assert!(b.elapsed_ms.unwrap_or(0) >= 1000, "{b:?}");
    // SQL Server shows the statement as it parameterized it.
    assert!(b.sql.as_deref().unwrap_or("").contains("dbine_processes"), "{b:?}");
    assert_eq!(b.command.as_deref(), Some("UPDATE"));
    let me = list.iter().find(|p| p.id == own).expect("its own session is listed");
    assert!(me.own && me.active);
    if driver == "sqlserver" {
        assert!(list.iter().any(|p| p.system), "background sessions are listed");
        assert!(w.cpu_ms.is_some() && w.reads.is_some(), "{w:?}");
    }

    assert!(matches!(admin.cancel_query(&worker_id).await, Err(Error::Unsupported(_))));
    admin.kill_session(&worker_id).await.unwrap();
    let (r, _) = tokio::time::timeout(Duration::from_secs(10), busy).await.expect("the batch stopped").unwrap();
    assert!(r.is_err(), "the session was ended");
    admin.kill_session(&holder_id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), blocked).await.expect("the waiter went on").unwrap().unwrap();
    assert!(admin.kill_session("1; DROP TABLE x").await.is_err(), "the id is a number");
    let _ = run(&mut admin, &format!("DROP TABLE {lock_table}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlserver() {
    list_and_kill("sqlserver", "DBINE_TEST_SQLSERVER_URL", "##dbine_processes").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish() {
    list_and_kill("babelfish", "DBINE_TEST_BABELFISH_URL", "dbo.dbine_processes").await;
}

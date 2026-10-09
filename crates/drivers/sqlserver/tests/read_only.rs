//! Read-only sessions against a real server (`DBINE_TEST_SQLSERVER_URL`, see
//! tests/blocking.rs): what a read-only run changes is rolled back, and the
//! statements hidden after a read in a batch are refused.
//! `cargo test -p dbine-driver-sqlserver --test read_only -- --ignored`

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

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

async fn run(s: &mut dyn Session, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await?;
    match out.error.take() {
        Some(e) => Err(dbine_driver::Error::Query(e)),
        None => Ok(out),
    }
}

async fn count(s: &mut dyn Session) -> i64 {
    let out = run(s, "SELECT COUNT(*) FROM dbo.ro_canary").await.unwrap();
    out.results.iter().rev().find(|r| !r.rows.is_empty()).unwrap().rows[0][0].as_i64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn read_only_runs_change_nothing() {
    let Some(cfg) = cfg() else { return };
    let driver: Arc<dyn Driver> = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut admin = driver.connect(&cfg, Some("tempdb")).await.unwrap();
    run(admin.as_mut(), "IF OBJECT_ID('dbo.ro_canary') IS NOT NULL DROP TABLE dbo.ro_canary; CREATE TABLE dbo.ro_canary (id int)").await.unwrap();
    run(admin.as_mut(), "INSERT INTO dbo.ro_canary VALUES (1)").await.unwrap();

    // The driver side of the guard: in implicit transactions a write is undone by the rollback.
    let mut s = driver.connect(&cfg, Some("tempdb")).await.unwrap();
    s.set_autocommit(false).await.unwrap();
    run(s.as_mut(), "SELECT 1 DELETE FROM dbo.ro_canary").await.unwrap();
    s.rollback().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    assert_eq!(count(admin.as_mut()).await, 1, "the rollback undid the delete");

    // The whole read-only session: hidden writes are refused, reads work, and nothing stays open.
    let mut ro = ReadOnlySession::with_dialect(driver.connect(&cfg, Some("tempdb")).await.unwrap(), driver.script_dialect());
    for sql in ["SELECT 1 DELETE FROM dbo.ro_canary", "SELECT 1 DISABLE TRIGGER ALL ON dbo.ro_canary", "WITH c AS (SELECT 1 x) DELETE FROM dbo.ro_canary"] {
        assert!(run(&mut ro, sql).await.is_err(), "{sql}");
    }
    assert_eq!(count(&mut ro).await, 1);
    let open = run(&mut ro, "SELECT @@TRANCOUNT").await.unwrap();
    assert_eq!(open.results.iter().rev().find(|r| !r.rows.is_empty()).unwrap().rows[0][0].as_i64(), Some(0));
    assert_eq!(count(admin.as_mut()).await, 1);

    run(admin.as_mut(), "DROP TABLE dbo.ro_canary").await.unwrap();
}

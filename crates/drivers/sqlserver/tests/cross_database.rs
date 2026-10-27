//! A query on one database reads another as `db.schema.table`, which is
//! what completion across databases offers (`Driver::cross_database`,
//! issue #20).
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test cross_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

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

#[tokio::test]
#[ignore]
async fn sqlserver() {
    let Some(c) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        return;
    };
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    assert!(d.cross_database());
    let mut s = d.connect(&c, None).await.unwrap();
    for sql in [
        "IF DB_ID('dbine_xdb_a') IS NOT NULL DROP DATABASE dbine_xdb_a",
        "IF DB_ID('dbine_xdb_b') IS NOT NULL DROP DATABASE dbine_xdb_b",
        "CREATE DATABASE dbine_xdb_a",
        "CREATE DATABASE dbine_xdb_b",
        "CREATE TABLE dbine_xdb_a.dbo.t (id INT PRIMARY KEY, name NVARCHAR(10)); INSERT INTO dbine_xdb_a.dbo.t VALUES (1, N'uno'), (2, N'dos')",
        "CREATE TABLE dbine_xdb_b.dbo.u (id INT PRIMARY KEY, amount DECIMAL(10,2)); INSERT INTO dbine_xdb_b.dbo.u VALUES (1, 10.5), (2, 20)",
    ] {
        let mut out = QueryOutcome::default();
        s.execute(sql, 100, &mut out).await.unwrap();
    }
    let mut a = d.connect(&c, Some("dbine_xdb_a")).await.unwrap();
    let mut out = QueryOutcome::default();
    a.execute(
        "SELECT t.name, b.amount FROM dbo.t JOIN dbine_xdb_b.dbo.u AS b ON b.id = t.id ORDER BY t.id",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let r = out.results.last().unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], serde_json::json!("uno"));
    drop(a);
    for sql in ["DROP DATABASE dbine_xdb_a", "DROP DATABASE dbine_xdb_b"] {
        let mut out = QueryOutcome::default();
        s.execute(sql, 10, &mut out).await.unwrap();
    }
}

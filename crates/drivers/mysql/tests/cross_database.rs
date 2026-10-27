//! A query on one database reads another by its name in front, which is
//! what completion across databases offers (`Driver::cross_database`,
//! issue #20).
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test cross_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
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

async fn reads_another_database(id: &str, env: &str) {
    let Some(c) = cfg(id, env) else { return };
    let d = dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    assert!(d.cross_database(), "{id}");
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_xdb_a; DROP DATABASE IF EXISTS dbine_xdb_b;
         CREATE DATABASE dbine_xdb_a; CREATE DATABASE dbine_xdb_b;
         CREATE TABLE dbine_xdb_a.t (id INT PRIMARY KEY, name VARCHAR(10));
         CREATE TABLE dbine_xdb_b.u (id INT PRIMARY KEY, amount DECIMAL(10,2));
         INSERT INTO dbine_xdb_a.t VALUES (1, 'uno'), (2, 'dos');
         INSERT INTO dbine_xdb_b.u VALUES (1, 10.5), (2, 20);",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let mut a = d.connect(&c, Some("dbine_xdb_a")).await.unwrap();
    let mut out = QueryOutcome::default();
    a.execute(
        "SELECT t.name, b.amount FROM t JOIN dbine_xdb_b.u AS b ON b.id = t.id ORDER BY t.id",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let r = out.results.last().unwrap();
    assert_eq!(r.rows.len(), 2, "{id}");
    assert_eq!(r.rows[0][0], serde_json::json!("uno"), "{id}");
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE dbine_xdb_a; DROP DATABASE dbine_xdb_b",
        10,
        &mut out,
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore]
async fn mysql() {
    reads_another_database("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    reads_another_database("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

//! A query on one database reads another as `db.table`, which is what
//! completion across databases offers (`Driver::cross_database`, issue
//! #20). ClickHouse 22.8 is covered by `old_server.rs`.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test cross_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn clickhouse() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    assert!(d.cross_database());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_xdb_a; DROP DATABASE IF EXISTS dbine_xdb_b;
         CREATE DATABASE dbine_xdb_a; CREATE DATABASE dbine_xdb_b;
         CREATE TABLE dbine_xdb_a.t (id UInt64, name String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE dbine_xdb_b.u (id UInt64, amount Decimal(10,2)) ENGINE = MergeTree ORDER BY id;
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
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], serde_json::json!("uno"));
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE dbine_xdb_a; DROP DATABASE dbine_xdb_b",
        10,
        &mut out,
    )
    .await
    .unwrap();
}

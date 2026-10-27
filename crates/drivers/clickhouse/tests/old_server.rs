//! ClickHouse servers older than `http_write_exception_in_output_format`
//! (22.8 and before, issue #19): DBine stops asking for that setting and
//! everything works as on a current server.
//!
//! ```sh
//! docker run -d --name dbine-test-clickhouse-22 -p 25124:8123 -e CLICKHOUSE_USER=dbine \
//!   -e CLICKHOUSE_PASSWORD=dbine clickhouse/clickhouse-server:22.8
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//!   cargo test -p dbine-driver-clickhouse --test old_server -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url =
        reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_22_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

#[tokio::test]
#[ignore]
async fn clickhouse_22_8() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();

    // The reported failure: connecting answered `Unknown setting
    // http_write_exception_in_output_format`.
    let mut s = d.connect(&c, None).await.expect("connects");
    let version = s.server_version().await.unwrap();
    assert!(version.contains("22.8"), "{version}");

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_old_a; DROP DATABASE IF EXISTS dbine_old_b;
         CREATE DATABASE dbine_old_a; CREATE DATABASE dbine_old_b;
         CREATE TABLE dbine_old_a.t (id UInt64, name String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE dbine_old_b.u (id UInt64, amount Decimal(10,2)) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_old_a.t VALUES (1, 'uno'), (2, 'dos');
         INSERT INTO dbine_old_b.u VALUES (1, 10.5), (2, 20);",
        100,
        &mut out,
    )
    .await
    .expect("DDL and inserts run");

    // What the explorer and completion read.
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.iter().any(|x| x == "dbine_old_a") && dbs.iter().any(|x| x == "dbine_old_b"));
    let mut a = d.connect(&c, Some("dbine_old_a")).await.unwrap();
    let objs = a.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == "t"), "{objs:?}");
    let cols = a.columns(&table("t")).await.unwrap();
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "name"]
    );

    // A query in one database that reads the other (issue #20).
    let mut out = QueryOutcome::default();
    a.execute(
        "SELECT t.name, u.amount FROM t JOIN dbine_old_b.u AS u ON u.id = t.id ORDER BY t.id",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let r = out.results.last().unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], serde_json::json!("uno"));

    // Errors still come back as errors, with the server's message.
    let mut out = QueryOutcome::default();
    let err = a
        .execute("SELECT * FROM dbine_old_b.nope", 10, &mut out)
        .await
        .expect_err("unknown table");
    assert!(err.to_string().contains("nope"), "{err}");
    // …and one in the middle of a script stops it there.
    let mut out = QueryOutcome::default();
    assert!(a
        .execute("SELECT 1; SELECT throwIf(1); SELECT 2", 10, &mut out)
        .await
        .is_err());

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE dbine_old_a; DROP DATABASE dbine_old_b",
        10,
        &mut out,
    )
    .await
    .unwrap();
}

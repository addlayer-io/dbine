//! Row estimates and comments through the password test hook against a
//! plain PostgreSQL (DSQL has no emulator):
//!
//! ```sh
//! DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome};

#[tokio::test]
#[ignore]
async fn dsql_stats() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
        eprintln!("DBINE_TEST_DSQL_URL not set; skipping");
        return;
    };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP VIEW IF EXISTS st_v; DROP TABLE IF EXISTS st_t; DROP TABLE IF EXISTS st_nunca; DROP FUNCTION IF EXISTS st_f(int);
         CREATE TABLE st_t (id int PRIMARY KEY);
         INSERT INTO st_t SELECT generate_series(1, 1234);
         ANALYZE st_t;
         CREATE TABLE st_nunca (id int);
         CREATE VIEW st_v AS SELECT id FROM st_t;
         COMMENT ON VIEW st_v IS 'ventas por id';
         CREATE FUNCTION st_f(x int) RETURNS int LANGUAGE sql AS 'SELECT x * 2';
         COMMENT ON FUNCTION st_f(int) IS 'doble';",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);

    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let t = rows.iter().find(|r| r.object.name == "st_t").expect("st_t");
    assert_eq!((t.object.kind.as_str(), t.object.schema.as_deref(), t.rows), (kinds::TABLE, Some("public"), 1234));
    assert!(rows.iter().all(|r| r.object.name != "st_nunca"), "never analyzed");

    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    assert!(c.iter().any(|c| c.object.kind == kinds::VIEW && c.object.name == "st_v" && c.comment == "ventas por id"));
    assert!(c.iter().any(|c| c.object.kind == kinds::FUNCTION && c.object.name == "st_f" && c.comment == "doble"));

    s.execute("DROP VIEW st_v; DROP TABLE st_t; DROP TABLE st_nunca; DROP FUNCTION st_f(int);", 10, &mut out).await.unwrap();
}

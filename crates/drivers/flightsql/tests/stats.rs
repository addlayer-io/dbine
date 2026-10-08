//! Row estimates and view comments from DuckDB behind GizmoSQL:
//!
//! ```sh
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!   cargo test -p dbine-driver-flightsql --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into())),
        password: Some(std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into())),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn flightsql_stats() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP VIEW IF EXISTS st_v; DROP TABLE IF EXISTS st_t;
         CREATE TABLE st_t AS SELECT range AS id FROM range(1234);
         CREATE VIEW st_v AS SELECT id FROM st_t;
         COMMENT ON VIEW st_v IS 'ventas por id'",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);

    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let t = rows.iter().find(|r| r.object.name == "st_t").expect("st_t");
    assert_eq!((t.object.kind.as_str(), t.rows), (kinds::TABLE, 1234));
    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    assert!(c.iter().any(|c| c.object.kind == kinds::VIEW && c.object.name == "st_v" && c.comment == "ventas por id"), "{c:?}");
    s.execute("DROP VIEW st_v; DROP TABLE st_t", 10, &mut out).await.unwrap();
}

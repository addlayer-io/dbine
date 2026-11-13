//! The function catalog (issue #97). DSQL has no emulator: this reads
//! `pg_proc` through the password test hook against a plain PostgreSQL 16.
//!   docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test functions -- --ignored
//! (user postgres, password dbine).

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

#[tokio::test]
#[ignore]
async fn the_catalog_is_pg_proc() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
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
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine")
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dbine_fn CASCADE;
         CREATE SCHEMA dbine_fn;
         CREATE FUNCTION dbine_fn.net_price(x numeric) RETURNS numeric LANGUAGE sql AS 'SELECT x * 2';",
        10,
        &mut out,
    )
    .await
    .unwrap();
    // Dropped before anything can fail.
    let c = s.function_catalog().await;
    s.execute("DROP SCHEMA dbine_fn CASCADE", 10, &mut out)
        .await
        .unwrap();
    let c = c.unwrap();
    eprintln!("dsql: {} entries", c.len());
    let kind = |n: &str| {
        c.iter()
            .find(|f| f.name == n && f.schema.is_empty())
            .map(|f| f.kind)
    };
    assert_eq!(kind("lower"), Some(FunctionKind::Scalar));
    assert_eq!(kind("string_agg"), Some(FunctionKind::Aggregate));
    assert_eq!(kind("row_number"), Some(FunctionKind::Window));
    assert_eq!(kind("generate_series"), Some(FunctionKind::Table));
    let agg = c.iter().find(|f| f.name == "string_agg").unwrap();
    assert!(!agg.description.is_empty());
    for n in [
        "approx_distinct",
        "int4pl",
        "boolin",
        "pg_stat_get_numscans",
        "int4_avg_combine",
        "int8dec",
        "int4range_canonical",
    ] {
        assert!(kind(n).is_none(), "{n}");
    }
    let mine = c.iter().find(|f| f.name == "net_price").unwrap();
    assert_eq!(
        (mine.schema.as_str(), mine.args.as_str()),
        ("dbine_fn", "(x numeric)")
    );
}

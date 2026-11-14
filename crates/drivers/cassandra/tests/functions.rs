//! The function catalog (#97) against real servers:
//! `DBINE_TEST_CASSANDRA_URL=localhost:25402` (cassandra:5) and
//! `DBINE_TEST_SCYLLADB_URL=localhost:25413`, then
//! `cargo test -p dbine-driver-cassandra --test functions -- --ignored`.
//! The user's function is checked where the server allows creating one
//! (`user_defined_functions_enabled` on Cassandra, Lua UDFs on ScyllaDB).

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome, Session};

async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    let d = dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    d.connect(&cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await
}

/// The catalog, with a user function created for it when the server
/// allows it (and dropped afterwards): `Some(udf)` then.
async fn catalog(s: &mut Box<dyn Session>, udf: &str) -> (Vec<FunctionInfo>, bool) {
    run(s, "CREATE KEYSPACE IF NOT EXISTS dbine_fn_it WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")
        .await
        .unwrap();
    let made = run(s, udf).await.is_ok();
    let c = s.function_catalog().await.unwrap();
    run(s, "DROP KEYSPACE dbine_fn_it").await.unwrap();
    (c, made)
}

fn has(c: &[FunctionInfo], n: &str) -> bool {
    c.iter().any(|f| f.name == n)
}

#[tokio::test]
#[ignore]
async fn cassandra_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else {
        return;
    };
    let mut s = open("cassandra", &url).await;
    let v = s.server_version().await.unwrap();
    let (c, made) = catalog(
        &mut s,
        "CREATE FUNCTION dbine_fn_it.twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x * 2;'",
    )
    .await;
    eprintln!("{v}: {} functions, user function created: {made}", c.len());
    for n in [
        "token",
        "writetime",
        "toTimestamp",
        "textAsBlob",
        "count",
        "avg",
    ] {
        assert!(has(&c, n), "{n}");
    }
    assert!(c
        .iter()
        .any(|f| f.name == "avg" && f.kind == FunctionKind::Aggregate));
    let v5 = dbine_driver::functions::version_at_least(&v, "5.0");
    assert_eq!(has(&c, "similarity_cosine"), v5, "{v}");
    assert_eq!(has(&c, "dateOf"), !v5, "{v}");
    assert!(!has(&c, "approx_distinct") && !has(&c, "upper"));
    // The current* functions are 4.0's, and the server runs them.
    let v4 = dbine_driver::functions::version_at_least(&v, "4.0");
    assert_eq!(has(&c, "currentTimestamp"), v4, "{v}");
    if v4 {
        run(&mut s, "SELECT currentTimeUUID(), currentTimestamp(), currentDate(), currentTime(), cast(gossip_generation AS text) FROM system.local")
            .await
            .unwrap();
    }
    if made {
        let f = c.iter().find(|f| f.name == "twice").unwrap();
        assert_eq!(f.schema, "dbine_fn_it");
        assert_eq!(f.args, "(x int)");
        assert_eq!(f.returns, "int");
    }
}

#[tokio::test]
#[ignore]
async fn scylladb_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else {
        return;
    };
    let mut s = open("scylladb", &url).await;
    let v = s.server_version().await.unwrap();
    let (c, made) = catalog(
        &mut s,
        "CREATE FUNCTION dbine_fn_it.twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'",
    )
    .await;
    eprintln!("{v}: {} functions, user function created: {made}", c.len());
    for n in [
        "token",
        "writetime",
        "toTimestamp",
        "dateOf",
        "count",
        "max",
    ] {
        assert!(has(&c, n), "{n}");
    }
    // Cassandra 5.0's, not ScyllaDB's.
    assert!(!has(&c, "mask_inner") && !has(&c, "to_json"));
    // Vector search is ScyllaDB 2025.4's.
    let vs = dbine_driver::functions::version_at_least(&v, "2025.4");
    assert_eq!(has(&c, "similarity_cosine"), vs, "{v}");
    if made {
        assert!(c
            .iter()
            .any(|f| f.name == "twice" && f.schema == "dbine_fn_it"));
    }
}

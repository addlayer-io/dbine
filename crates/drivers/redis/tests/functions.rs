//! The command catalog (#97) against real servers:
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400` (redis:7),
//! `DBINE_TEST_REDIS6_URL` (redis:6.2, without `COMMAND DOCS`),
//! `DBINE_TEST_VALKEY_URL`, `DBINE_TEST_DRAGONFLY_URL`, then
//! `cargo test -p dbine-driver-redis --test functions -- --ignored`.

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome, Session};

async fn session(driver: &str, url: &str, read_only: bool) -> Box<dyn Session> {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        read_only,
        ..Default::default()
    };
    let d = dbine_driver_redis::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    d.connect(&cfg, Some("db0")).await.unwrap()
}

async fn catalog(driver: &str, url: &str) -> (String, Vec<FunctionInfo>) {
    let mut s = session(driver, url, true).await;
    let v = s.server_version().await.unwrap();
    (v, s.function_catalog().await.unwrap())
}

fn find<'a>(c: &'a [FunctionInfo], name: &str) -> Option<&'a FunctionInfo> {
    c.iter().find(|f| f.name == name)
}

fn common(driver: &str, c: &[FunctionInfo]) {
    eprintln!("{driver}: {} commands", c.len());
    assert!(c.len() > 150, "{driver}: {}", c.len());
    assert!(c.iter().all(|f| f.kind == FunctionKind::Command));
    let hset = find(c, "HSET").expect("HSET");
    assert!(hset.args.contains("field value"), "{driver}: {}", hset.args);
    assert!(find(c, "GET").is_some() && find(c, "ZADD").is_some());
    assert!(find(c, "CONFIG GET").is_some(), "{driver}: CONFIG GET");
    // Containers come as their subcommands; internal plumbing is left out.
    assert!(find(c, "CONFIG").is_none(), "{driver}: bare CONFIG");
    assert!(find(c, "REPLCONF").is_none(), "{driver}: REPLCONF");
    // Redis 6's guards against cross-protocol attacks aren't commands.
    assert!(find(c, "POST").is_none() && find(c, "HOST:").is_none());
    assert!(find(c, "SELECTX").is_none());
}

#[tokio::test]
#[ignore]
async fn redis_commands() {
    let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") else {
        return;
    };
    let (v, c) = catalog("redis", &url).await;
    common("redis", &c);
    // From COMMAND DOCS: arguments and the server's summary.
    let hset = find(&c, "HSET").unwrap();
    assert_eq!(hset.args, "key field value [field value ...]");
    assert!(!hset.description.is_empty());
    assert!(find(&c, "CLIENT KILL").is_some_and(|f| !f.description.is_empty()));
    // Hash field expiration is Redis 7.4's; HGETEX is 8.0's.
    let v74 = dbine_driver::functions::version_at_least(&v, "7.4");
    assert_eq!(find(&c, "HEXPIRE").is_some(), v74, "{v}");
    if !dbine_driver::functions::version_at_least(&v, "8.0") {
        assert!(find(&c, "HGETEX").is_none(), "{v}");
    }

    // Inside an open MULTI nothing is sent (it would be queued into the
    // transaction): the static catalog, and EXEC runs only the user's.
    let mut s = session("redis", &url, false).await;
    let mut out = QueryOutcome::default();
    s.execute("MULTI\nSET dbine_fn_probe 1", 10, &mut out)
        .await
        .unwrap();
    let c = s.function_catalog().await.unwrap();
    assert!(find(&c, "HSET").is_some_and(|f| f.description.is_empty()));
    let mut out = QueryOutcome::default();
    s.execute("EXEC\nDEL dbine_fn_probe", 10, &mut out)
        .await
        .unwrap();
    let exec = &out.results[0];
    assert_eq!(exec.rows.len(), 1, "{:?}", exec.rows);
}

#[tokio::test]
#[ignore]
async fn redis6_commands() {
    let Ok(url) = std::env::var("DBINE_TEST_REDIS6_URL") else {
        return;
    };
    let (v, c) = catalog("redis", &url).await;
    common("redis 6", &c);
    // From COMMAND, which lists CLIENT bare: the catalog's subcommands
    // that the server's version has.
    assert!(!dbine_driver::functions::version_at_least(&v, "7.0"), "{v}");
    assert!(find(&c, "CLIENT INFO").is_some() && find(&c, "CLIENT KILL").is_some());
    assert!(find(&c, "CLIENT NO-EVICT").is_none(), "{v}");
    assert!(find(&c, "COMMAND DOCS").is_none(), "{v}");
    assert!(find(&c, "PUBSUB SHARDCHANNELS").is_none(), "{v}");
    assert!(find(&c, "GETEX").is_some_and(|f| f.args.starts_with("key [EX")));
}

#[tokio::test]
#[ignore]
async fn valkey_commands() {
    let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") else {
        return;
    };
    let (_, c) = catalog("valkey", &url).await;
    common("valkey", &c);
    assert!(find(&c, "CLIENT KILL").is_some());
}

#[tokio::test]
#[ignore]
async fn dragonfly_commands() {
    let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") else {
        return;
    };
    let (_, c) = catalog("dragonfly", &url).await;
    common("dragonfly", &c);
}

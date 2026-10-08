//! Row estimates and object comments against real servers, skipped without
//! their env vars (`DBINE_TEST_REDIS_URL`, `DBINE_TEST_VALKEY_URL`,
//! `DBINE_TEST_DRAGONFLY_URL`, as `redis://host:port`):
//!
//! ```sh
//! DBINE_TEST_REDIS_URL=redis://localhost:25400 \
//!   cargo test -p dbine-driver-redis --test stats -- --ignored
//! ```
//!
//! The keys of `db12`, from `INFO keyspace`. Point it only at a
//! `dbine-test-*` server of your own (never `dbine-test-shots-*`): it adds
//! and deletes three `dbine:s:*` keys and touches nothing else.

use dbine_driver::{ConnectionConfig, QueryOutcome};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.trim_end_matches('/').parse().unwrap(), ..Default::default() }
}

async fn check(driver: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&cfg(driver, &url), Some("db12")).await.unwrap();
    let mut out = QueryOutcome::default();
    // Never FLUSHDB: the server may hold someone else's data. Count before,
    // add three keys of our own, count again, delete only ours.
    let before = s.row_estimates().await.unwrap().first().map_or(0, |r| r.rows);
    s.execute("SET dbine:s:a 1\nHSET dbine:s:h f 1\nRPUSH dbine:s:l x y", 10, &mut out).await.unwrap();
    let rows = s.row_estimates().await.unwrap();
    s.execute("DEL dbine:s:a dbine:s:h dbine:s:l", 10, &mut out).await.unwrap();
    assert_eq!(rows.len(), 1, "{driver}: {rows:?}");
    assert_eq!((rows[0].object.kind.as_str(), rows[0].object.name.as_str()), ("database", "db12"));
    assert_eq!(rows[0].rows, before + 3, "{driver}: {rows:?}");
    assert!(s.object_comments().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn redis_stats() {
    check("redis", "DBINE_TEST_REDIS_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn valkey_stats() {
    check("valkey", "DBINE_TEST_VALKEY_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dragonfly_stats() {
    check("dragonfly", "DBINE_TEST_DRAGONFLY_URL").await;
}

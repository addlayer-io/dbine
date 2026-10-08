//! "Propiedades" of a logical database against real servers, skipped
//! without their env vars (`DBINE_TEST_REDIS_URL`, `DBINE_TEST_VALKEY_URL`,
//! `DBINE_TEST_DRAGONFLY_URL`, as `redis://host:port`):
//!
//! ```sh
//! DBINE_TEST_REDIS_URL=redis://localhost:25400 \
//!   cargo test -p dbine-driver-redis --test properties -- --ignored
//! ```
//!
//! Facts only: keys and keys with an expiry of `db9`, read from a session
//! on `db0`.

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
    assert!(d.capabilities().database_properties);
    let mut s9 = d.connect(&cfg(driver, &url), Some("db9")).await.unwrap();
    let mut out = QueryOutcome::default();
    s9.execute("FLUSHDB\nSET dbine:p:a 1\nSET dbine:p:b 2 EX 600\nSET dbine:p:c 3", 10, &mut out).await.unwrap();

    let mut s = d.connect(&cfg(driver, &url), Some("db0")).await.unwrap();
    let p = s.database_properties("db9").await.unwrap();
    assert!(p.fields.is_empty());
    let get = |l: &str| p.info.iter().find(|i| i.label == l).map(|i| i.value.clone());
    assert_eq!(get("Claves").as_deref(), Some("3"), "{driver}: {:?}", p.info);
    assert_eq!(get("Claves con vencimiento (expires)").as_deref(), Some("1"), "{driver}: {:?}", p.info);
    assert!(s.alter_database("db9", &[("x".to_string(), "1".to_string())].into()).await.is_err());

    s9.execute("FLUSHDB", 10, &mut out).await.unwrap();
    let p = s.database_properties("db9").await.unwrap();
    assert_eq!(p.info[0].value, "0");
}

#[tokio::test]
#[ignore]
async fn redis_properties() {
    check("redis", "DBINE_TEST_REDIS_URL").await;
}

#[tokio::test]
#[ignore]
async fn valkey_properties() {
    check("valkey", "DBINE_TEST_VALKEY_URL").await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_properties() {
    check("dragonfly", "DBINE_TEST_DRAGONFLY_URL").await;
}

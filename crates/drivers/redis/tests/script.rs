//! Editor scripts against a real server, as in tests/integration.rs: each
//! command's place, errors with their code and line, and MULTI / EXEC.
//!
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400 cargo test -p dbine-driver-redis --test script -- --ignored`
//! (`DBINE_TEST_VALKEY_URL` / `DBINE_TEST_DRAGONFLY_URL` too).

use dbine_driver::{ConnectionConfig, MessageLevel, QueryOutcome, Session};
use serde_json::json;

async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    let c = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.trim_end_matches('/').parse().unwrap(), ..Default::default() };
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    d.connect(&c, Some("9")).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

async fn check(driver: &str, url: &str) {
    let mut s = open(driver, url).await;
    run(&mut s, "DEL dbine:script:a dbine:script:l").await.unwrap();

    // Each command's place in the script.
    let out = run(&mut s, "SET dbine:script:a 1\n# comment\n  INCR dbine:script:a").await.unwrap();
    assert_eq!((out.results[1].statement, out.results[1].line, out.results[1].offset), (Some(1), Some(3), Some(33)));

    // The failing command: its code and line; the script stops there.
    let mut out = QueryOutcome::default();
    let e = s.execute("GET dbine:script:a\nLPUSH dbine:script:a x\nGET dbine:script:a", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((e.code.as_deref(), e.line, e.offset), (Some("WRONGTYPE"), Some(2), Some(19)), "{e:?}");
    assert_eq!(out.results.len(), 1);
    let e = run(&mut s, "GET a\nNOSUCHCOMMAND x").await.unwrap_err().to_script_error();
    assert_eq!((e.code.as_deref(), e.line), (Some("ERR"), Some(2)), "{e:?}");

    // MULTI / EXEC on the session's connection: queued, then atomic.
    let out = run(&mut s, "MULTI\nINCR dbine:script:a\nRPUSH dbine:script:l x\nEXEC").await.unwrap();
    assert_eq!(out.results[1].rows[0][0], json!("QUEUED"));
    assert!(!out.log.iter().any(|m| m.level == MessageLevel::Warning), "{:?}", out.log);
    assert_eq!(run(&mut s, "GET dbine:script:a").await.unwrap().results[0].rows[0][0], json!("3"));
    // A MULTI left open across runs is pointed out.
    let out = run(&mut s, "MULTI\nINCR dbine:script:a").await.unwrap();
    assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning && m.text.starts_with("MULTI sigue abierto")), "{:?}", out.log);
    let out = run(&mut s, "DISCARD").await.unwrap();
    assert!(!out.log.iter().any(|m| m.level == MessageLevel::Warning));
    assert_eq!(run(&mut s, "GET dbine:script:a").await.unwrap().results[0].rows[0][0], json!("3"));
    run(&mut s, "DEL dbine:script:a dbine:script:l").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn redis_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") else { return };
    check("redis", &url).await;
}

#[tokio::test]
#[ignore]
async fn valkey_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") else { return };
    check("valkey", &url).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") else { return };
    check("dragonfly", &url).await;
}

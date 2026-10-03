//! The client list, cancelling a blocking command and closing a client,
//! against real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`redis://host:port`, see tests/integration.rs) and is skipped without
//! it:
//!
//! ```sh
//! DBINE_TEST_REDIS_URL=redis://localhost:25400 \
//!   cargo test -p dbine-driver-redis --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.trim_start_matches("redis://").trim_end_matches('/');
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    Some(ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap();
    out
}

async fn scalar(s: &mut Box<dyn Session>, text: &str) -> String {
    let o = run(s, text).await;
    match &o.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| (f as i64).to_string()),
        v => v.to_string(),
    }
}

/// A client parked in `BLPOP` shows up running, the lister's own row is
/// flagged, cancelling fails the `BLPOP` but keeps the connection, and
/// closing it drops it from the list.
async fn list_cancel_kill(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let caps = d.capabilities();
    let dragonfly = driver == "dragonfly";
    assert!(caps.processes && caps.kill_session);
    assert_eq!(caps.cancel_query, !dragonfly, "Dragonfly has no CLIENT UNBLOCK");
    let mut admin = d.connect(&cfg, Some("db0")).await.unwrap();
    let own = scalar(&mut admin, "CLIENT ID").await;

    let mut worker = d.connect(&cfg, Some("db3")).await.unwrap();
    let worker_id = scalar(&mut worker, "CLIENT ID").await;
    let blocked = tokio::spawn(async move {
        let mut o = QueryOutcome::default();
        let r = worker.execute("BLPOP dbine:processes:test:empty 30", 100, &mut o).await;
        (r.is_err() || o.error.is_some() || !o.errors.is_empty(), worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(w.active, "{w:?}");
    // Dragonfly doesn't report the command.
    assert_eq!(w.command.as_deref(), (!dragonfly).then_some("BLPOP"));
    assert_eq!(w.database.as_deref(), Some("db3"));
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    assert!(!w.own && !w.system);
    assert!(list.iter().find(|p| p.id == own).expect("its own client is listed").own);

    assert!(admin.cancel_query("1 KILL").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own client");
    assert!(admin.kill_session(&own).await.is_err(), "not its own client");
    // Kept open until it's closed from the admin session (on Dragonfly it
    // stays in its BLPOP: a dropped JoinHandle doesn't stop the task).
    let mut _kept: Option<Box<dyn Session>> = None;
    if dragonfly {
        let e = admin.cancel_query(&worker_id).await.unwrap_err();
        assert!(matches!(e, dbine_driver::Error::Unsupported(_)), "{e:?}");
    } else {
        admin.cancel_query(&worker_id).await.unwrap();
        let (failed, mut worker) = tokio::time::timeout(Duration::from_secs(10), blocked).await.expect("the BLPOP stopped").unwrap();
        assert!(failed, "the BLPOP failed");
        assert_eq!(scalar(&mut worker, "PING").await, "PONG", "the connection is still open");
        assert!(admin.cancel_query(&worker_id).await.is_err(), "nothing left to cancel");
        _kept = Some(worker);
    }

    admin.kill_session(&worker_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!admin.processes().await.unwrap().iter().any(|p| p.id == worker_id), "the worker is gone");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn redis() {
    list_cancel_kill("redis", "DBINE_TEST_REDIS_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn valkey() {
    list_cancel_kill("valkey", "DBINE_TEST_VALKEY_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dragonfly() {
    list_cancel_kill("dragonfly", "DBINE_TEST_DRAGONFLY_URL").await;
}

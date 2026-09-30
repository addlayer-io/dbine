//! Native backups against a real server (see tests/integration.rs):
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400 cargo test -p dbine-driver-redis --test backup -- --ignored`
//! (`DBINE_TEST_VALKEY_URL` / `DBINE_TEST_DRAGONFLY_URL` for those servers).

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome};
use std::collections::BTreeMap;

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.trim_end_matches('/').parse().unwrap(), ..Default::default() }
}

async fn backup(driver: &str, var: &str) {
    let Ok(url) = std::env::var(var) else { panic!("{var} is not set") };
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let spec = d.backup().expect("native backups");
    assert!(spec.server_wide && spec.history && !spec.restore && !spec.delete);
    let mut s = d.connect(&cfg(driver, &url), None).await.unwrap();
    s.execute("SET dbine:backup:k 1", 10, &mut QueryOutcome::default()).await.unwrap();

    let options = BTreeMap::from([("command".to_string(), "SAVE".to_string())]);
    let script = d.backup_script(&BackupAction::Backup { database: None, options }).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.unwrap_or_else(|e| panic!("{script}: {e}"));

    let h = s.backups(None).await.unwrap();
    println!("{driver}: {h:#?}");
    assert_eq!(h.len(), 1);
    let e = &h[0];
    assert!(matches!(e.kind.as_deref(), Some("RDB" | "DFS")), "{e:?}");
    assert!(e.finished.as_deref().is_some_and(|f| f.ends_with('Z')), "{e:?}");
    assert!(!e.restorable);
    s.execute("DEL dbine:backup:k", 10, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn redis_backup() {
    backup("redis", "DBINE_TEST_REDIS_URL").await;
}

#[tokio::test]
#[ignore]
async fn valkey_backup() {
    backup("valkey", "DBINE_TEST_VALKEY_URL").await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_backup() {
    backup("dragonfly", "DBINE_TEST_DRAGONFLY_URL").await;
}

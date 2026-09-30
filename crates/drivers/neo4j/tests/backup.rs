//! Memgraph's snapshots against a real server (see tests/integration.rs):
//! `DBINE_TEST_MEMGRAPH_URL=localhost:27687 cargo test -p dbine-driver-neo4j --test backup -- --ignored`.
//! It replaces the test server's data with the snapshot it takes.

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

async fn count(s: &mut Box<dyn Session>) -> String {
    let mut out = QueryOutcome::default();
    s.execute("MATCH (n:DbineBackup) RETURN count(n) AS c", 10, &mut out).await.unwrap();
    out.results[0].rows[0][0].to_string()
}

#[tokio::test]
#[ignore]
async fn memgraph_snapshot() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { panic!("DBINE_TEST_MEMGRAPH_URL is not set") };
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "memgraph".into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "memgraph").unwrap();
    let spec = d.backup().expect("native backups");
    assert!(spec.restore && spec.history && !spec.delete);
    let mut s = d.connect(&cfg, None).await.unwrap();
    s.execute("MATCH (n:DbineBackup) DELETE n; CREATE (:DbineBackup {v: 1})", 10, &mut QueryOutcome::default()).await.unwrap();

    let script = d.backup_script(&BackupAction::Backup { database: None, options: BTreeMap::new() }).unwrap();
    s.execute(&script, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{script}: {e}"));
    let h = s.backups(Some("memgraph")).await.unwrap();
    println!("{h:#?}");
    let e = h.first().expect("a snapshot");
    assert!(e.restorable && e.size.is_some_and(|n| n > 0) && e.started.is_some(), "{e:?}");

    s.execute("CREATE (:DbineBackup {v: 2})", 10, &mut QueryOutcome::default()).await.unwrap();
    assert_eq!(count(&mut s).await, "2");
    let script = d.backup_script(&BackupAction::Restore { source: e.id.clone(), database: None, options: BTreeMap::new() }).unwrap();
    println!("{script}");
    s.execute(&script, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{script}: {e}"));
    assert_eq!(count(&mut s).await, "1");
    s.execute("MATCH (n:DbineBackup) DELETE n", 10, &mut QueryOutcome::default()).await.unwrap();
}

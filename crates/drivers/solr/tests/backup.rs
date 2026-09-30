//! Native backups against real servers (ignored by default), the same
//! containers as `integration.rs`:
//!
//! ```sh
//! docker exec dbine-test-solrcloud mkdir -p /var/solr/data/backups
//! DBINE_TEST_SOLR_URL=http://localhost:25522 DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523 \
//!   cargo test -p dbine-driver-solr --test backup -- --ignored
//! ```
//! The backup folder is inside SOLR_HOME, so it needs no `solr.allowPaths`.
//! Each test creates and drops its own `dbine_bk*` collections / cores.

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

async fn session(url: &str) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "solr".into(), host: url.into(), ..Default::default() };
    dbine_driver_solr::drivers()[0].connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{text}"));
    out
}

fn script(a: BackupAction) -> String {
    dbine_driver_solr::drivers()[0].backup_script(&a).expect("script")
}

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The documents of a collection, or 0 while it isn't serving yet.
async fn count(s: &mut Box<dyn Session>, core: &str) -> usize {
    let mut out = QueryOutcome::default();
    match s.execute(&format!("GET /solr/{core}/select?q=*:*&rows=10"), 100, &mut out).await {
        Ok(()) => out.results.first().map_or(0, |r| r.rows.len()),
        Err(_) => 0,
    }
}

async fn wait_count(s: &mut Box<dyn Session>, core: &str, n: usize) -> bool {
    for _ in 0..40 {
        if count(s, core).await == n {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

const SEED: &str = r#"[{"id": "1", "t_s": "a"}, {"id": "2", "t_s": "b"}, {"id": "3", "t_s": "c"}]"#;

#[tokio::test]
#[ignore]
async fn solrcloud_backup_restore_delete() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") else { return };
    let mut s = session(&url).await;
    run(&mut s, "DELETE /solr/dbine_bk?if_exists=true\n\nDELETE /solr/dbine_bk_restored?if_exists=true").await;
    run(&mut s, &format!("PUT /solr/dbine_bk\n\nPOST /solr/dbine_bk/update?commit=true\n{SEED}")).await;

    let backup = script(BackupAction::Backup {
        database: None,
        options: opts(&[("mode", "cloud"), ("collection", "dbine_bk"), ("location", "/var/solr/data/backups"), ("set_default", "true")]),
    });
    run(&mut s, &backup).await;
    run(&mut s, &backup).await; // a second, incremental point

    let history = s.backups(Some("dbine_bk")).await.expect("history");
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[0].id, "cloud:dbine_bk/1@/var/solr/data/backups");
    assert_eq!(history[1].kind.as_deref(), Some("Completo"));
    assert!(s.backups(None).await.unwrap().iter().any(|e| e.database.as_deref() == Some("dbine_bk")));

    let restore = script(BackupAction::Restore { source: history[0].id.clone(), database: Some("dbine_bk_restored".into()), options: BTreeMap::new() });
    run(&mut s, &restore).await;
    assert!(wait_count(&mut s, "dbine_bk_restored", 3).await, "the restored collection doesn't have the documents");

    for e in &history {
        run(&mut s, &script(BackupAction::Delete { source: e.id.clone() })).await;
    }
    assert!(s.backups(Some("dbine_bk")).await.unwrap().is_empty());
    run(&mut s, "DELETE /solr/dbine_bk\n\nDELETE /solr/dbine_bk_restored\n\nGET /solr/admin/collections?action=CLUSTERPROP&name=location").await;
}

#[tokio::test]
#[ignore]
async fn standalone_backup_restore_delete() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else { return };
    let mut s = session(&url).await;
    let core = "testcore";
    run(&mut s, &format!("POST /solr/{core}/update?commit=true\n{{\"delete\": {{\"query\": \"*:*\"}}}}\n\nPOST /solr/{core}/update?commit=true\n{SEED}")).await;

    run(&mut s, &script(BackupAction::Backup { database: None, options: opts(&[("mode", "standalone"), ("collection", core), ("snapshot", "dbine_bk")]) })).await;
    // The replication handler backs up in the background.
    let mut entry = None;
    for _ in 0..20 {
        let h = s.backups(Some(core)).await.expect("history");
        if let Some(e) = h.into_iter().find(|e| e.status.as_deref() == Some("success")) {
            entry = Some(e);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let entry = entry.expect("backup in the history");
    assert!(entry.id.starts_with("core:testcore/dbine_bk@/var/solr/data/testcore/data"), "{}", entry.id);

    // Restore into the same core after emptying it.
    run(&mut s, &format!("POST /solr/{core}/update?commit=true\n{{\"delete\": {{\"query\": \"*:*\"}}}}")).await;
    assert!(wait_count(&mut s, core, 0).await);
    run(&mut s, &script(BackupAction::Restore { source: entry.id.clone(), database: Some(core.into()), options: BTreeMap::new() })).await;
    assert!(wait_count(&mut s, core, 3).await, "the restore didn't bring the documents back");

    run(&mut s, &script(BackupAction::Delete { source: entry.id.clone() })).await;
}

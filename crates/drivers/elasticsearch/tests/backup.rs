//! Snapshots against real servers, which need `path.repo` (ignored by default):
//!
//! ```sh
//! docker run -d --name dbine-test-es-backup -p 25530:9200 -e discovery.type=single-node \
//!   -e xpack.security.enabled=false -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" -e path.repo=/tmp/snaps \
//!   docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//! docker run -d --name dbine-test-os-backup -p 25531:9200 -e discovery.type=single-node \
//!   -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
//!   -e "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m" -e path.repo=/tmp/snaps opensearchproject/opensearch:2.17.1
//! DBINE_TEST_ES_BACKUP_URL=http://localhost:25530 DBINE_TEST_OS_BACKUP_URL=http://localhost:25531 \
//!   cargo test -p dbine-driver-elasticsearch --test backup -- --ignored
//! ```

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    out
}

async fn snapshots(id: &str, var: &str) {
    let url = std::env::var(var).unwrap_or_else(|_| panic!("{var} is not set"));
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let spec = d.backup().expect("native backups");
    assert!(spec.server_wide && spec.history && spec.restore && spec.delete);
    let cfg = ConnectionConfig { driver: id.into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.execute("DELETE /dbine_bk,restored-dbine_bk?ignore_unavailable=true", 10, &mut QueryOutcome::default()).await;
    run(&mut s, "PUT /dbine_bk\n\nPOST /dbine_bk/_doc?refresh=true\n{\"t\": \"hola\"}").await;

    let backup = d
        .backup_script(&BackupAction::Backup {
            database: None,
            options: opts(&[("repository", "dbine_repo"), ("location", "/tmp/snaps/dbine"), ("snapshot", "snap-1"), ("indices", "dbine_bk")]),
        })
        .unwrap();
    println!("{backup}");
    run(&mut s, &backup).await;

    let h = s.backups(None).await.unwrap();
    println!("{h:#?}");
    let e = h.iter().find(|e| e.id == "dbine_repo/snap-1").expect("the snapshot");
    assert_eq!(e.status.as_deref(), Some("SUCCESS"));
    assert!(e.restorable && e.started.is_some() && e.finished.is_some());

    let restore = d
        .backup_script(&BackupAction::Restore {
            source: e.id.clone(),
            database: None,
            options: opts(&[("rename_pattern", "(.+)"), ("rename_replacement", "restored-$1")]),
        })
        .unwrap();
    println!("{restore}");
    run(&mut s, &restore).await;
    let out = run(&mut s, "GET /restored-dbine_bk/_count").await;
    assert!(format!("{:?}", out.results).contains('1'), "{:?}", out.results);

    let delete = d.backup_script(&BackupAction::Delete { source: e.id.clone() }).unwrap();
    run(&mut s, &delete).await;
    assert!(!s.backups(None).await.unwrap().iter().any(|e| e.id == "dbine_repo/snap-1"));
    run(&mut s, "DELETE /dbine_bk,restored-dbine_bk\n\nDELETE /_snapshot/dbine_repo").await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch_snapshots() {
    snapshots("elasticsearch", "DBINE_TEST_ES_BACKUP_URL").await;
}

#[tokio::test]
#[ignore]
async fn opensearch_snapshots() {
    snapshots("opensearch", "DBINE_TEST_OS_BACKUP_URL").await;
}

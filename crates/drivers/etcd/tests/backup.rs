//! A snapshot against a real etcd (see tests/integration.rs):
//! `DBINE_TEST_ETCD_URL=http://localhost:25379 cargo test -p dbine-driver-etcd --test backup -- --ignored`

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome};
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn snapshot_save() {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_ETCD_URL").expect("DBINE_TEST_ETCD_URL")).expect("URL");
    let cfg = ConnectionConfig { driver: "etcd".into(), host: url.host_str().unwrap().into(), port: url.port().unwrap_or(0), ..Default::default() };
    let d = dbine_driver_etcd::drivers().remove(0);
    let spec = d.backup().expect("native backups");
    assert!(spec.server_wide && !spec.history && !spec.restore);
    let mut s = d.connect(&cfg, None).await.unwrap();
    s.execute("put dbine/backup/k hola", 10, &mut QueryOutcome::default()).await.unwrap();

    let dir = std::env::temp_dir().join("dbine etcd backup");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("snap.db");
    let _ = std::fs::remove_file(&path);
    let options = BTreeMap::from([("path".to_string(), path.to_string_lossy().into_owned())]);
    let script = d.backup_script(&BackupAction::Backup { database: None, options }).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.unwrap_or_else(|e| panic!("{script}: {e}"));
    println!("{script} → {:?}", out.messages);
    let bytes = std::fs::read(&path).unwrap();
    // A bbolt file: its magic (0xED0CDAED) sits in the meta page at 16..20.
    assert!(bytes.len() > 4096 && bytes[16..20] == 0xED0C_DAEDu32.to_le_bytes(), "{} bytes", bytes.len());
    assert!(!dir.join("snap.db.part").exists());
    std::fs::remove_dir_all(&dir).unwrap();
    s.execute("del dbine/backup/k", 10, &mut QueryOutcome::default()).await.unwrap();
}

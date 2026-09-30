//! Native backups against real servers, each skipped without its variable
//! (see tests/integration.rs for the containers):
//!
//! - `DBINE_TEST_MYSQL_URL`: CLONE LOCAL (installs the clone plugin).
//! - `DBINE_TEST_TIDB_BR_URL`: a TiDB with TiKV (BACKUP needs it; the
//!   unistore image refuses): pd + tikv + tidb containers sharing a volume
//!   at /bk (local:// is written by both TiDB and TiKV).
//! - `DBINE_TEST_MANTICORE_URL`, `DBINE_TEST_GREPTIMEDB_URL`.
//! - `DBINE_TEST_STARROCKS_URL` with `DBINE_TEST_STARROCKS_REPO`, a
//!   repository already created (CREATE REPOSITORY … ON LOCATION "s3://…").
//!
//! `cargo test -p dbine-driver-mysql --test backup -- --ignored --test-threads=1`

use dbine_driver::{BackupAction, ConnectionConfig, Driver, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}: {e}");
    }
    out
}

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test]
#[ignore]
async fn mysql_clone() {
    let Some(cfg) = cfg("mysql", "DBINE_TEST_MYSQL_URL") else { return };
    let d = driver("mysql");
    let spec = d.backup().unwrap();
    assert!(spec.server_wide && spec.history && !spec.restore);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("INSTALL PLUGIN clone SONAME 'mysql_clone.so'", 1, &mut out).await;
    let dir = format!("/var/lib/mysql-files/dbine_clone_{}", now());
    let sql = d.backup_script(&BackupAction::Backup { database: None, options: opts(&[("directory", &dir)]) }).unwrap();
    run(&mut s, &sql).await;
    let h = s.backups(None).await.unwrap();
    let e = h.iter().find(|e| e.id.trim_end_matches('/') == dir).unwrap_or_else(|| panic!("{h:?}"));
    assert_eq!(e.status.as_deref(), Some("Completed"));
    assert!(e.size.unwrap_or(0) > 0 && e.started.as_deref().is_some_and(|t| t.contains('T')));
}

#[tokio::test]
#[ignore]
async fn tidb_backup_restore() {
    let Some(cfg) = cfg("tidb", "DBINE_TEST_TIDB_BR_URL") else { return };
    let d = driver("tidb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP DATABASE IF EXISTS dbine_bk; CREATE DATABASE dbine_bk; CREATE TABLE dbine_bk.t (id INT PRIMARY KEY, v VARCHAR(10)); INSERT INTO dbine_bk.t VALUES (1, 'uno'), (2, 'dos');").await;
    let dest = format!("local:///bk/dbine_bk_{}", now());
    let sql = d
        .backup_script(&BackupAction::Backup { database: Some("dbine_bk".into()), options: opts(&[("destination", &dest), ("checksum", "true")]) })
        .unwrap();
    run(&mut s, &sql).await;
    let h = s.backups(Some("dbine_bk")).await.unwrap();
    let e = h.iter().find(|e| e.id == dest && e.kind.as_deref() == Some("Backup")).unwrap_or_else(|| panic!("{h:?}"));
    assert!(e.restorable, "{e:?}");

    run(&mut s, "DROP DATABASE dbine_bk").await;
    let sql = d
        .backup_script(&BackupAction::Restore { source: e.id.clone(), database: Some("dbine_bk".into()), options: opts(&[]) })
        .unwrap();
    run(&mut s, &sql).await;
    let out = run(&mut s, "SELECT COUNT(*) FROM dbine_bk.t").await;
    assert_eq!(text(&out.results[0].rows[0][0]), "2");
    assert!(s.backups(None).await.unwrap().iter().any(|e| e.kind.as_deref() == Some("Restauración")));
    run(&mut s, "DROP DATABASE dbine_bk").await;
}

#[tokio::test]
#[ignore]
async fn manticore_backup() {
    let Some(cfg) = cfg("manticore", "DBINE_TEST_MANTICORE_URL") else { return };
    let d = driver("manticore");
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE TABLE IF NOT EXISTS dbine_bk (title TEXT)").await;
    let sql = d
        .backup_script(&BackupAction::Backup { database: None, options: opts(&[("directory", "/tmp"), ("tables", "dbine_bk")]) })
        .unwrap();
    let out = run(&mut s, &sql).await;
    let path = text(&out.results[0].rows[0][0]);
    assert!(path.starts_with("/tmp/backup-"), "{path}");
}

#[tokio::test]
#[ignore]
async fn greptime_copy_database() {
    let Some(cfg) = cfg("greptimedb", "DBINE_TEST_GREPTIMEDB_URL") else { return };
    let d = driver("greptimedb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_bk").await;
    run(&mut s, "CREATE TABLE IF NOT EXISTS dbine_bk.m (ts TIMESTAMP TIME INDEX, v DOUBLE)").await;
    run(&mut s, "INSERT INTO dbine_bk.m VALUES (1000, 1.5), (2000, 2.5)").await;
    let path = format!("dbine_bk_{}", now());
    let sql = d.backup_script(&BackupAction::Backup { database: Some("dbine_bk".into()), options: opts(&[("path", &path)]) }).unwrap();
    run(&mut s, &sql).await;
    run(&mut s, "DELETE FROM dbine_bk.m WHERE ts = 1000 OR ts = 2000").await;
    let sql = d
        .backup_script(&BackupAction::Restore { source: path.clone(), database: Some("dbine_bk".into()), options: opts(&[]) })
        .unwrap();
    run(&mut s, &sql).await;
    let out = run(&mut s, "SELECT COUNT(*) FROM dbine_bk.m").await;
    assert_eq!(text(&out.results[0].rows[0][0]), "2");
    run(&mut s, "DROP DATABASE dbine_bk").await;
}

#[tokio::test]
#[ignore]
async fn starrocks_snapshot() {
    let (Some(cfg), Ok(repo)) = (cfg("starrocks", "DBINE_TEST_STARROCKS_URL"), std::env::var("DBINE_TEST_STARROCKS_REPO")) else {
        return;
    };
    let d = driver("starrocks");
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP DATABASE IF EXISTS dbine_bk").await;
    run(&mut s, "CREATE DATABASE dbine_bk").await;
    run(&mut s, "CREATE TABLE dbine_bk.t (id INT, v VARCHAR(10)) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES (\"replication_num\" = \"1\")").await;
    run(&mut s, "INSERT INTO dbine_bk.t VALUES (1, 'uno'), (2, 'dos')").await;
    let label = format!("dbine_bk_{}", now());
    let sql = d
        .backup_script(&BackupAction::Backup { database: Some("dbine_bk".into()), options: opts(&[("repository", &repo), ("label", &label)]) })
        .unwrap();
    run(&mut s, &sql).await;
    // BACKUP is asynchronous.
    let entry = loop {
        let h = s.backups(Some("dbine_bk")).await.unwrap();
        if let Some(e) = h.into_iter().find(|e| e.id.starts_with(&format!("{repo}/{label}/")) && e.restorable) {
            break e;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    };
    let sql = d
        .backup_script(&BackupAction::Restore {
            source: entry.id.clone(),
            database: Some("dbine_bk2".into()),
            options: opts(&[("replication_num", "1")]),
        })
        .unwrap();
    run(&mut s, "DROP DATABASE IF EXISTS dbine_bk2").await;
    run(&mut s, "CREATE DATABASE dbine_bk2").await;
    run(&mut s, &sql).await;
    for _ in 0..60 {
        let out = run(&mut s, "SHOW RESTORE FROM dbine_bk2").await;
        let state = out.results[0].columns.iter().position(|c| c.name == "State").map(|i| text(&out.results[0].rows[0][i]));
        if state.as_deref() == Some("FINISHED") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let out = run(&mut s, "SELECT COUNT(*) FROM dbine_bk2.t").await;
    assert_eq!(text(&out.results[0].rows[0][0]), "2");
    run(&mut s, "DROP DATABASE dbine_bk; DROP DATABASE dbine_bk2").await;
}

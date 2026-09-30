//! Native backups against real servers: back up, find it in the history,
//! restore (and delete, where the engine can). Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! # CrateDB needs path.repo for fs repositories:
//! docker run -d --name dbine-test-cratedb-backup -p 25121:5432 crate:latest \
//!   -Cdiscovery.type=single-node -Cpath.repo=/tmp/crate-repos
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_CRATEDB_BACKUP_URL=postgres://crate@localhost:25121/doc \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//!   cargo test -p dbine-driver-postgres --test backup -- --ignored
//! ```

use dbine_driver::{BackupAction, ConnectionConfig, Driver, QueryOutcome, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 10, &mut out).await;
}

async fn count(s: &mut Box<dyn Session>, sql: &str) -> String {
    let out = run(s, sql).await;
    let v = &out.results[0].rows[0][0];
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let spec = d.backup().unwrap();
    assert!(spec.restore && spec.history && !spec.delete && !spec.server_wide);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    for db in ["dbine_bk", "dbine_bk_copia"] {
        quiet(&mut admin, &format!("DROP DATABASE IF EXISTS {db} CASCADE")).await;
    }
    run(&mut admin, "CREATE DATABASE dbine_bk").await;
    let mut s = d.connect(&cfg, Some("dbine_bk")).await.unwrap();
    run(&mut s, "CREATE TABLE facturas (id INT PRIMARY KEY, total DECIMAL)").await;
    run(&mut s, "INSERT INTO facturas VALUES (1, 10.5), (2, 20)").await;

    let collection = format!("nodelocal://1/dbine-bk-{}", stamp());
    let full = d
        .backup_script(&BackupAction::Backup { database: Some("dbine_bk".into()), options: options(&[("collection", &collection)]) })
        .unwrap();
    run(&mut s, &full).await;
    run(&mut s, "INSERT INTO facturas VALUES (3, 30)").await;
    let incremental = d
        .backup_script(&BackupAction::Backup {
            database: Some("dbine_bk".into()),
            options: options(&[("collection", &collection), ("type", "incremental")]),
        })
        .unwrap();
    run(&mut s, &incremental).await;

    let history = s.backups(Some("dbine_bk")).await.unwrap();
    let mine: Vec<_> = history.iter().filter(|e| e.location.as_deref() == Some(collection.as_str())).collect();
    assert_eq!(mine.len(), 2, "{history:#?}");
    assert_eq!(mine[0].kind.as_deref(), Some("Incremental"));
    let entry = mine[1];
    assert_eq!((entry.kind.as_deref(), entry.status.as_deref(), entry.restorable), (Some("Completo"), Some("completado"), true));
    assert_eq!(entry.database.as_deref(), Some("dbine_bk"));
    assert!(entry.id.starts_with(&format!("{collection}/")), "{}", entry.id);
    assert!(entry.started.as_deref().is_some_and(|t| t.contains('T')), "{entry:?}");
    assert!(entry.size.is_some_and(|n| n > 0), "{entry:?}");
    // Another database's history doesn't show them.
    assert!(s.backups(Some("defaultdb")).await.unwrap().iter().all(|e| e.location.as_deref() != Some(collection.as_str())));

    // Into a new name: the full chain (the incremental's row too).
    let restore = d
        .backup_script(&BackupAction::Restore {
            source: entry.id.clone(),
            database: Some("dbine_bk_copia".into()),
            options: options(&[("from_database", "dbine_bk")]),
        })
        .unwrap();
    run(&mut admin, &restore).await;
    assert_eq!(count(&mut admin, "SELECT count(*) FROM dbine_bk_copia.facturas").await, "3");

    drop(s);
    for db in ["dbine_bk", "dbine_bk_copia"] {
        run(&mut admin, &format!("DROP DATABASE {db} CASCADE")).await;
    }
    // Under its own name from the latest backup, once it's gone.
    let restore = d
        .backup_script(&BackupAction::Restore { source: collection.clone(), database: Some("dbine_bk".into()), options: BTreeMap::new() })
        .unwrap();
    run(&mut admin, &restore).await;
    assert_eq!(count(&mut admin, "SELECT count(*) FROM dbine_bk.facturas").await, "3");
    run(&mut admin, "DROP DATABASE dbine_bk CASCADE").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cratedb() {
    let Some(cfg) = cfg("cratedb", "DBINE_TEST_CRATEDB_BACKUP_URL") else {
        eprintln!("DBINE_TEST_CRATEDB_BACKUP_URL not set; skipping");
        return;
    };
    let d = driver("cratedb");
    let spec = d.backup().unwrap();
    assert!(spec.restore && spec.history && spec.delete && spec.server_wide);
    let mut s = d.connect(&cfg, None).await.unwrap();
    quiet(&mut s, "DROP TABLE IF EXISTS doc.dbine_bk_t").await;
    run(&mut s, "CREATE TABLE doc.dbine_bk_t (id INT PRIMARY KEY, nombre TEXT)").await;
    run(&mut s, "INSERT INTO doc.dbine_bk_t VALUES (1, 'uno'), (2, 'dos')").await;
    run(&mut s, "REFRESH TABLE doc.dbine_bk_t").await;

    let n = stamp();
    let repo = format!("dbine repo {n}");
    let snap = format!("dbine_{n}");
    let backup = d
        .backup_script(&BackupAction::Backup {
            database: None,
            options: options(&[
                ("repository", &repo),
                ("create_repository", "true"),
                ("location", &format!("/tmp/crate-repos/{n}")),
                ("snapshot", &snap),
                ("tables", "doc.dbine_bk_t"),
            ]),
        })
        .unwrap();
    run(&mut s, &backup).await;

    let history = s.backups(None).await.unwrap();
    let entry = history.iter().find(|e| e.id.ends_with(&snap)).unwrap_or_else(|| panic!("{history:#?}"));
    assert_eq!((entry.status.as_deref(), entry.restorable), (Some("completado"), true));
    assert!(entry.details.iter().any(|(k, v)| k == "Tablas" && v.contains("dbine_bk_t")), "{entry:?}");
    assert_eq!(entry.location.as_deref(), Some(format!("/tmp/crate-repos/{n}").as_str()));
    assert!(entry.finished.as_deref().is_some_and(|t| t.ends_with(":00")), "{entry:?}");

    run(&mut s, "DROP TABLE doc.dbine_bk_t").await;
    let restore = d
        .backup_script(&BackupAction::Restore { source: entry.id.clone(), database: None, options: options(&[("tables", "doc.dbine_bk_t")]) })
        .unwrap();
    run(&mut s, &restore).await;
    run(&mut s, "REFRESH TABLE doc.dbine_bk_t").await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM doc.dbine_bk_t").await, "2");

    run(&mut s, &d.backup_script(&BackupAction::Delete { source: entry.id.clone() }).unwrap()).await;
    assert!(s.backups(None).await.unwrap().iter().all(|e| e.id != entry.id));
    run(&mut s, "DROP TABLE doc.dbine_bk_t").await;
    run(&mut s, &format!("DROP REPOSITORY \"{repo}\"")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn h2() {
    let Some(cfg) = cfg("h2", "DBINE_TEST_H2_URL") else {
        eprintln!("DBINE_TEST_H2_URL not set; skipping");
        return;
    };
    let d = driver("h2");
    let spec = d.backup().unwrap();
    assert!(spec.restore && !spec.history && !spec.delete && spec.server_wide);
    let mut s = d.connect(&cfg, None).await.unwrap();
    quiet(&mut s, "DROP TABLE IF EXISTS dbine_bk_t").await;
    run(&mut s, "CREATE TABLE dbine_bk_t (id INT PRIMARY KEY, nombre VARCHAR(20))").await;
    run(&mut s, "INSERT INTO dbine_bk_t VALUES (1, 'uno'), (2, 'd''os')").await;

    let file = format!("/tmp/dbine-bk-{}.sql", stamp());
    run(&mut s, &d.backup_script(&BackupAction::Backup { database: None, options: options(&[("file", &file)]) }).unwrap()).await;
    let zip = file.replace(".sql", ".zip");
    run(&mut s, &d.backup_script(&BackupAction::Backup { database: None, options: options(&[("file", &zip), ("format", "zip")]) }).unwrap()).await;

    // The script replaces the table (DROP), so the extra row goes away.
    run(&mut s, "INSERT INTO dbine_bk_t VALUES (3, 'tres')").await;
    let restore = d.backup_script(&BackupAction::Restore { source: file.clone(), database: None, options: BTreeMap::new() }).unwrap();
    run(&mut s, &restore).await;
    assert_eq!(count(&mut s, "SELECT count(*) FROM dbine_bk_t").await, "2");
    assert_eq!(count(&mut s, "SELECT nombre FROM dbine_bk_t WHERE id = 2").await, "d'os");
    run(&mut s, "DROP TABLE dbine_bk_t").await;
    eprintln!("left on the H2 server: {file}, {zip}");
}

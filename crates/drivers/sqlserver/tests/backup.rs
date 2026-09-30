//! Native backups against a real server (`DBINE_TEST_SQLSERVER_URL`, see
//! tests/blocking.rs): full and differential backups, the history, a
//! restore as a new database (MOVE) and one over an existing database
//! (single-user).
//! `cargo test -p dbine-driver-sqlserver --test backup -- --ignored`

use dbine_driver::{BackupAction, ConnectionConfig, Driver, QueryOutcome, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn count(s: &mut Box<dyn Session>, sql: &str) -> i64 {
    let out = run(s, sql).await.unwrap();
    let v = &out.results.iter().rev().find(|r| !r.rows.is_empty()).unwrap().rows[0][0];
    v.as_i64().unwrap_or_else(|| panic!("not a number: {v:?}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn backup_history_and_restore() {
    let Some(cfg) = cfg() else { return };
    let d: Arc<dyn Driver> = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let spec = d.backup().unwrap();
    assert_eq!(spec.script_database, "master");
    let mut m = d.connect(&cfg, Some(spec.script_database)).await.unwrap();
    let (src, copy) = ("dbine_bk_src", "dbine_bk_co'py");
    for db in [src, copy] {
        let q = db.replace('\'', "''");
        run(&mut m, &format!("IF DB_ID(N'{q}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END")).await.unwrap();
    }
    run(&mut m, &format!("CREATE DATABASE [{src}]")).await.unwrap();
    run(&mut m, &format!("CREATE TABLE [{src}].dbo.t (id int PRIMARY KEY); INSERT INTO [{src}].dbo.t VALUES (1), (2);")).await.unwrap();

    // Full to the default folder, then a differential with one more row.
    let full = d.backup_script(&BackupAction::Backup { database: Some(src.into()), options: o(&[("verify", "true")]) }).unwrap();
    let out = run(&mut m, &full).await.unwrap();
    let file = out.results.last().unwrap().rows[0][1].as_str().unwrap().to_string();
    assert!(file.contains("dbine_bk_src_full_") && file.ends_with(".bak"), "{file}");
    run(&mut m, &format!("INSERT INTO [{src}].dbo.t VALUES (3);")).await.unwrap();
    let diff = d
        .backup_script(&BackupAction::Backup { database: Some(src.into()), options: o(&[("kind", "differential"), ("path", &file)]) })
        .unwrap();
    run(&mut m, &diff).await.unwrap();

    let history = m.backups(Some(src)).await.unwrap();
    assert!(history.len() >= 2, "{history:?}");
    let (d_entry, f_entry) = (&history[0], &history[1]);
    assert_eq!(d_entry.kind.as_deref(), Some("Diferencial"));
    assert_eq!(f_entry.kind.as_deref(), Some("Completo"));
    assert_eq!(f_entry.id, format!("{file}|1"));
    assert_eq!(d_entry.id, format!("{file}|2"));
    assert!(f_entry.restorable && d_entry.restorable);
    assert!(f_entry.size.unwrap() > 0);
    assert!(m.backups(None).await.unwrap().len() >= 2);

    // As a new database: the files are moved; full NORECOVERY + differential.
    let restore = |source: &str, db: &str, opts: &[(&str, &str)]| {
        d.backup_script(&BackupAction::Restore { source: source.into(), database: Some(db.into()), options: o(opts) }).unwrap()
    };
    run(&mut m, &restore(&f_entry.id, copy, &[("recovery", "NORECOVERY")])).await.unwrap();
    run(&mut m, &restore(&d_entry.id, copy, &[])).await.unwrap();
    assert_eq!(count(&mut m, "SELECT COUNT(*) FROM [dbine_bk_co'py].dbo.t").await, 3);
    let moved = count(&mut m, "SELECT COUNT(*) FROM sys.master_files WHERE database_id = DB_ID(N'dbine_bk_co''py') AND physical_name LIKE N'%dbine_bk_co''py%'").await;
    assert_eq!(moved, 2);

    // Over the existing source, with another session connected to it.
    let mut other = d.connect(&cfg, Some(src)).await.unwrap();
    run(&mut other, "SELECT 1").await.unwrap();
    run(&mut m, &format!("DELETE FROM [{src}].dbo.t;")).await.unwrap();
    run(&mut m, &restore(&f_entry.id, src, &[])).await.unwrap();
    assert_eq!(count(&mut m, "SELECT COUNT(*) FROM dbine_bk_src.dbo.t").await, 2);
    let access = run(&mut m, "SELECT CAST(DATABASEPROPERTYEX(N'dbine_bk_src', 'UserAccess') AS nvarchar(20))").await.unwrap();
    assert_eq!(access.results[0].rows[0][0].as_str(), Some("MULTI_USER"));

    // A failing restore leaves the database multi-user.
    assert!(run(&mut m, &restore("/nonexistent/x.bak", src, &[("relocate", "false")])).await.is_err());
    let access = run(&mut m, "SELECT CAST(DATABASEPROPERTYEX(N'dbine_bk_src', 'UserAccess') AS nvarchar(20))").await.unwrap();
    assert_eq!(access.results[0].rows[0][0].as_str(), Some("MULTI_USER"));

    for db in [src, copy] {
        run(&mut m, &format!("ALTER DATABASE [{}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{}];", db.replace(']', "]]"), db.replace(']', "]]")))
            .await
            .unwrap();
    }
    run(&mut m, "EXEC msdb.dbo.sp_delete_database_backuphistory N'dbine_bk_src'").await.unwrap();
    println!("backup file left on the server: {file}");
}

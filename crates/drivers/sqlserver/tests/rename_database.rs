//! "Renombrar base…" against a real server: the statements of
//! `rename_database_script` run one by one in `master`, the way the app does
//! it, while another session is open on the database. Reads
//! `DBINE_TEST_SQLSERVER_URL` and `DBINE_TEST_BABELFISH_URL`
//! (`mssql://user:pass@host:port`), skipped without them:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test rename_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use serde_json::Value;
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    match &out.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        Value::Null => "NULL".into(),
        v => v.to_string(),
    }
}

async fn drop_if_there(master: &mut Box<dyn Session>, db: &str) {
    if master.list_databases().await.unwrap().iter().any(|x| x == db) {
        master.drop_database(db).await.unwrap();
    }
}

/// Creates `old` with a table and a row and opens a second session on it.
/// Returns a session on `master` (the spec's `database_from`) and that one.
async fn setup(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, old: &str, new: &str) -> (Box<dyn Session>, Box<dyn Session>) {
    let spec = d.rename_spec().unwrap();
    assert!(spec.databases && !spec.database_moves);
    let from = spec.database_from.clone().unwrap();
    assert_eq!(from, "master");
    let mut master = d.connect(cfg, Some(&from)).await.expect("connect to master");
    drop_if_there(&mut master, old).await;
    drop_if_there(&mut master, new).await;
    master.create_database(old).await.unwrap();
    {
        let mut s = d.connect(cfg, Some(old)).await.unwrap();
        run(&mut s, "CREATE TABLE dbo.t (id int PRIMARY KEY, name nvarchar(20))").await.unwrap();
        run(&mut s, "INSERT INTO dbo.t VALUES (1, N'uno')").await.unwrap();
    }
    let mut other = d.connect(cfg, Some(old)).await.unwrap();
    assert_eq!(scalar(&mut other, "SELECT COUNT(*) FROM dbo.t").await, "1");
    (master, other)
}

/// Runs the script's statements one by one in `master`, as the app does.
async fn rename(d: &Arc<dyn Driver>, master: &mut Box<dyn Session>, old: &str, new: &str) -> dbine_driver::Result<()> {
    let script = d.rename_database_script(old, new, &[]).unwrap();
    eprintln!("{script:#?}");
    for stmt in &script.statements {
        run(master, stmt).await?;
    }
    Ok(())
}

/// The new name is there with the row, the old one is gone.
async fn check_renamed(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, master: &mut Box<dyn Session>, old: &str, new: &str) {
    let dbs = master.list_databases().await.unwrap();
    assert!(dbs.iter().any(|x| x == new), "{dbs:?}");
    assert!(!dbs.iter().any(|x| x == old), "{dbs:?}");
    let mut renamed = d.connect(cfg, Some(new)).await.unwrap();
    assert_eq!(scalar(&mut renamed, "SELECT name FROM dbo.t WHERE id = 1").await, "uno");
    assert_eq!(scalar(&mut renamed, "SELECT DB_NAME()").await, new);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlserver_rename_database() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = driver("sqlserver");
    let (old, new) = ("dbine_rendb_old", "dbine_rendb_new]x");

    let spec = d.rename_spec().unwrap();
    assert!(spec.database_note.as_deref().unwrap().contains("SINGLE_USER WITH ROLLBACK IMMEDIATE"));
    let (mut master, mut other) = setup(&d, &cfg, old, new).await;
    rename(&d, &mut master, old, new).await.unwrap();
    check_renamed(&d, &cfg, &mut master, old, new).await;
    // The other session was ended by ROLLBACK IMMEDIATE: its next statement
    // fails (the driver may reconnect, but to the old name, which is gone).
    let err = run(&mut other, "SELECT COUNT(*) FROM dbo.t").await;
    eprintln!("other session after the rename: {err:?}");
    assert!(err.is_err(), "the other session should have been ended");
    // Nobody is left in the database, and it's open to everyone again.
    // (The check session of the helper may take a moment to go.)
    let lit = new.replace('\'', "''");
    let mut left = String::new();
    for _ in 0..20 {
        left = scalar(&mut master, &format!("SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE database_id = DB_ID(N'{lit}')")).await;
        if left == "0" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(left, "0");
    assert_eq!(scalar(&mut master, &format!("SELECT user_access_desc FROM sys.databases WHERE name = N'{lit}'")).await, "MULTI_USER");
    assert_eq!(scalar(&mut master, &format!("SELECT COALESCE(CAST(DB_ID(N'{old}') AS varchar(10)), 'NULL')")).await, "NULL");

    drop(other);
    master.drop_database(new).await.unwrap();
    assert!(!master.list_databases().await.unwrap().iter().any(|x| x == new));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_rename_database() {
    let Some(cfg) = cfg("babelfish", "DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let d = driver("babelfish");
    let (old, new) = ("dbine_rendb_bbf_old", "dbine_rendb_bbf_new");
    let (mut master, other) = setup(&d, &cfg, old, new).await;
    let with_other = rename(&d, &mut master, old, new).await;
    // Babelfish doesn't end it, and refuses while it's there (the note
    // says to close it first): nothing was renamed.
    eprintln!("with another session open: {with_other:?}");
    assert!(matches!(&with_other, Err(e) if e.to_string().contains("exclusively locked")), "{with_other:?}");
    assert!(master.list_databases().await.unwrap().iter().any(|x| x == old));
    drop(other);
    let mut after = Err(dbine_driver::Error::Query("not run".into()));
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        after = rename(&d, &mut master, old, new).await;
        if after.is_ok() {
            break;
        }
    }
    after.expect("the rename once the other session is closed");
    check_renamed(&d, &cfg, &mut master, old, new).await;
    master.drop_database(new).await.unwrap();
}

//! Data Pump backups against a real server (see tests/integration.rs). The
//! user needs READ/WRITE on DATA_PUMP_DIR and the Data Pump roles:
//!
//! ```sql
//! GRANT READ, WRITE ON DIRECTORY DATA_PUMP_DIR TO dbine;
//! GRANT DATAPUMP_EXP_FULL_DATABASE, DATAPUMP_IMP_FULL_DATABASE TO dbine;
//! ```
//!
//! `DBINE_TEST_ORACLE_URL=… cargo test -p dbine-driver-oracle --test backup -- --ignored`

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}: {e}");
    }
    out
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

#[tokio::test]
#[ignore]
async fn export_import_delete() {
    let Some(cfg) = config() else { return };
    let driver = dbine_driver_oracle::drivers().remove(0);
    let spec = driver.backup().unwrap();
    assert!(spec.restore && spec.delete && spec.history && !spec.server_wide);
    let mut s = driver.connect(&cfg, None).await.unwrap();

    // A small schema of its own to export.
    let mut out = QueryOutcome::default();
    for u in ["DBINE_BK_SRC", "DBINE_BK_DST"] {
        let _ = s.execute(&format!("DROP USER {u} CASCADE"), 1, &mut out).await;
    }
    run(&mut s, "CREATE USER DBINE_BK_SRC NO AUTHENTICATION QUOTA UNLIMITED ON USERS").await;
    run(&mut s, "CREATE TABLE DBINE_BK_SRC.T (ID NUMBER PRIMARY KEY, V VARCHAR2(10))").await;
    run(&mut s, "INSERT INTO DBINE_BK_SRC.T VALUES (1, 'uno')").await;
    run(&mut s, "INSERT INTO DBINE_BK_SRC.T VALUES (2, 'dos')").await;
    run(&mut s, "COMMIT").await;

    let file = format!("dbine_bk_{}.dmp", std::process::id());
    let opts: BTreeMap<String, String> = [("file".to_string(), file.clone())].into();
    let sql = driver.backup_script(&BackupAction::Backup { database: Some("DBINE_BK_SRC".into()), options: opts }).unwrap();
    let out = run(&mut s, &sql).await;
    assert!(out.messages.iter().any(|m| m.contains("COMPLETED")), "{:?}", out.messages);

    // History: the jobs the server keeps (none left once they finish).
    let h = s.backups(Some("DBINE_BK_SRC")).await.unwrap();
    assert!(h.iter().all(|e| !e.restorable || e.id.contains('/')), "{h:?}");

    // Into another schema, remapped.
    let opts: BTreeMap<String, String> = [("source_schema".to_string(), "DBINE_BK_SRC".to_string())].into();
    let source = format!("DATA_PUMP_DIR/{file}");
    let sql = driver
        .backup_script(&BackupAction::Restore { source: source.clone(), database: Some("DBINE_BK_DST".into()), options: opts })
        .unwrap();
    run(&mut s, &sql).await;
    let out = run(&mut s, "SELECT COUNT(*) FROM DBINE_BK_DST.T").await;
    assert_eq!(text(&out.results[0].rows[0][0]), "2");

    let sql = driver.backup_script(&BackupAction::Delete { source: source.clone() }).unwrap();
    run(&mut s, &sql).await;
    // Gone: deleting it again fails.
    let mut out = QueryOutcome::default();
    assert!(s.execute(&sql, 1, &mut out).await.is_err());

    for u in ["DBINE_BK_SRC", "DBINE_BK_DST"] {
        run(&mut s, &format!("DROP USER {u} CASCADE")).await;
    }
}

//! Native backups against a real server (`DBINE_TEST_CLICKHOUSE_URL`, see
//! tests/integration.rs). The server needs a backups disk named `backups`:
//!
//! ```xml
//! <!-- /etc/clickhouse-server/config.d/backups-disk.xml -->
//! <clickhouse>
//!   <storage_configuration><disks><backups>
//!     <type>local</type><path>/var/lib/clickhouse/backups/</path>
//!   </backups></disks></storage_configuration>
//!   <backups>
//!     <allowed_disk>backups</allowed_disk>
//!     <allowed_path>/var/lib/clickhouse/backups/</allowed_path>
//!   </backups>
//! </clickhouse>
//! ```
//!
//! `cargo test -p dbine-driver-clickhouse --test backup -- --ignored`

use dbine_driver::{BackupAction, ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
    kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn backup_history_restore() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    assert!(d.backup().is_some());
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP DATABASE IF EXISTS dbine_bk; DROP DATABASE IF EXISTS dbine_bk_r; CREATE DATABASE dbine_bk").await;
    run(&mut s, "CREATE TABLE dbine_bk.t (id Int32, s String) ENGINE = MergeTree ORDER BY id; INSERT INTO dbine_bk.t VALUES (1, 'a'), (2, 'b')").await;

    let name = format!("dbine_bk-{}.zip", uuid_ish());
    let script = d
        .backup_script(&BackupAction::Backup { database: Some("dbine_bk".into()), options: opts(&[("name", &name)]) })
        .unwrap();
    run(&mut s, &script).await;
    run(&mut s, "SYSTEM FLUSH LOGS").await;

    let h = s.backups(Some("dbine_bk")).await.unwrap();
    let e = h.iter().find(|e| e.id.contains(&name)).unwrap_or_else(|| panic!("in the history: {h:?}"));
    assert!(e.restorable, "{e:?}");
    assert_eq!(e.database.as_deref(), Some("dbine_bk"));
    assert_eq!(e.status.as_deref(), Some("BACKUP_CREATED"));
    assert!(e.size.unwrap_or(0) > 0 && e.started.is_some() && e.finished.is_some(), "{e:?}");
    // Another database's tab doesn't list it.
    assert!(s.backups(Some("otra_base")).await.unwrap().iter().all(|x| !x.id.contains(&name)));

    let restore = d
        .backup_script(&BackupAction::Restore {
            source: e.id.clone(),
            database: Some("dbine_bk_r".into()),
            options: opts(&[("from_database", "dbine_bk")]),
        })
        .unwrap();
    run(&mut s, &restore).await;
    let out = run(&mut s, "SELECT count() FROM dbine_bk_r.t").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(2));

    // Into the same database, adding to its rows.
    let again = d
        .backup_script(&BackupAction::Restore {
            source: name.clone(),
            database: Some("dbine_bk".into()),
            options: opts(&[("allow_non_empty_tables", "true")]),
        })
        .unwrap();
    run(&mut s, &again).await;
    let out = run(&mut s, "SELECT count() FROM dbine_bk.t").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(4));

    run(&mut s, "DROP DATABASE dbine_bk; DROP DATABASE dbine_bk_r").await;
}

fn uuid_ish() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

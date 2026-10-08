//! "Propiedades" of a database against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, `mssql://user:pass@host:port`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test properties -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

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

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    match &out.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlserver_properties() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, Some("master")).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database("dbine_props").await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("recovery").map(String::as_str), Some("FULL"));
    assert!(p.fields.iter().any(|f| f.key == "file:dbine_props:size"), "a field per file");
    assert!(p.info.iter().any(|i| i.label == "Tamaño"));
    assert!(p.warnings.contains_key("read_only"));
    let size: i64 = p.values["file:dbine_props:size"].parse().unwrap();

    let changes: BTreeMap<String, String> = [
        ("recovery", "SIMPLE"),
        ("auto_shrink", "true"),
        ("compatibility", "150"),
        ("allow_snapshot_isolation", "true"),
        ("file:dbine_props:size", &(size + 16).to_string()),
        ("read_only", "true"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in [("recovery", "SIMPLE"), ("auto_shrink", "true"), ("compatibility", "150"), ("allow_snapshot_isolation", "true"), ("read_only", "true")] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }
    assert_eq!(p.values["file:dbine_props:size"], (size + 16).to_string());

    // Back to read-write so it can be dropped normally.
    s.alter_database("dbine_props", &[("read_only".to_string(), String::new())].into()).await.unwrap();
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values.get("read_only").map(String::as_str), Some(""));
    let mut out = QueryOutcome::default();
    s.execute("SELECT 1", 1, &mut out).await.unwrap();
    s.drop_database("dbine_props").await.unwrap();
}

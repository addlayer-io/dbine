//! "Nueva base de datos" with options, against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, `mssql://user:pass@host:port`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test create_database -- --ignored
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
async fn sqlserver_options() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    assert!(d.create_database_fields().iter().any(|f| f.key == "collation"));
    let mut s = d.connect(&cfg, Some("master")).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;

    let choices = s.create_database_choices().await.unwrap();
    let get = |k: &str| choices.iter().find(|c| c.key == k).unwrap_or_else(|| panic!("{k}"));
    assert!(get("collation").values.iter().any(|c| c == "Latin1_General_CI_AS"));
    assert!(get("collation").default.is_some() && get("owner").default.is_some());
    let data_dir = get("data_path").default.clone().expect("the default data folder");

    let options: BTreeMap<String, String> = [
        ("collation", "Latin1_General_CS_AS"),
        ("data_path", data_dir.as_str()),
        ("data_size", "16"),
        ("data_growth", "8MB"),
        ("log_size", "8"),
        ("recovery", "SIMPLE"),
        ("compatibility", "150"),
        ("owner", "sa"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();

    let row = scalar(
        &mut s,
        "SELECT CONCAT(collation_name COLLATE DATABASE_DEFAULT, '|', recovery_model_desc COLLATE DATABASE_DEFAULT, '|', compatibility_level, '|', SUSER_SNAME(owner_sid) COLLATE DATABASE_DEFAULT)
         FROM sys.databases WHERE name = 'dbine_create_opts'",
    )
    .await;
    assert_eq!(row, "Latin1_General_CS_AS|SIMPLE|150|sa");
    let files = scalar(
        &mut s,
        "SELECT STRING_AGG(CONCAT(name COLLATE DATABASE_DEFAULT, ':', size * 8 / 1024, 'MB:', physical_name COLLATE DATABASE_DEFAULT), ' ') FROM sys.master_files
         WHERE database_id = DB_ID('dbine_create_opts')",
    )
    .await;
    eprintln!("{files}");
    assert!(files.contains("dbine_create_opts:16MB:") && files.contains("dbine_create_opts.mdf"), "{files}");
    assert!(files.contains("dbine_create_opts_log:8MB:"), "{files}");

    s.drop_database("dbine_create_opts").await.unwrap();
    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

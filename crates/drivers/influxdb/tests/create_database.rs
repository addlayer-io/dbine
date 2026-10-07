//! "Nueva base de datos" with options, against real servers, each test
//! skipped without its variable (see `integration.rs` for the containers):
//!
//! ```sh
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn open(driver: &str, cfg: &ConnectionConfig) -> (std::sync::Arc<dyn dbine_driver::Driver>, Box<dyn Session>) {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let s = d.connect(cfg, None).await.unwrap();
    (d, s)
}

async fn plain(s: &mut Box<dyn Session>) {
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn influxdb1_options() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url, ..Default::default() };
    let (d, mut s) = open("influxdb1", &cfg).await;
    let _ = s.drop_database("dbine_create_opts").await;
    let o = opts(&[("duration", "30d"), ("shard_duration", "1d"), ("replication", "1"), ("rp_name", "mensual")]);
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SHOW RETENTION POLICIES ON dbine_create_opts", 10, &mut out).await.unwrap();
    let r = &out.results.last().unwrap().rows[0];
    eprintln!("{r:?}");
    assert_eq!(r[0], Value::String("mensual".into()));
    assert_eq!(r[1], Value::String("720h0m0s".into()));
    assert_eq!(r[2], Value::String("24h0m0s".into()));
    s.drop_database("dbine_create_opts").await.unwrap();
    plain(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn influxdb2_options() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org);
    cfg.options.insert("token".into(), token.clone());
    let (d, mut s) = open("influxdb", &cfg).await;
    let _ = s.drop_database("dbine_create_opts").await;
    let o = opts(&[("retention", "30d"), ("shard_duration", "1d"), ("description", "prueba")]);
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let b: Value = reqwest::Client::new()
        .get(format!("{url}/api/v2/buckets?name=dbine_create_opts"))
        .header("Authorization", format!("Token {token}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let b = &b["buckets"][0];
    assert_eq!(b["description"], "prueba");
    assert_eq!(b["retentionRules"][0]["everySeconds"], 2_592_000);
    assert_eq!(b["retentionRules"][0]["shardGroupDurationSeconds"], 86_400);
    s.drop_database("dbine_create_opts").await.unwrap();
    plain(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn influxdb3_options() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url.clone(), ..Default::default() };
    let (d, mut s) = open("influxdb3", &cfg).await;
    let _ = s.drop_database("dbine_create_opts").await;
    let o = opts(&[("retention", "30d")]);
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let dbs: Value = reqwest::get(format!("{url}/api/v3/configure/database?format=json")).await.unwrap().json().await.unwrap();
    let db = dbs.as_array().unwrap().iter().find(|d| d["iox::database"] == "dbine_create_opts").cloned();
    assert!(db.is_some(), "{dbs}");
    // The retention, from the catalog's system table.
    let rows: Value = reqwest::Client::new()
        .post(format!("{url}/api/v3/query_sql"))
        .json(&serde_json::json!({ "db": "_internal", "q": "SELECT retention_period_ns FROM system.databases WHERE database_name = 'dbine_create_opts'", "format": "json" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eprintln!("{rows}");
    assert_eq!(rows[0]["retention_period_ns"], 2_592_000_000_000_000u64);
    s.drop_database("dbine_create_opts").await.unwrap();
    plain(&mut s).await;
}

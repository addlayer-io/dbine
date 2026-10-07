//! "Nueva base de datos" with options, skipped without
//! `DBINE_TEST_SPANNER_URL`. Against the Cloud Spanner emulator's REST
//! gateway:
//!
//! ```sh
//! docker run -d --name dbine-test-spanner -p 25303:9020 gcr.io/cloud-spanner-emulator/emulator
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test create_database -- --ignored --nocapture
//! ```
//!
//! The emulator neither reports a database's options in its resource nor
//! checks the leader against a configuration: the options are read back
//! from the database's DDL (`getDdl`).

use dbine_driver::ConnectionConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn spanner_options() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `db1`" })).send().await;

    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    let _ = s.drop_database("dbine_create_plain").await;

    let choices = s.create_database_choices().await.unwrap();
    assert!(choices.iter().any(|c| c.key == "version_retention" && c.default.as_deref() == Some("1h")));

    let options: BTreeMap<String, String> =
        [("version_retention".to_string(), "3d".to_string()), ("default_leader".to_string(), "us-east1".to_string())].into();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    let ddl: Value = http.get(format!("{url}/v1/projects/test/instances/i1/databases/dbine_create_opts/ddl")).send().await.unwrap().json().await.unwrap();
    eprintln!("{ddl}");
    let ddl = ddl["statements"].to_string();
    assert!(ddl.contains("version_retention_period = '3d'") && ddl.contains("default_leader = 'us-east1'"), "{ddl}");
    s.drop_database("dbine_create_opts").await.unwrap();

    // A bad value never reaches the API.
    let bad: BTreeMap<String, String> = [("version_retention".to_string(), "3d'".to_string())].into();
    assert!(s.create_database_with("dbine_create_opts", &bad).await.is_err());

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_create_plain".to_string()));
    s.drop_database("dbine_create_plain").await.unwrap();
}

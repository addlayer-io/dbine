//! "Propiedades" of a database, skipped without `DBINE_TEST_SPANNER_URL`.
//! Against the Cloud Spanner emulator's REST gateway:
//!
//! ```sh
//! docker run -d --name dbine-test-spanner -p 25303:9020 gcr.io/cloud-spanner-emulator/emulator
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test properties -- --ignored --nocapture
//! ```
//!
//! The emulator doesn't know `optimizer_version` /
//! `optimizer_statistics_package` and doesn't implement drop protection
//! (`databases.patch`): those are checked only to fail with its error, the
//! rest is applied and read back (from `getDdl`).

use dbine_driver::ConnectionConfig;
use serde_json::json;
use std::collections::BTreeMap;

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn spanner_properties() {
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
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database_with("dbine_props", &c(&[("version_retention", "3d")])).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("version_retention_period").map(String::as_str), Some("3d"));
    assert_eq!(p.values.get("drop_protection").map(String::as_str), Some(""));
    assert!(p.info.iter().any(|i| i.label == "Estado" && i.value == "READY"));
    assert!(p.warnings.contains_key("default_leader"));

    let changes = c(&[
        ("version_retention_period", "7d"),
        ("default_leader", "us-east1"),
        ("default_time_zone", "Europe/Madrid"),
        ("default_sequence_kind", "bit_reversed_positive"),
    ]);
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }

    // Emptied: back to the default (NULL).
    s.alter_database("dbine_props", &c(&[("default_time_zone", "")])).await.unwrap();
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["default_time_zone"], "");

    // The options go first: when drop protection fails (the emulator
    // lacks it), the error says one change was applied.
    let two = c(&[("version_retention_period", "1h"), ("drop_protection", "true")]);
    match s.alter_database("dbine_props", &two).await {
        Ok(()) => {
            assert_eq!(s.database_properties("dbine_props").await.unwrap().values["drop_protection"], "true");
            s.alter_database("dbine_props", &c(&[("drop_protection", "")])).await.unwrap();
        }
        Err(e) => assert!(e.to_string().contains("se aplicaron 1 de 2"), "{e}"),
    }
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["version_retention_period"], "1h");

    // A bad value never reaches the API.
    assert!(s.alter_database("dbine_props", &c(&[("default_leader", "x'")])).await.is_err());
    s.drop_database("dbine_props").await.unwrap();
}

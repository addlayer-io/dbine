//! "Propiedades" of a dataset, skipped without `DBINE_TEST_BIGQUERY_URL`.
//! Against bigquery-emulator:
//!
//! ```sh
//! docker run -d --name dbine-test-bigquery -p 25302:9050 ghcr.io/goccy/bigquery-emulator --project=test --dataset=ds1
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test properties -- --ignored --nocapture
//! ```
//!
//! The emulator accepts `datasets.patch` but doesn't keep the change (nor
//! does it keep `datasets.update` or run `ALTER SCHEMA`): against it the
//! read-back after the change is skipped. Every other step runs.

use dbine_driver::{ConnectionConfig, Session};
use std::collections::BTreeMap;

async fn open() -> Option<(Box<dyn Session>, String)> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url.clone());
    Some((dbine_driver_bigquery::drivers().pop().unwrap().connect(&c, None).await.unwrap(), url))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bigquery_properties() {
    let Some((mut s, url)) = open().await else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let emulator = url.contains("localhost") || url.contains("127.0.0.1");
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    assert!(d.capabilities().database_properties);
    let _ = s.drop_database("dbine_props").await;
    let o: BTreeMap<String, String> =
        [("description", "antes"), ("labels", "equipo=ventas"), ("table_expiration_days", "30")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    s.create_database_with("dbine_props", &o).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("description").map(String::as_str), Some("antes"));
    assert_eq!(p.values.get("label:equipo").map(String::as_str), Some("ventas"));
    assert!(p.fields.iter().any(|f| f.key == "label:equipo"), "a field per label");
    assert!(p.info.iter().any(|i| i.label == "Ubicación"));
    assert!(p.warnings.contains_key("storage_billing_model"));
    if !emulator {
        assert_eq!(p.values.get("default_table_expiration_days").map(String::as_str), Some("30"));
    }

    let changes: BTreeMap<String, String> = [
        ("description", "ventas \"históricas\""),
        ("default_table_expiration_days", "7"),
        ("max_time_travel_hours", "72"),
        ("label:equipo", ""),
        ("labels_add", "entorno=prod"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();

    // A bad value never reaches the API.
    assert!(s.alter_database("dbine_props", &[("max_time_travel_hours".to_string(), "50".to_string())].into()).await.is_err());

    if emulator {
        eprintln!("emulator: datasets.patch accepted, read-back skipped (it doesn't keep changes)");
    } else {
        let p = s.database_properties("dbine_props").await.unwrap();
        assert_eq!(p.values["description"], "ventas \"históricas\"");
        assert_eq!(p.values["default_table_expiration_days"], "7");
        assert_eq!(p.values["max_time_travel_hours"], "72");
        assert!(!p.values.contains_key("label:equipo"));
        assert_eq!(p.values.get("label:entorno").map(String::as_str), Some("prod"));
    }
    s.drop_database("dbine_props").await.unwrap();
}

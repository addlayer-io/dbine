//! "Nueva base de datos" (a dataset) with options, skipped without
//! `DBINE_TEST_BIGQUERY_URL`. Against bigquery-emulator:
//!
//! ```sh
//! docker run -d --name dbine-test-bigquery -p 25302:9050 ghcr.io/goccy/bigquery-emulator --project=test --dataset=ds1
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Session};
use serde_json::Value;
use std::collections::BTreeMap;

async fn open() -> Option<(Box<dyn Session>, String)> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url.clone());
    Some((dbine_driver_bigquery::drivers().pop().unwrap().connect(&c, None).await.unwrap(), url))
}

async fn dataset(url: &str, name: &str) -> Value {
    reqwest::get(format!("{url}/bigquery/v2/projects/test/datasets/{name}")).await.unwrap().json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bigquery_options() {
    let Some((mut s, url)) = open().await else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    let _ = s.drop_database("dbine_create_opts").await;

    let choices = s.create_database_choices().await.unwrap();
    assert!(choices.iter().any(|c| c.key == "location" && c.values.iter().any(|v| v == "EU")));

    let options: BTreeMap<String, String> = [
        ("location", "EU"),
        ("table_expiration_days", "30"),
        ("description", "Ventas \"históricas\""),
        ("labels", "equipo=ventas\nentorno=prod"),
        ("collation", "und:ci"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    let ds = dataset(&url, "dbine_create_opts").await;
    eprintln!("{ds}");
    assert_eq!(ds["location"], "EU");
    assert_eq!(ds["labels"]["equipo"], "ventas");
    assert_eq!(ds["labels"]["entorno"], "prod");
    assert_eq!(ds["description"], "Ventas \"históricas\"");
    assert_eq!(ds["defaultTableExpirationMs"], "2592000000");
    assert_eq!(ds["defaultCollation"], "und:ci");
    s.drop_database("dbine_create_opts").await.unwrap();

    // A bad value never reaches the API.
    let bad: BTreeMap<String, String> = [("labels".to_string(), "Equipo=x".to_string())].into();
    assert!(s.create_database_with("dbine_create_opts", &bad).await.is_err());

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_create_plain".to_string()));
    s.drop_database("dbine_create_plain").await.unwrap();
}

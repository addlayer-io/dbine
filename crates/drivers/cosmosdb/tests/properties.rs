//! "Propiedades" of a database against the emulator or an account (see
//! `integration.rs`), skipped without `DBINE_TEST_COSMOSDB_URL`:
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb --test properties -- --ignored --nocapture
//! ```
//! The key defaults to the emulator's well-known one
//! (`DBINE_TEST_COSMOSDB_KEY` overrides it).

use dbine_driver::{ConnectionConfig, DatabaseProperties};
use std::collections::BTreeMap;

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn fact<'a>(p: &'a DatabaseProperties, label: &str) -> Option<&'a str> {
    p.info.iter().find(|i| i.label == label).map(|i| i.value.as_str())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cosmosdb_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else {
        eprintln!("DBINE_TEST_COSMOSDB_URL not set; skipping");
        return;
    };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url, trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key);
    let d = dbine_driver_cosmosdb::drivers().into_iter().next().unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&c, None).await.unwrap();

    // Shared throughput: the offer can change.
    let _ = s.drop_database("dbine_props").await;
    s.create_database_with("dbine_props", &changes(&[("throughput_mode", "manual"), ("throughput", "400")])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    eprintln!("{:#?}", p.info);
    assert_eq!(fact(&p, "Contenedores"), Some("0"));
    assert!(fact(&p, "Identificador interno (_rid)").is_some_and(|v| !v.is_empty()));
    assert_eq!(p.values.get("throughput_mode").map(String::as_str), Some("manual"));
    assert_eq!(p.values.get("throughput").map(String::as_str), Some("400"));
    assert!(p.warnings.contains_key("throughput_mode"));

    let ch = changes(&[("throughput", "600")]);
    eprintln!("{}", d.alter_database_script("dbine_props", &ch).unwrap());
    s.alter_database("dbine_props", &ch).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("throughput").map(String::as_str), Some("600"));

    // To autoscale with a maximum, and back to manual.
    let ch = changes(&[("throughput_mode", "autoscale"), ("autoscale_max", "4000")]);
    eprintln!("{}", d.alter_database_script("dbine_props", &ch).unwrap());
    s.alter_database("dbine_props", &ch).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("throughput_mode").map(String::as_str), Some("autoscale"));
    assert_eq!(p.values.get("autoscale_max").map(String::as_str), Some("4000"));
    // Manual RU/s on an autoscale offer need the switch.
    assert!(s.alter_database("dbine_props", &changes(&[("throughput", "500")])).await.is_err());
    s.alter_database("dbine_props", &changes(&[("throughput_mode", "manual"), ("throughput", "500")])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("throughput_mode").map(String::as_str), Some("manual"));
    assert_eq!(p.values.get("throughput").map(String::as_str), Some("500"));
    assert!(s.alter_database("dbine_props", &changes(&[("throughput", "450")])).await.is_err());
    s.drop_database("dbine_props").await.unwrap();

    // Without database throughput: facts only.
    let _ = s.drop_database("dbine_props_plain").await;
    s.create_database("dbine_props_plain").await.unwrap();
    let p = s.database_properties("dbine_props_plain").await.unwrap();
    assert!(p.fields.is_empty());
    assert!(fact(&p, "Throughput de la base").is_some_and(|v| v.starts_with("No tiene")), "{:?}", p.info);
    assert!(s.alter_database("dbine_props_plain", &changes(&[("throughput", "400")])).await.is_err());
    s.drop_database("dbine_props_plain").await.unwrap();
}

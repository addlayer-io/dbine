//! "Propiedades" of a database against a real server
//! (`DBINE_TEST_MONGODB_URL`, a connection string), skipped without it:
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL='mongodb://root:secret@localhost:25201/?authSource=admin' \
//!   cargo test -p dbine-driver-mongodb --test properties -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::collections::BTreeMap;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let mut c = ConnectionConfig { driver: driver.into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mongodb_properties() {
    let Some(c) = cfg("DBINE_TEST_MONGODB_URL", "mongodb") else {
        eprintln!("DBINE_TEST_MONGODB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == "mongodb").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database("dbine_props").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("use dbine_props\ndb.items.insertMany([{ a: 1 }, { a: 2 }, { a: 3 }])", 10, &mut out).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert!(p.info.iter().any(|i| i.label == "Documentos (objects)" && i.value == "3"), "{:?}", p.info);
    assert!(p.info.iter().any(|i| i.label == "Espacio total (totalSize)"));
    assert_eq!(p.values.get("profile").map(String::as_str), Some("0"));
    let slowms = p.values["slowms"].clone();
    let rate = p.values["sample_rate"].clone();
    assert!(p.warnings.contains_key("profile"));

    let ch = changes(&[("profile", "1"), ("slowms", "150"), ("sample_rate", "0.5")]);
    eprintln!("{}", d.alter_database_script("dbine_props", &ch).unwrap());
    s.alter_database("dbine_props", &ch).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in [("profile", "1"), ("slowms", "150"), ("sample_rate", "0.5")] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }

    // Only the server-wide value: the level stays.
    s.alter_database("dbine_props", &changes(&[("slowms", "175")])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("profile").map(String::as_str), Some("1"));
    assert_eq!(p.values.get("slowms").map(String::as_str), Some("175"));

    assert!(s.alter_database("dbine_props", &changes(&[("profile", "9")])).await.is_err());

    // Put the server back as it was.
    s.alter_database("dbine_props", &changes(&[("profile", "0"), ("slowms", &slowms), ("sample_rate", &rate)])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("profile").map(String::as_str), Some("0"));
    assert_eq!(p.values.get("slowms"), Some(&slowms));
    s.drop_database("dbine_props").await.unwrap();
}

/// FerretDB (`DBINE_TEST_FERRETDB_URL`): facts only.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn ferretdb_properties() {
    let Some(c) = cfg("DBINE_TEST_FERRETDB_URL", "ferretdb") else {
        eprintln!("DBINE_TEST_FERRETDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == "ferretdb").unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database("dbine_props").await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert!(p.fields.is_empty());
    assert!(p.info.iter().any(|i| i.label == "Colecciones"), "{:?}", p.info);
    assert!(s.alter_database("dbine_props", &changes(&[("profile", "1")])).await.is_err());
    s.drop_database("dbine_props").await.unwrap();
}

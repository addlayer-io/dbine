//! "Propiedades" of a database (a Unity Catalog catalog) against a real
//! Databricks SQL warehouse (there's no container for it), skipped without
//! one. The user needs CREATE CATALOG on the metastore:
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test properties -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}")).ok().filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig { driver: "databricks".into(), host: env("HOST")?, database: env("CATALOG")?, ..Default::default() };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    Some(c)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks_properties() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DATABRICKS_* not set; skipping");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database("dbine_props").await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert!(!p.values["owner"].is_empty());
    assert!(!p.info.is_empty());

    let mut changes: BTreeMap<String, String> = [("comment".to_string(), "ventas 'históricas'".to_string())].into();
    if p.fields.iter().any(|f| f.key == "predictive_optimization") {
        changes.insert("predictive_optimization".into(), "DISABLE".into());
    }
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }
    s.alter_database("dbine_props", &[("comment".to_string(), String::new())].into()).await.unwrap();
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["comment"], "");
    s.drop_database("dbine_props").await.unwrap();
}

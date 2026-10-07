//! "Nueva base de datos" (a Unity Catalog catalog) with options, against a
//! real Databricks SQL warehouse (there's no container for it), skipped
//! without one. The user needs CREATE CATALOG on the metastore;
//! `DBINE_TEST_DATABRICKS_LOCATION`, optional, is a path inside an external
//! location to use as the managed location:
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
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

/// `DESCRIBE CATALOG EXTENDED`'s `info_value` for `info_name`.
async fn describe(s: &mut Box<dyn Session>, catalog: &str, name: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    s.execute(&format!("DESCRIBE CATALOG EXTENDED `{catalog}`"), 100, &mut out).await.unwrap();
    out.results.last()?.rows.iter().find(|r| r[0] == Value::String(name.into())).and_then(|r| r[1].as_str().map(str::to_string))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks_options() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DATABRICKS_* not set; skipping");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    eprintln!("{:?}", s.create_database_choices().await.unwrap());

    let mut options: BTreeMap<String, String> = BTreeMap::from([("comment".to_string(), "ventas 'históricas'".to_string())]);
    if let Some(l) = env("LOCATION") {
        options.insert("managed_location".into(), l);
    }
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    assert_eq!(describe(&mut s, "dbine_create_opts", "Comment").await.as_deref(), Some("ventas 'históricas'"));
    if let Some(l) = options.get("managed_location") {
        let root = describe(&mut s, "dbine_create_opts", "Storage Root").await.unwrap_or_default();
        assert!(root.starts_with(l.trim_end_matches('/')), "{root}");
    }
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

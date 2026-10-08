//! "Propiedades" of a database against a real Snowflake account (there's no
//! emulator), skipped without one. The role needs CREATE DATABASE:
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=miorg-micuenta DBINE_TEST_SNOWFLAKE_USER=JDOE DBINE_TEST_SNOWFLAKE_TOKEN=<PAT> \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test properties -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok().filter(|v| !v.is_empty());
    let mut options = BTreeMap::from([("account".to_string(), var("ACCOUNT")?), ("token".to_string(), var("TOKEN")?)]);
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    Some(ConnectionConfig { driver: "snowflake".into(), username: var("USER"), options: options.into_iter().collect(), ..Default::default() })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn snowflake_properties() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_SNOWFLAKE_* not set; skipping");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("DBINE_PROPS").await;
    s.create_database("DBINE_PROPS").await.unwrap();

    let p = s.database_properties("DBINE_PROPS").await.unwrap();
    assert!(p.values.contains_key("data_retention_time_in_days"));
    assert!(!p.values["owner"].is_empty());
    assert!(p.info.iter().any(|i| i.label == "Creada"));

    let changes: BTreeMap<String, String> = [
        ("comment", "ventas 'históricas'"),
        ("data_retention_time_in_days", "0"),
        ("max_data_extension_time_in_days", "3"),
        ("default_ddl_collation", "en-ci"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.alter_database_script("DBINE_PROPS", &changes).unwrap());
    s.alter_database("DBINE_PROPS", &changes).await.unwrap();
    let p = s.database_properties("DBINE_PROPS").await.unwrap();
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }

    // Emptying a parameter goes back to the account's value.
    s.alter_database("DBINE_PROPS", &[("default_ddl_collation".to_string(), String::new())].into()).await.unwrap();
    assert_ne!(s.database_properties("DBINE_PROPS").await.unwrap().values.get("default_ddl_collation").map(String::as_str), Some("en-ci"));
    s.drop_database("DBINE_PROPS").await.unwrap();
}

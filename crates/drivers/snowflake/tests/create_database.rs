//! "Nueva base de datos" with options, against a real Snowflake account
//! (there's no emulator), skipped without one. The role needs CREATE
//! DATABASE on the account:
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=miorg-micuenta DBINE_TEST_SNOWFLAKE_USER=JDOE DBINE_TEST_SNOWFLAKE_TOKEN=<PAT> \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok().filter(|v| !v.is_empty());
    let mut options = BTreeMap::from([("account".to_string(), var("ACCOUNT")?), ("token".to_string(), var("TOKEN")?)]);
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    Some(ConnectionConfig { driver: "snowflake".into(), username: var("USER"), options: options.into_iter().collect(), ..Default::default() })
}

async fn column(s: &mut Box<dyn Session>, sql: &str, name: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).unwrap_or_else(|| panic!("{name}"));
    match &r.rows[0][i] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn snowflake_options() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_SNOWFLAKE_* not set; skipping");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("DBINE_CREATE_OPTS").await;

    let choices = s.create_database_choices().await.unwrap();
    assert!(choices.iter().any(|c| c.key == "retention_days" && c.default.is_some()), "{choices:?}");

    let options: BTreeMap<String, String> = [
        ("transient", "true"),
        ("retention_days", "0"),
        ("max_extension_days", "7"),
        ("collation", "en-ci"),
        ("comment", "ventas 'históricas'"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.create_database_script("DBINE_CREATE_OPTS", &options).unwrap());
    s.create_database_with("DBINE_CREATE_OPTS", &options).await.unwrap();
    let show = "SHOW DATABASES LIKE 'DBINE_CREATE_OPTS'";
    assert_eq!(column(&mut s, show, "options").await, "TRANSIENT");
    assert_eq!(column(&mut s, show, "retention_time").await, "0");
    assert_eq!(column(&mut s, show, "comment").await, "ventas 'históricas'");
    let params = "SHOW PARAMETERS LIKE 'DEFAULT_DDL_COLLATION' IN DATABASE \"DBINE_CREATE_OPTS\"";
    assert_eq!(column(&mut s, params, "value").await, "en-ci");
    s.drop_database("DBINE_CREATE_OPTS").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("DBINE_CREATE_PLAIN", &BTreeMap::new()).await.unwrap();
    s.drop_database("DBINE_CREATE_PLAIN").await.unwrap();
}

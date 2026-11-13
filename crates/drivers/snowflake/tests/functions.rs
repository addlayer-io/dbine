//! The function catalog (#97) against a real Snowflake account (there's no
//! emulator):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=miorg-micuenta DBINE_TEST_SNOWFLAKE_USER=JDOE DBINE_TEST_SNOWFLAKE_TOKEN=<PAT> \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind};

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([
        ("account".to_string(), var("ACCOUNT")?),
        ("token".to_string(), var("TOKEN")?),
    ]);
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        database: var("DATABASE").unwrap_or_default(),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn snowflake_lists_its_functions() {
    let Some(c) = config() else { return };
    let d = dbine_driver_snowflake::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 500, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    let f = find("APPROX_COUNT_DISTINCT").expect("APPROX_COUNT_DISTINCT");
    assert_eq!(f.kind, FunctionKind::Aggregate);
    assert!(f.args.starts_with('('), "{f:?}");
    assert!(!f.description.is_empty());
    assert_eq!(find("FLATTEN").unwrap().kind, FunctionKind::Table);
    assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
    for absent in ["APPROX_DISTINCT", "UNIQ", "TOSTARTOFDAY"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

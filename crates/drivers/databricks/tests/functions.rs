//! The function catalog (#97) against a real workspace (there's no
//! emulator). Skipped without the variables:
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig {
        driver: "databricks".into(),
        host: env("HOST")?,
        database: env("CATALOG")?,
        ..Default::default()
    };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    c.options.insert(
        "schema".into(),
        env("SCHEMA").unwrap_or_else(|| "default".into()),
    );
    Some(c)
}

#[tokio::test]
#[ignore]
async fn databricks_lists_its_functions() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE OR REPLACE TEMPORARY FUNCTION dbine_fn_it(x INT) RETURNS INT RETURN x * 2",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let list = s.function_catalog().await.unwrap();
    let _ = s
        .execute(
            "DROP TEMPORARY FUNCTION IF EXISTS dbine_fn_it",
            10,
            &mut out,
        )
        .await;
    assert!(list.len() > 300, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    assert_eq!(
        find("approx_count_distinct").unwrap().kind,
        FunctionKind::Aggregate
    );
    assert_eq!(find("row_number").unwrap().kind, FunctionKind::Window);
    assert_eq!(find("explode").unwrap().kind, FunctionKind::Table);
    assert!(find("dbine_fn_it").is_some());
    for absent in ["approx_distinct", "uniq", "toStartOfDay"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

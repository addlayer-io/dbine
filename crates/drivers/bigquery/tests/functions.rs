//! The function catalog (#97) against bigquery-emulator:
//!
//! ```sh
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test functions -- --ignored
//! ```
//!
//! The emulator has no `INFORMATION_SCHEMA.ROUTINES`: the built-ins come
//! from the static catalog and the user's functions are left out, as on a
//! project whose region view can't be read.

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn bigquery_lists_its_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        return;
    };
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let mut s = dbine_driver_bigquery::drivers()
        .pop()
        .unwrap()
        .connect(&c, Some("ds1"))
        .await
        .unwrap();
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 300, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    assert_eq!(
        find("APPROX_COUNT_DISTINCT").unwrap().kind,
        FunctionKind::Aggregate
    );
    assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
    assert_eq!(find("HOST").unwrap().schema, "NET");
    // The conditional expressions written as calls, and no examples.
    for name in [
        "COALESCE",
        "IF",
        "IFNULL",
        "NULLIF",
        "FLOAT64",
        "LAX_FLOAT64",
    ] {
        assert!(find(name).is_some(), "{name}");
    }
    assert_eq!(list.iter().filter(|f| f.name == "RANGE_BUCKET").count(), 1);
    assert_eq!(find("ST_UNION_AGG").unwrap().kind, FunctionKind::Aggregate);
    for absent in [
        "approx_distinct",
        "PENDING_COMMIT_TIMESTAMP",
        "toStartOfDay",
    ] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

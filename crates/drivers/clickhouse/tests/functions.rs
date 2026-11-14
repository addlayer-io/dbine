//! The function catalog (#97) from real servers:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome};

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

fn find<'a>(list: &'a [FunctionInfo], name: &str) -> Option<&'a FunctionInfo> {
    list.iter().find(|f| f.name == name)
}

async fn session(env: &str, id: &str) -> Option<Box<dyn dbine_driver::Session>> {
    let c = cfg(env, id)?;
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    Some(d.connect(&c, None).await.unwrap())
}

#[tokio::test]
#[ignore]
async fn clickhouse_lists_its_functions() {
    let Some(mut s) = session("DBINE_TEST_CLICKHOUSE_URL", "clickhouse").await else {
        return;
    };
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP FUNCTION IF EXISTS dbine_fn_it; CREATE FUNCTION dbine_fn_it AS (x, k) -> k * x",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let list = s.function_catalog().await;
    s.execute("DROP FUNCTION IF EXISTS dbine_fn_it", 10, &mut out)
        .await
        .unwrap();
    let list = list.unwrap();
    assert!(list.len() > 1000, "{}", list.len());

    let f = find(&list, "dateDiff").expect("dateDiff");
    assert_eq!(f.kind, FunctionKind::Scalar);
    assert!(f.args.starts_with("(unit"), "{f:?}");
    assert_eq!(f.returns, "Int64");
    assert!(!f.description.is_empty());
    assert_eq!(find(&list, "uniq").unwrap().kind, FunctionKind::Aggregate);
    assert_eq!(
        find(&list, "row_number").unwrap().kind,
        FunctionKind::Window
    );
    assert_eq!(find(&list, "numbers").unwrap().kind, FunctionKind::Table);
    assert_eq!(find(&list, "lcase").unwrap().args, "(s)");
    let udf = find(&list, "dbine_fn_it").expect("the user's function");
    assert_eq!(udf.args, "(x, k)");
    // The syntax's trailing comments aren't arguments (`tokens(value) --
    // 'splitByNonAlpha' tokenizer`).
    for f in &list {
        assert!(!f.args.contains("--"), "{f:?}");
    }
    // Not ClickHouse's, and its internal ones are left out.
    for absent in [
        "approx_distinct",
        "_CAST",
        "__getScalar",
        "validateNestedArraySizes",
    ] {
        assert!(find(&list, absent).is_none(), "{absent}");
    }
}

/// 22.8's `system.functions` has no documentation columns.
#[tokio::test]
#[ignore]
async fn clickhouse_22_lists_its_functions() {
    let Some(mut s) = session("DBINE_TEST_CLICKHOUSE_22_URL", "clickhouse").await else {
        return;
    };
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 500, "{}", list.len());
    assert_eq!(find(&list, "uniq").unwrap().kind, FunctionKind::Aggregate);
    assert_eq!(
        find(&list, "toStartOfDay").unwrap().kind,
        FunctionKind::Scalar
    );
    assert_eq!(find(&list, "numbers").unwrap().kind, FunctionKind::Table);
    // Added after 22.8.
    for absent in ["groupConcat", "mortonEncode", "approx_distinct"] {
        assert!(find(&list, absent).is_none(), "{absent}");
    }
}

#[tokio::test]
#[ignore]
async fn timeplus_lists_its_functions() {
    let Some(mut s) = session("DBINE_TEST_TIMEPLUS_URL", "timeplus").await else {
        return;
    };
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 300, "{}", list.len());
    assert_eq!(find(&list, "count").unwrap().kind, FunctionKind::Aggregate);
    assert!(find(&list, "to_string").is_some());
    assert!(find(&list, "approx_distinct").is_none());
}

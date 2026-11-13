//! The function catalog (#97) against the Cloud Spanner emulator:
//!
//! ```sh
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test functions -- --ignored
//! ```
//!
//! The test creates instance `i1` and database `db1` in project `test` if
//! missing, like the other tests.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn spanner_lists_its_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        return;
    };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: "db1".into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url.as_str()),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let mut s = dbine_driver_spanner::drivers()
        .pop()
        .unwrap()
        .connect(&cfg, None)
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    // A change stream brings its read function.
    s.execute("CREATE CHANGE STREAM dbine_fn_it FOR ALL", 10, &mut out)
        .await
        .unwrap();
    let list = s.function_catalog().await;
    let mut done = QueryOutcome::default();
    s.execute("DROP CHANGE STREAM dbine_fn_it", 10, &mut done)
        .await
        .unwrap();
    let list = list.unwrap();
    assert!(list.len() > 200, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    assert_eq!(find("COUNT").unwrap().kind, FunctionKind::Aggregate);
    assert!(find("PENDING_COMMIT_TIMESTAMP").is_some());
    assert_eq!(find("HOST").unwrap().schema, "NET");
    if out.error.is_none() {
        let read = find("READ_dbine_fn_it").expect("the change stream's read function");
        assert_eq!(read.kind, FunctionKind::Table);
        assert!(read.args.contains("start_timestamp"), "{read:?}");
    }
    // The conditional expressions written as calls, and no examples.
    for name in [
        "COALESCE",
        "IFNULL",
        "NULLIF",
        "ADDDATE",
        "LAX_FLOAT64",
        "PATH_LENGTH",
    ] {
        assert!(find(name).is_some(), "{name}");
    }
    assert_eq!(list.iter().filter(|f| f.name == "SEARCH").count(), 1);
    // Other engines' functions.
    for absent in ["approx_distinct", "APPENDS", "toStartOfDay"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

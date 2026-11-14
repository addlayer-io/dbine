//! The function catalog (#97) from a real Dremio OSS (the test creates the
//! first user if needed):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};
use serde_json::json;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

/// A fresh Dremio has no users: create the first one (ignored if it exists).
async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http
            .get(format!("{base}/apiv2/server_status"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
}

#[tokio::test]
#[ignore]
async fn dremio_lists_its_functions() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE OR REPLACE FUNCTION \"@dbine\".dbine_fn_it (x INT, k INT) RETURNS INT RETURN x * k",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let list = s.function_catalog().await;
    s.execute("DROP FUNCTION \"@dbine\".dbine_fn_it", 10, &mut out)
        .await
        .unwrap();
    let list = list.unwrap();
    assert!(list.len() > 300, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    let f = find("APPROX_COUNT_DISTINCT").expect("APPROX_COUNT_DISTINCT");
    assert_eq!(f.kind, FunctionKind::Aggregate);
    assert!(!f.description.is_empty());
    assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
    assert_eq!(find("ABS").unwrap().args, "(numeric_expression NUMERIC)");
    let udf = find("dbine_fn_it").expect("the user's function");
    assert_eq!(udf.schema, "\"@dbine\"");
    assert_eq!(udf.args, "(x, k)");
    for absent in ["approx_distinct", "uniq", "toStartOfDay"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

//! Named tuples show their field names, in declared order, recursively in
//! arrays and maps:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//!   cargo test -p dbine-driver-clickhouse --test structured -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::{json, Value};

fn cfg(var: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

const SQL: &str = "SELECT
  [CAST((42, true, [('demo-product', 2)], NULL)
    AS Tuple(customer_id UInt32, active Bool,
             items Array(Tuple(product_name String, quantity Int32)), note Nullable(String)))] AS a,
  CAST(('', []) AS Tuple(`a b` String, e Array(Int32))) AS b,
  map('k', CAST(tuple(1) AS Tuple(id Int32))) AS m,
  tuple(1, 'x') AS anon";

async fn check(var: &str) {
    let Some(c) = cfg(var) else {
        eprintln!("{var} not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(SQL, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let row = &out.results[0].rows[0];
    let text = |v: &Value| v.as_str().expect("JSON text").to_string();
    // Declared order, as the server wrote it.
    assert_eq!(
        text(&row[0]),
        r#"[{"customer_id":42,"active":true,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]"#
    );
    let parse = |v: &Value| serde_json::from_str::<Value>(&text(v)).unwrap();
    assert_eq!(parse(&row[1]), json!({"a b": "", "e": []}));
    assert_eq!(parse(&row[2]), json!({"k": {"id": 1}}));
    assert_eq!(parse(&row[3]), json!([1, "x"]));
}

#[tokio::test]
#[ignore]
async fn clickhouse_named_tuples() {
    check("DBINE_TEST_CLICKHOUSE_URL").await;
}

#[tokio::test]
#[ignore]
async fn clickhouse_22_8_named_tuples() {
    check("DBINE_TEST_CLICKHOUSE_22_URL").await;
}

//! Structs show their field names, against a real Flight SQL server
//! (GizmoSQL, DuckDB behind):
//!
//! ```sh
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!   cargo test -p dbine-driver-flightsql --test structured -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::{json, Value};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into()),
        ),
        password: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into()),
        ),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn flightsql_structs_have_field_names() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT [{'customer_id': 42::BIGINT, 'active': true,
                  'items': [{'product_name': 'demo-product', 'quantity': 2}],
                  'note': NULL::VARCHAR}] AS a,
                {'a b': '', 'e': []::INTEGER[]} AS b,
                MAP {'k': {'id': 1}} AS m",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let row = &out.results[0].rows[0];
    let parse = |v: &Value| serde_json::from_str::<Value>(v.as_str().expect("JSON text")).unwrap();
    assert_eq!(
        parse(&row[0]),
        json!([{"customer_id": 42, "active": true,
                "items": [{"product_name": "demo-product", "quantity": 2}], "note": null}])
    );
    assert_eq!(parse(&row[1]), json!({"a b": "", "e": []}));
    assert_eq!(parse(&row[2]), json!({"k": {"id": 1}}));
}

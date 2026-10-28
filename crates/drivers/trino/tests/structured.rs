//! Row values show their field names, against a real Trino (and Presto):
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//!   cargo test -p dbine-driver-trino --test structured -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::{json, Value};

fn cfg(var: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

async fn first_row(var: &str, driver: &str, sql: &str) -> Option<Vec<Value>> {
    let cfg = cfg(var, driver)?;
    let d = dbine_driver_trino::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    Some(out.results[0].rows[0].clone())
}

const SQL: &str = r#"SELECT
  ARRAY[CAST(ROW(42, true, ARRAY[ROW('demo-product', 2)], NULL)
    AS ROW(customer_id BIGINT, active BOOLEAN,
           items ARRAY(ROW(product_name VARCHAR, quantity INTEGER)), note VARCHAR))] AS a,
  CAST(ROW('', ARRAY[], X'CAFE', DECIMAL '1.50', 9007199254740993)
    AS ROW("a b" VARCHAR, e ARRAY(INTEGER), bin VARBINARY, d DECIMAL(5,2), big BIGINT)) AS b,
  MAP(ARRAY['k'], ARRAY[CAST(ROW(1) AS ROW(id INTEGER))]) AS m,
  ROW(1, 'x') AS anon"#;

fn parse(v: &Value) -> Value {
    serde_json::from_str(v.as_str().expect("JSON text")).unwrap()
}

fn check(row: &[Value]) {
    assert_eq!(
        parse(&row[0]),
        json!([{"customer_id": 42, "active": true,
                "items": [{"product_name": "demo-product", "quantity": 2}], "note": null}])
    );
    assert_eq!(
        parse(&row[1]),
        json!({"a b": "", "e": [], "bin": "0xCAFE", "d": "1.50", "big": "9007199254740993"})
    );
    assert_eq!(parse(&row[2]), json!({"k": {"id": 1}}));
    assert_eq!(parse(&row[3]), json!([1, "x"]));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trino_rows_have_field_names() {
    let Some(row) = first_row("DBINE_TEST_TRINO_URL", "trino", SQL).await else {
        eprintln!("DBINE_TEST_TRINO_URL not set; skipping");
        return;
    };
    // The issue's example, verbatim as JSON text.
    assert_eq!(
        row[0],
        json!(
            r#"[{"customer_id":42,"active":true,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]"#
        )
    );
    check(&row);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn presto_rows_have_field_names() {
    let Some(row) = first_row("DBINE_TEST_PRESTO_URL", "presto", SQL).await else {
        eprintln!("DBINE_TEST_PRESTO_URL not set; skipping");
        return;
    };
    check(&row);
}

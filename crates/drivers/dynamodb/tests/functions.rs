//! The PartiQL function catalog (#97) through a session on DynamoDB Local:
//! `DBINE_TEST_DYNAMODB_URL=http://localhost:25300 cargo test -p dbine-driver-dynamodb --test functions -- --ignored`.
//! The catalog is static; this checks the session offers it and that
//! PartiQL takes each function.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

#[tokio::test]
#[ignore]
async fn dynamodb_partiql_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_DYNAMODB_URL") else {
        return;
    };
    let mut c = ConnectionConfig {
        driver: "dynamodb".into(),
        ..Default::default()
    };
    for (k, v) in [
        ("region", "us-east-1"),
        ("auth_mode", "keys"),
        ("access_key_id", "dummy"),
        ("secret_access_key", "dummy"),
        ("endpoint_url", url.as_str()),
    ] {
        c.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let cat = s.function_catalog().await.unwrap();
    let has = |n: &str| {
        cat.iter()
            .any(|f| f.name == n && f.kind == FunctionKind::Scalar)
    };
    for n in [
        "BEGINS_WITH",
        "CONTAINS",
        "SIZE",
        "ATTRIBUTE_TYPE",
        "EXISTS",
    ] {
        assert!(has(n), "{n}");
    }
    // DynamoDB's PartiQL has no aggregates or SQL string functions.
    assert!(!has("COUNT") && !has("UPPER"));

    // The server takes them: a table of its own, removed at the end.
    let t = "dbine_fn_probe";
    let mut out = QueryOutcome::default();
    let _ = s
        .execute(&format!("DROP TABLE IF EXISTS \"{t}\""), 10, &mut out)
        .await;
    let mut out = QueryOutcome::default();
    s.execute(
        &format!(
            r#"CREATE TABLE "{t}" {{"AttributeDefinitions": [{{"AttributeName": "pk", "AttributeType": "S"}}], "KeySchema": [{{"AttributeName": "pk", "KeyType": "HASH"}}], "BillingMode": "PAY_PER_REQUEST"}}"#
        ),
        10,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        &format!("INSERT INTO \"{t}\" VALUE {{'pk': 'abc', 'tags': ['x', 'y']}}"),
        10,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = QueryOutcome::default();
    let r = s
        .execute(
            &format!(
                "SELECT pk FROM \"{t}\" WHERE BEGINS_WITH(pk, 'ab') AND CONTAINS(tags, 'x') AND SIZE(tags) = 2 AND ATTRIBUTE_TYPE(pk, 'S')"
            ),
            10,
            &mut out,
        )
        .await;
    let mut drop = QueryOutcome::default();
    s.execute(&format!("DROP TABLE \"{t}\""), 10, &mut drop)
        .await
        .unwrap();
    r.unwrap();
    assert_eq!(out.results.last().map(|r| r.rows.len()), Some(1));
}

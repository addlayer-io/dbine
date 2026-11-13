//! The function catalog (#97) against a real Couchbase Server, already
//! initialized (as `tests/integration.rs` leaves it):
//! `DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891
//! cargo test -p dbine-driver-couchbase --test functions -- --ignored`.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

const USER: &str = "Administrator";
const PASS: &str = "secreto1";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert(
        "mgmt_port".into(),
        std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()),
    );
    Some(c)
}

#[tokio::test]
#[ignore]
async fn couchbase_functions() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE OR REPLACE FUNCTION dbine_fn_probe(a, b) { a + b }",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let c = s.function_catalog().await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("DROP FUNCTION dbine_fn_probe", 10, &mut out)
        .await
        .unwrap();

    eprintln!("{v}: {} functions", c.len());
    let find = |n: &str| c.iter().find(|f| f.name == n);
    assert_eq!(find("COUNT").unwrap().kind, FunctionKind::Aggregate);
    assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
    assert!(find("ARRAY_CONTAINS").is_some() && find("DATE_ADD_STR").is_some());
    // Vector search arrived in 7.6, its approximate distance in 8.0.
    let at = |m| dbine_driver::functions::version_at_least(&v, m);
    assert_eq!(find("VECTOR_DISTANCE").is_some(), at("7.6"), "{v}");
    assert_eq!(find("APPROX_VECTOR_DISTANCE").is_some(), at("8.0"), "{v}");
    // Not SQL++'s.
    assert!(find("approx_distinct").is_none() && find("JSON_VALUE").is_none());
    let udf = find("dbine_fn_probe").expect("the user's function");
    assert_eq!(udf.args, "(a, b)");
    assert_eq!(udf.schema, "default");
    assert_eq!(udf.kind, FunctionKind::Scalar);
}

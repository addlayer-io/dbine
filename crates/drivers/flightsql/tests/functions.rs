//! The function catalog (#97) from a real Flight SQL server (GizmoSQL,
//! DuckDB behind, and with its SQLite backend, `DATABASE_BACKEND=sqlite`):
//!
//! ```sh
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//! DBINE_TEST_FLIGHTSQL_SQLITE_URL=http://localhost:25338 \
//!   cargo test -p dbine-driver-flightsql --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    cfg_from("DBINE_TEST_FLIGHTSQL_URL")
}

fn cfg_from(env: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
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
async fn duckdb_behind_flight_sql_lists_its_functions() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE OR REPLACE MACRO dbine_fn_it(x, k) AS x * k",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let list = s.function_catalog().await;
    s.execute("DROP MACRO IF EXISTS dbine_fn_it", 10, &mut out)
        .await
        .unwrap();
    let list = list.unwrap();
    assert!(list.len() > 500, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    let f = find("lower").expect("lower");
    assert_eq!(f.kind, FunctionKind::Scalar);
    assert!(f.args.starts_with('('), "{f:?}");
    assert!(f.schema.is_empty());
    assert_eq!(
        find("approx_count_distinct").unwrap().kind,
        FunctionKind::Aggregate
    );
    assert_eq!(find("read_csv").unwrap().kind, FunctionKind::Table);
    let udf = find("dbine_fn_it").expect("the user's macro");
    assert_eq!(udf.schema, "main");
    assert_eq!(udf.args, "(x, k)");
    // Trino's and ClickHouse's, not DuckDB's.
    for absent in ["approx_distinct", "toStartOfDay", "~~"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

/// SQLite behind GizmoSQL: SQLite's `pragma_function_list`, not DuckDB's
/// list nor `GetSqlInfo`'s generic one.
#[tokio::test]
#[ignore]
async fn sqlite_behind_flight_sql_lists_its_functions() {
    let Some(c) = cfg_from("DBINE_TEST_FLIGHTSQL_SQLITE_URL") else {
        return;
    };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let list = s.function_catalog().await.unwrap();
    let find = |name: &str| list.iter().find(|f| f.name == name);
    assert!(find("sqlite_version").is_some());
    assert_eq!(find("substr").unwrap().args, "(x, y)");
    assert_eq!(
        find("pragma_table_info").map(|f| f.kind),
        Some(FunctionKind::Table)
    );
    // DuckDB's, not SQLite's.
    for absent in ["read_csv", "list_value"] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

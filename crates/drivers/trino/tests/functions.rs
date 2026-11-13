//! The function catalog (#97) from real servers:
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//! DBINE_TEST_PRESTO_URL=http://localhost:25181 \
//!   cargo test -p dbine-driver-trino --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome};

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

fn find<'a>(list: &'a [FunctionInfo], name: &str) -> Option<&'a FunctionInfo> {
    list.iter().find(|f| f.name == name)
}

async fn session(env: &str, id: &str) -> Option<Box<dyn dbine_driver::Session>> {
    let c = cfg(env, id)?;
    let d = dbine_driver_trino::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    Some(d.connect(&c, None).await.unwrap())
}

#[tokio::test]
#[ignore]
async fn trino_lists_its_functions() {
    let Some(mut s) = session("DBINE_TEST_TRINO_URL", "trino").await else {
        return;
    };
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS memory.dbine_fn_it CASCADE;
         CREATE SCHEMA memory.dbine_fn_it;
         USE memory.dbine_fn_it;
         CREATE FUNCTION memory.dbine_fn_it.dbine_twice(x bigint) RETURNS bigint COMMENT 'twice x' RETURN x * 2",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let list = s.function_catalog().await;
    s.execute(
        "DROP FUNCTION memory.dbine_fn_it.dbine_twice(bigint); DROP SCHEMA memory.dbine_fn_it",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let list = list.unwrap();
    assert!(list.len() > 500, "{}", list.len());

    let f = find(&list, "approx_distinct").expect("approx_distinct");
    assert_eq!(f.kind, FunctionKind::Aggregate);
    assert_eq!(f.returns, "bigint");
    let f = list
        .iter()
        .find(|f| f.name == "date_diff" && f.args == "(varchar(x), date, date)")
        .expect("date_diff");
    assert!(!f.description.is_empty());
    assert_eq!(
        find(&list, "row_number").unwrap().kind,
        FunctionKind::Window
    );
    assert!(list
        .iter()
        .any(|f| f.name == "sequence" && f.kind == FunctionKind::Table));
    let udf = find(&list, "dbine_twice").expect("the user's routine");
    assert_eq!(udf.schema, "memory.dbine_fn_it");
    assert_eq!(udf.args, "(bigint)");
    assert_eq!(udf.description, "twice x");
    // MySQL's and ClickHouse's, not Trino's.
    for absent in ["group_concat", "uniq", "toStartOfDay"] {
        assert!(find(&list, absent).is_none(), "{absent}");
    }
}

#[tokio::test]
#[ignore]
async fn presto_lists_its_functions() {
    let Some(mut s) = session("DBINE_TEST_PRESTO_URL", "presto").await else {
        return;
    };
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 500, "{}", list.len());
    assert_eq!(
        find(&list, "approx_distinct").unwrap().kind,
        FunctionKind::Aggregate
    );
    assert_eq!(
        find(&list, "row_number").unwrap().kind,
        FunctionKind::Window
    );
    for absent in ["group_concat", "uniq"] {
        assert!(find(&list, absent).is_none(), "{absent}");
    }
}

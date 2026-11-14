//! The function catalog (issue #97) on an in-memory database: DuckDB's own
//! list, the window functions and the user's macros.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

#[tokio::test]
async fn the_catalog_is_duckdbs_list() {
    let cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: ":memory:".into(),
        ..Default::default()
    };
    let d = dbine_driver_duckdb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE SCHEMA dbine_fn;
         CREATE MACRO dbine_fn.twice(a) AS a * 2;
         CREATE MACRO dbine_fn.upto(n) AS TABLE SELECT * FROM range(n);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("duckdb: {} entries", c.len());
    for f in c.iter().filter(|f| {
        ["string_split", "count", "read_csv", "concat", "row_number"].contains(&f.name.as_str())
    }) {
        eprintln!("{f:?}");
    }
    let kind = |n: &str| c.iter().find(|f| f.name == n).map(|f| f.kind);
    assert_eq!(kind("string_split"), Some(FunctionKind::Scalar));
    assert_eq!(kind("approx_count_distinct"), Some(FunctionKind::Aggregate));
    assert_eq!(kind("read_csv"), Some(FunctionKind::Table));
    assert_eq!(kind("row_number"), Some(FunctionKind::Window));
    assert_eq!(kind("lag"), Some(FunctionKind::Window));
    assert!(c
        .iter()
        .any(|f| f.name == "string_split" && f.schema.is_empty() && !f.description.is_empty()));
    let twice = c.iter().find(|f| f.name == "twice").unwrap();
    assert_eq!(
        (twice.kind, twice.schema.as_str(), twice.args.as_str()),
        (FunctionKind::Scalar, "dbine_fn", "(a)")
    );
    assert_eq!(kind("upto"), Some(FunctionKind::Table));
    // Another engine's, operators, internals.
    for n in ["approx_distinct", "->>", "+", "~~", "date_add_months"] {
        assert!(kind(n).is_none(), "{n}");
    }
    assert!(!c.iter().any(|f| f.name.starts_with("__")));

    s.execute("DROP SCHEMA dbine_fn CASCADE", 10, &mut out)
        .await
        .unwrap();
}

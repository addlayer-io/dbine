//! The function catalog (issue #97) on a real database file: what the
//! linked SQLite lists, and nothing it doesn't run (no server needed).

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};

#[tokio::test]
async fn the_catalog_is_what_this_build_runs() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-fn-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: path.display().to_string(),
        ..Default::default()
    };
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("sqlite: {} entries", c.len());
    let has = |n: &str| c.iter().any(|f| f.name == n);
    let kind = |n: &str| c.iter().find(|f| f.name == n).map(|f| f.kind);

    for n in ["abs", "substr", "json_extract", "printf", "strftime"] {
        assert_eq!(kind(n), Some(FunctionKind::Scalar), "{n}");
    }
    assert_eq!(kind("group_concat"), Some(FunctionKind::Aggregate));
    assert_eq!(kind("row_number"), Some(FunctionKind::Window));
    assert_eq!(kind("json_each"), Some(FunctionKind::Table));
    assert_eq!(kind("pragma_table_info"), Some(FunctionKind::Table));
    let substr = c.iter().find(|f| f.name == "substr").unwrap();
    assert!(substr.args.starts_with("(x, y"), "{substr:?}");
    // Not SQLite's, an operator, a keyword.
    for n in ["approx_distinct", "->>", "current_date", "date_trunc"] {
        assert!(!has(n), "{n}");
    }

    // Listed exactly when this build runs it (math functions and friends
    // depend on compile options).
    for (name, call) in [
        ("acos", "SELECT acos(1)"),
        ("concat", "SELECT concat('a', 'b')"),
        ("unhex", "SELECT unhex('41')"),
        ("soundex", "SELECT soundex('a')"),
        ("median", "SELECT median(1)"),
    ] {
        let mut out = QueryOutcome::default();
        let runs = s.execute(call, 10, &mut out).await.is_ok();
        assert_eq!(has(name), runs, "{name}");
    }

    drop(s);
    let _ = std::fs::remove_file(&path);
}

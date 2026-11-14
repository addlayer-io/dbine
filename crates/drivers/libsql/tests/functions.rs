//! The function catalog (issue #97) against a real libSQL server (sqld):
//!
//! ```sh
//! docker run -d --name dbine-test-libsql -p 25880:8080 ghcr.io/tursodatabase/libsql-server:latest
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 cargo test -p dbine-driver-libsql --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn the_catalog_is_what_the_server_lists() {
    let Ok(url) = std::env::var("DBINE_TEST_LIBSQL_URL") else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let mut cfg = ConnectionConfig {
        driver: "libsql".into(),
        host: url,
        ..Default::default()
    };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    let d = dbine_driver_libsql::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("libsql: {} entries", c.len());
    let kind = |n: &str| c.iter().find(|f| f.name == n).map(|f| f.kind);
    assert_eq!(kind("abs"), Some(FunctionKind::Scalar));
    assert_eq!(kind("json_extract"), Some(FunctionKind::Scalar));
    assert_eq!(kind("count"), Some(FunctionKind::Aggregate));
    assert_eq!(kind("row_number"), Some(FunctionKind::Window));
    assert_eq!(kind("json_each"), Some(FunctionKind::Table));
    // libSQL's own vector functions, which no static catalog has.
    let v = c.iter().find(|f| f.name == "vector_distance_cos").unwrap();
    assert_eq!(
        (v.kind, v.args.as_str()),
        (FunctionKind::Scalar, "(arg1, arg2)")
    );
    let substr = c.iter().find(|f| f.name == "substr").unwrap();
    assert!(substr.args.starts_with("(x, y"), "{substr:?}");
    for n in ["approx_distinct", "->>", "current_timestamp", "date_trunc"] {
        assert!(kind(n).is_none(), "{n}");
    }
}

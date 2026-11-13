//! The function catalog (#97) from a real Drill:
//!
//! ```sh
//! DBINE_TEST_DRILL_URL=http://localhost:25847 \
//!   cargo test -p dbine-driver-drill --test functions -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn drill_lists_its_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_DRILL_URL") else {
        return;
    };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "drill".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    let d = dbine_driver_drill::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let list = s.function_catalog().await.unwrap();
    assert!(list.len() > 200, "{}", list.len());
    let find = |name: &str| list.iter().find(|f| f.name == name);
    assert_eq!(find("sum").unwrap().kind, FunctionKind::Aggregate);
    assert_eq!(find("lower").unwrap().args, "(varchar)");
    assert_eq!(find("row_number").unwrap().kind, FunctionKind::Window);
    assert!(find("kvgen").is_some() && find("flatten").is_some());
    // Implementation names, and other engines' functions.
    for absent in [
        "castBIGINT",
        "extractDay",
        "greater_than",
        "approx_distinct",
        "uniq",
    ] {
        assert!(find(absent).is_none(), "{absent}");
    }
}

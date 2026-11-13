//! The operator catalog (#97) against a real server:
//! `DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin
//! cargo test -p dbine-driver-mongodb --test functions -- --ignored`.

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn mongodb_operators() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_URL") else {
        return;
    };
    let mut cfg = ConnectionConfig {
        driver: "mongodb".into(),
        database: "dbine_it".into(),
        read_only: true,
        ..Default::default()
    };
    cfg.options.insert("connection_string".into(), url);
    let d = dbine_driver_mongodb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} entries", c.len());
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in [
        "$match",
        "$group",
        "$lookup",
        "$sum",
        "$dateTrunc",
        "$setWindowFields",
        "$in",
        "$inc",
    ] {
        assert!(has(n), "{n}");
    }
    let find = c
        .iter()
        .find(|f| f.name == "find" && f.schema == "db.collection")
        .unwrap();
    assert_eq!(find.kind, FunctionKind::Scalar);
    // mongo:7: 8.0's operators are not offered.
    let v8 = dbine_driver::functions::version_at_least(&v, "8.0");
    assert_eq!(has("$toUUID"), v8, "{v}");
    assert_eq!(
        has("$median"),
        dbine_driver::functions::version_at_least(&v, "7.0")
    );
    assert!(!has("$nosuchop"));
}

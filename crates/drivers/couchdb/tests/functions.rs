//! The Mango operator catalog (#97) against a real server:
//! `DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202
//! cargo test -p dbine-driver-couchdb --test functions -- --ignored`.

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn couchdb_mango_operators() {
    let Ok(url) = std::env::var("DBINE_TEST_COUCHDB_URL") else {
        return;
    };
    let rest = url.strip_prefix("http://").unwrap();
    let (auth, host) = rest.split_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (h, p) = host.trim_end_matches('/').split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only: true,
        ..Default::default()
    };
    let d = dbine_driver_couchdb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} operators", c.len());
    assert!(c.iter().all(|f| f.kind == FunctionKind::Operator));
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in ["$eq", "$regex", "$elemMatch", "$allMatch", "$keyMapMatch"] {
        assert!(has(n), "{n}");
    }
    assert_eq!(
        has("$beginsWith"),
        dbine_driver::functions::version_at_least(&v, "3.4")
    );
    // MongoDB's, not Mango's.
    assert!(!has("$match") && !has("$where"));
}

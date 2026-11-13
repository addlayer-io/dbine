//! The command catalog (#97) against a real etcd:
//! `DBINE_TEST_ETCD_URL=http://localhost:25379 cargo test -p dbine-driver-etcd --test functions -- --ignored`.

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn etcd_commands() {
    let Ok(url) = std::env::var("DBINE_TEST_ETCD_URL") else {
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "etcd".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        read_only: true,
        ..Default::default()
    };
    let d = dbine_driver_etcd::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} commands", c.len());
    assert!(c.iter().all(|f| f.kind == FunctionKind::Command));
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in [
        "get",
        "put",
        "del",
        "lease grant",
        "member list",
        "role grant-permission",
    ] {
        assert!(has(n), "{n}");
    }
    assert_eq!(
        has("auth status"),
        dbine_driver::functions::version_at_least(&v, "3.5")
    );
    // Commands the editor refuses are not offered.
    assert!(!has("watch") && !has("lock") && !has("lease keep-alive"));
}

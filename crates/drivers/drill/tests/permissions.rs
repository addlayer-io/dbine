//! What the login may do, against a real Apache Drill (embedded mode, no
//! authentication: everyone may use the profiler):
//!
//! ```sh
//! docker run -d -i --name dbine-test-drill -p 25847:8047 apache/drill
//! DBINE_TEST_DRILL_URL=http://localhost:25847 cargo test -p dbine-driver-drill --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, Permissions};

#[tokio::test]
#[ignore]
async fn profiler_without_authentication() {
    let Ok(url) = std::env::var("DBINE_TEST_DRILL_URL") else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig { driver: "drill".into(), host: url.host_str().unwrap().into(), port: url.port().unwrap_or(0), ..Default::default() };
    let d = dbine_driver_drill::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    assert_eq!(s.permissions(Some("dfs.tmp")).await.unwrap(), Permissions { profiler: Access::Allowed, ..Default::default() });
}

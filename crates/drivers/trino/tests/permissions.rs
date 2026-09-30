//! What the login may do, against a real Trino:
//!
//! ```sh
//! docker run -d --name dbine-test-trino -p 25180:8080 trinodb/trino
//! DBINE_TEST_TRINO_URL=http://localhost:25180 cargo test -p dbine-driver-trino --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig};

#[tokio::test]
#[ignore]
async fn profiler_is_the_only_action() {
    let Ok(url) = std::env::var("DBINE_TEST_TRINO_URL") else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: "trino".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let p = s.permissions(Some("memory")).await.unwrap();
    assert_eq!(p.profiler, Access::Allowed);
    assert_eq!(p, dbine_driver::Permissions { profiler: Access::Allowed, ..Default::default() });
}

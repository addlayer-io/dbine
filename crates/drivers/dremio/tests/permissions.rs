//! What the login may do, against a real Dremio OSS (every user is an
//! administrator there; the first user is created if needed):
//!
//! ```sh
//! docker run -d --name dbine-test-dremio -p 25947:9047 dremio/dremio-oss
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, Permissions};
use serde_json::json;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

#[tokio::test]
#[ignore]
async fn community_users_are_admins() {
    let Ok(url) = std::env::var("DBINE_TEST_DREMIO_URL") else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http.get(format!("{base}/apiv2/server_status")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;

    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let p = s.permissions(Some("$scratch")).await.unwrap();
    let all_admin = Permissions {
        create_database: Access::Allowed,
        drop_database: Access::Allowed,
        profiler: Access::Allowed,
        ..Default::default()
    };
    assert_eq!(p, all_admin);
}

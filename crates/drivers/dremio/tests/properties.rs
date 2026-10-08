//! "Propiedades" of a source and a space against a real Dremio OSS, skipped
//! without `DBINE_TEST_DREMIO_URL` (the test creates the first user if
//! needed, and a NAS source over the container's /tmp):
//!
//! ```sh
//! docker run -d --name dbine-test-dremio -p 25947:9047 dremio/dremio-oss
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test properties -- --ignored
//! ```

use dbine_driver::ConnectionConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<(ConnectionConfig, String)> {
    let raw = std::env::var("DBINE_TEST_DREMIO_URL").ok()?;
    let url = reqwest::Url::parse(&raw).expect("URL");
    let c = ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    Some((c, raw.trim_end_matches('/').to_string()))
}

/// A fresh Dremio has no users: create the first one (ignored if it exists)
/// and log in.
async fn token(http: &reqwest::Client, base: &str) -> String {
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
    let login: Value = http.post(format!("{base}/apiv2/login")).json(&json!({"userName": USER, "password": PASS})).send().await.unwrap().json().await.unwrap();
    format!("_dremio{}", login["token"].as_str().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dremio_properties() {
    let Some((c, base)) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let auth = token(&http, &base).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    let _ = s.drop_database("dbine_props_space").await;
    let r = http
        .post(format!("{base}/api/v3/catalog"))
        .header("Authorization", &auth)
        .json(&json!({"entityType": "source", "name": "dbine_props", "type": "NAS", "config": {"path": "/tmp"}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap_or_default());

    let p = s.database_properties("dbine_props").await.unwrap();
    assert!(p.info.iter().any(|i| i.label == "Tipo" && i.value.contains("NAS")));
    assert!(!p.values["names_refresh_hours"].is_empty());

    let changes: BTreeMap<String, String> = [
        ("names_refresh_hours", "2"),
        ("dataset_expire_hours", "5"),
        ("dataset_update_mode", "PREFETCH_QUERIED"),
        ("auto_promote", "true"),
        ("reflection_refresh_hours", "3"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }

    // A space only shows its facts.
    s.create_database("dbine_props_space").await.unwrap();
    let p = s.database_properties("dbine_props_space").await.unwrap();
    assert!(p.fields.is_empty());
    assert!(p.info.iter().any(|i| i.value == "Espacio"));
    assert!(s.alter_database("dbine_props_space", &changes).await.is_err());

    s.drop_database("dbine_props_space").await.unwrap();
    s.drop_database("dbine_props").await.unwrap();
}

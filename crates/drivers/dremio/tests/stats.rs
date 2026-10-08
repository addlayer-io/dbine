//! A view's wiki as its comment, against a real Dremio OSS (the first user
//! is created if needed, as in `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome};
use serde_json::{json, Value};
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dremio_comments() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
        return;
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
    let _ = s.drop_database("dbine_st").await;
    s.create_database("dbine_st").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE VIEW dbine_st.v AS SELECT 1 AS id; CREATE VIEW dbine_st.sin AS SELECT 2 AS id", 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);

    // The wiki, written through the REST API.
    let login: Value = http.post(format!("{base}/apiv2/login")).json(&json!({"userName": USER, "password": PASS})).send().await.unwrap().json().await.unwrap();
    let auth = format!("_dremio{}", login["token"].as_str().unwrap());
    let v: Value = http.get(format!("{base}/api/v3/catalog/by-path/dbine_st/v")).header("Authorization", &auth).send().await.unwrap().json().await.unwrap();
    let id = v["id"].as_str().unwrap();
    let r = http
        .post(format!("{base}/api/v3/catalog/{id}/collaboration/wiki"))
        .header("Authorization", &auth)
        .json(&json!({"text": "Ventas por día"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);

    let mut s = d.connect(&c, Some("dbine_st")).await.unwrap();
    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(
        (comments[0].object.kind.as_str(), comments[0].object.schema.as_deref(), comments[0].object.name.as_str(), comments[0].comment.as_str()),
        (kinds::VIEW, Some("dbine_st"), "v", "Ventas por día")
    );
    assert!(s.row_estimates().await.unwrap().is_empty());
    drop(s);
    let mut s = d.connect(&c, None).await.unwrap();
    s.drop_database("dbine_st").await.unwrap();
}

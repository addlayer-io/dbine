//! "Nueva base de datos" (a bucket) with options, against a real,
//! provisioned node (see `integration.rs`, which provisions it), skipped
//! without `DBINE_TEST_COUCHBASE_URL`:
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test create_database -- --ignored
//! ```

use dbine_driver::ConnectionConfig;
use serde_json::Value;
use std::collections::BTreeMap;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn couchbase_options() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let mgmt = format!("http://{}:{}", c.host, c.options["mgmt_port"]);
    let d = dbine_driver_couchbase::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    let choices = s.create_database_choices().await.unwrap();
    eprintln!("{choices:?}");
    assert!(choices.iter().any(|c| c.key == "ram_quota"));

    let o: BTreeMap<String, String> = [
        ("bucket_type", "couchbase"),
        ("ram_quota", "128"),
        ("replicas", "0"),
        ("eviction", "fullEviction"),
        ("durability", "none"),
        ("flush", "true"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let b: Value = reqwest::Client::new()
        .get(format!("{mgmt}/pools/default/buckets/dbine_create_opts"))
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(b["quota"]["rawRAM"], 128 * 1024 * 1024);
    assert_eq!(b["replicaNumber"], 0);
    assert_eq!(b["evictionPolicy"], "fullEviction");
    assert!(b["controllers"]["flush"].is_string(), "flush enabled");
    s.drop_database("dbine_create_opts").await.unwrap();
    // maxTTL is Enterprise only: the server's refusal comes back as is.
    let ttl: BTreeMap<String, String> = [("max_ttl".to_string(), "60".to_string())].into();
    if let Err(e) = s.create_database_with("dbine_create_opts", &ttl).await {
        eprintln!("{e}");
    } else {
        s.drop_database("dbine_create_opts").await.unwrap();
    }

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

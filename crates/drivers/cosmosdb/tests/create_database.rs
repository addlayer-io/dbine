//! "Nueva base de datos" with shared throughput, against the emulator or an
//! account (see `integration.rs`), skipped without `DBINE_TEST_COSMOSDB_URL`:
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25203 \
//!   cargo test -p dbine-driver-cosmosdb --test create_database -- --ignored
//! ```
//! The key defaults to the emulator's well-known one
//! (`DBINE_TEST_COSMOSDB_KEY` overrides it).

use dbine_driver::ConnectionConfig;
use dbine_driver_cosmosdb::auth_header;
use serde_json::Value;
use std::collections::BTreeMap;

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

/// A signed GET of a feed (`rtype`, `link`, `path`).
async fn get(url: &str, key: &str, rtype: &str, link: &str, path: &str) -> Value {
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    http.get(format!("{url}{path}"))
        .header("Authorization", auth_header(key, "GET", rtype, link, &date).unwrap())
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn cosmosdb_options() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else {
        eprintln!("DBINE_TEST_COSMOSDB_URL not set; skipping");
        return;
    };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url.clone(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key.clone());
    let d = dbine_driver_cosmosdb::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;

    let o: BTreeMap<String, String> =
        [("throughput_mode", "manual"), ("throughput", "400")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let db = get(&url, &key, "dbs", "dbs/dbine_create_opts", "/dbs/dbine_create_opts").await;
    let rid = db["_rid"].as_str().unwrap().to_string();
    let offers = get(&url, &key, "offers", "", "/offers").await;
    let offer = offers["Offers"].as_array().unwrap().iter().find(|o| o["offerResourceId"] == rid.as_str()).cloned();
    eprintln!("{offer:?}");
    assert_eq!(offer.expect("the database's offer")["content"]["offerThroughput"], 400);
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

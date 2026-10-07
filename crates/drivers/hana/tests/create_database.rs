//! "Nueva base de datos" (a schema) with options, against a real SAP HANA
//! (`DBINE_TEST_HANA_URL`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:39041 \
//!   cargo test -p dbine-driver-hana --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
use std::collections::BTreeMap;

fn config(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("hana://").expect("hana://user:pass@host:port");
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (host, port) = addr.split_once(':').unwrap();
    ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn schema_owner() {
    let Ok(url) = std::env::var("DBINE_TEST_HANA_URL") else {
        eprintln!("DBINE_TEST_HANA_URL not set; skipping");
        return;
    };
    let d = dbine_driver_hana::drivers().remove(0);
    let mut s = d.connect(&config(&url), None).await.unwrap();
    let _ = s.drop_database("DBINE_CREATE_OPTS").await;
    let choices = s.create_database_choices().await.unwrap();
    let me = choices.iter().find(|c| c.key == "owner").and_then(|c| c.default.clone()).expect("current user");

    let options: BTreeMap<String, String> = [("owner".to_string(), me.clone())].into();
    eprintln!("{}", d.create_database_script("DBINE_CREATE_OPTS", &options).unwrap());
    s.create_database_with("DBINE_CREATE_OPTS", &options).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT SCHEMA_OWNER FROM SYS.SCHEMAS WHERE SCHEMA_NAME = 'DBINE_CREATE_OPTS'", 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], Value::String(me));
    s.drop_database("DBINE_CREATE_OPTS").await.unwrap();

    s.create_database_with("DBINE_CREATE_PLAIN", &BTreeMap::new()).await.unwrap();
    s.drop_database("DBINE_CREATE_PLAIN").await.unwrap();
}

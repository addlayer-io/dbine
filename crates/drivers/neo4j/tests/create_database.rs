//! "Nueva base de datos" with options, against Neo4j Enterprise (the
//! edition that creates databases), skipped without
//! `DBINE_TEST_NEO4J_EE_URL` (`user:pass@host:port`):
//!
//! ```sh
//! DBINE_TEST_NEO4J_EE_URL=neo4j:dbine-test-pass@localhost:17688 \
//!   cargo test -p dbine-driver-neo4j --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn neo4j_options() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_EE_URL") else {
        eprintln!("DBINE_TEST_NEO4J_EE_URL not set; skipping");
        return;
    };
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    let mut s = d.connect(&cfg, Some("system")).await.unwrap();
    let _ = s.drop_database("dbine-create-opts").await;
    let choices = s.create_database_choices().await.unwrap();
    eprintln!("{choices:?}");

    let o: BTreeMap<String, String> = [("primaries", "1"), ("secondaries", "0"), ("store_format", "aligned"), ("tx_log_enrichment", "DIFF")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    eprintln!("{}", d.create_database_script("dbine-create-opts", &o).unwrap());
    s.create_database_with("dbine-create-opts", &o).await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute(
        "SHOW DATABASES YIELD name, store, currentPrimariesCount, options WHERE name = 'dbine-create-opts' RETURN store, currentPrimariesCount, options",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let r = &out.results.last().unwrap().rows[0];
    eprintln!("{r:?}");
    assert!(r[0].as_str().unwrap_or_default().contains("aligned"), "{r:?}");
    assert_eq!(r[1], Value::from(1));
    assert!(r[2].to_string().contains("DIFF"), "{r:?}");
    s.drop_database("dbine-create-opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine-create-plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine-create-plain").await.unwrap();
}

//! "Nueva base de datos" with options, against a real server, skipped
//! without `DBINE_TEST_CLICKHOUSE_URL`:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test create_database -- --ignored
//! ```
//!
//! `Replicated` and `ON CLUSTER` need Keeper, which the test container
//! doesn't run: they are covered by the script's unit tests.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    match &out.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_options() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    let _ = s.drop_database("dbine_create_plain").await;

    let choices = s.create_database_choices().await.unwrap();
    let get = |k: &str| choices.iter().find(|c| c.key == k).unwrap_or_else(|| panic!("{k}"));
    assert_eq!(get("engine").default.as_deref(), Some("Atomic"));
    assert!(!get("cluster").values.is_empty(), "system.clusters");

    let options: BTreeMap<String, String> =
        [("engine", "Memory"), ("comment", "ventas 'históricas' \\ 2026")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    let row = scalar(&mut s, "SELECT concat(engine, '|', comment) FROM system.databases WHERE name = 'dbine_create_opts'").await;
    assert_eq!(row, "Memory|ventas 'históricas' \\ 2026");
    s.drop_database("dbine_create_opts").await.unwrap();

    let options: BTreeMap<String, String> = [("engine", "Atomic"), ("comment", "x")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT concat(engine, '|', comment) FROM system.databases WHERE name = 'dbine_create_opts'").await, "Atomic|x");
    s.drop_database("dbine_create_opts").await.unwrap();

    // A bad value never reaches the server.
    let bad: BTreeMap<String, String> = [("engine", "Replicated"), ("zoo_path", "/a'); DROP")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    assert!(s.create_database_with("dbine_create_opts", &bad).await.is_err());

    // Without options it's the plain create (Atomic).
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT engine FROM system.databases WHERE name = 'dbine_create_plain'").await, "Atomic");
    s.drop_database("dbine_create_plain").await.unwrap();
}

//! "Nueva base de datos" with options, against a real Amazon Athena (there
//! is no local emulator), skipped without it. Athena has no DDL that reads a
//! database's comment, location or properties back, so this checks that
//! the statement runs and the database appears; the script itself is
//! covered by the unit tests:
//!
//! ```sh
//! DBINE_TEST_ATHENA_DATABASE=dbine_test \
//! DBINE_TEST_ATHENA_LOCATION=s3://my-bucket/dbine-test/ \
//! DBINE_TEST_ATHENA_OUTPUT=s3://my-bucket/athena-results/ \
//! AWS_REGION=us-east-1 \
//!   cargo test -p dbine-driver-athena --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn athena_options() {
    let (Some(db), Some(location)) = (env("DBINE_TEST_ATHENA_DATABASE"), env("DBINE_TEST_ATHENA_LOCATION")) else {
        eprintln!("DBINE_TEST_ATHENA_DATABASE / DBINE_TEST_ATHENA_LOCATION not set; skipping");
        return;
    };
    let mut cfg = ConnectionConfig { driver: "athena".into(), database: db, ..Default::default() };
    if let Some(o) = env("DBINE_TEST_ATHENA_OUTPUT") {
        cfg.options.insert("output_location".into(), o);
    }
    if let Some(r) = env("AWS_REGION") {
        cfg.options.insert("region".into(), r);
    }
    let d = dbine_driver_athena::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = s.drop_database("dbine_create_opts").await;

    let options: BTreeMap<String, String> = [
        ("comment", "ventas 'históricas'".to_string()),
        ("location", format!("{}/dbine_create_opts/", location.trim_end_matches('/'))),
        ("properties", "creador=dbine\nequipo=ventas".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    assert!(s.list_databases().await.unwrap().iter().any(|d| d == "dbine_create_opts"));
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

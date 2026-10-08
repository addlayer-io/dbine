//! "Propiedades" of a database against a real Amazon Athena (there is no
//! local emulator), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ATHENA_DATABASE=dbine_test \
//! DBINE_TEST_ATHENA_LOCATION=s3://my-bucket/dbine-test/ \
//! DBINE_TEST_ATHENA_OUTPUT=s3://my-bucket/athena-results/ \
//! AWS_REGION=us-east-1 \
//!   cargo test -p dbine-driver-athena --test properties -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn athena_properties() {
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
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = s.drop_database("dbine_props").await;
    let options: BTreeMap<String, String> = [
        ("comment", "ventas".to_string()),
        ("location", format!("{}/dbine_props/", location.trim_end_matches('/'))),
        ("properties", "creador=dbine".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    s.create_database_with("dbine_props", &options).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("prop:creador").map(String::as_str), Some("dbine"));
    assert!(p.info.iter().any(|i| i.value == "ventas"), "the description as a fact");

    let changes: BTreeMap<String, String> =
        [("prop:creador", "ana o'neil"), ("properties_add", "equipo=ventas")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values["prop:creador"], "ana o'neil");
    assert_eq!(p.values.get("prop:equipo").map(String::as_str), Some("ventas"));
    s.drop_database("dbine_props").await.unwrap();
}

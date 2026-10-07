//! "Nueva base de datos" with options, against a real TDengine
//! (taosAdapter's REST port), skipped without `DBINE_TEST_TDENGINE_URL`:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn tdengine_options() {
    let Some(url) = std::env::var("DBINE_TEST_TDENGINE_URL").ok() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;

    let o: BTreeMap<String, String> = [
        ("precision", "us"),
        ("keep", "30d"),
        ("duration", "1d"),
        ("replica", "1"),
        ("vgroups", "1"),
        ("buffer", "16"),
        ("cachemodel", "last_row"),
        ("cachesize", "2"),
        ("wal_level", "2"),
        ("wal_fsync_period", "1000"),
        ("comp", "1"),
        ("stt_trigger", "4"),
        ("single_stable", "true"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let sql = d.create_database_script("dbine_create_opts", &o).unwrap();
    eprintln!("{sql}");
    s.create_database_with("dbine_create_opts", &o).await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT `precision`, `keep`, `duration`, `replica`, `vgroups`, `buffer`, `cachemodel`, `cachesize`, `wal_level`, `wal_fsync_period`, `comp`, `stt_trigger`, `single_stable` \
         FROM information_schema.ins_databases WHERE name = 'dbine_create_opts'",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let r = &out.results.last().unwrap().rows[0];
    let t: Vec<String> = r.iter().map(|v| match v { Value::String(s) => s.clone(), v => v.to_string() }).collect();
    eprintln!("{t:?}");
    assert_eq!(t[0], "us");
    assert!(t[1].starts_with("30d"), "{t:?}");
    assert_eq!(t[2], "1d");
    assert_eq!(&t[3..], ["1", "1", "16", "last_row", "2", "2", "1000", "1", "4", "true"].map(String::from), "{t:?}");
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

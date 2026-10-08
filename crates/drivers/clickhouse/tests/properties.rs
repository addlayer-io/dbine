//! "Propiedades" of a database against a real server, skipped without
//! `DBINE_TEST_CLICKHOUSE_URL`:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test properties -- --ignored
//! ```
//!
//! `MODIFY SETTING` needs a MaterializedPostgreSQL or DataLakeCatalog
//! database (another server): it's covered by the script's unit tests.

use dbine_driver::ConnectionConfig;
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

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_properties() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    let o: BTreeMap<String, String> = [("comment".to_string(), "antes".to_string())].into();
    s.create_database_with("dbine_props", &o).await.unwrap();
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute("CREATE TABLE dbine_props.t (id UInt32) ENGINE = MergeTree ORDER BY id", 1, &mut out).await.unwrap();
    s.execute("INSERT INTO dbine_props.t SELECT number FROM numbers(1000)", 1, &mut out).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("comment").map(String::as_str), Some("antes"));
    let fact = |p: &dbine_driver::DatabaseProperties, l: &str| p.info.iter().find(|i| i.label == l).map(|i| i.value.clone());
    assert_eq!(fact(&p, "Motor (ENGINE)").as_deref(), Some("Atomic"));
    assert_eq!(fact(&p, "Tablas").as_deref(), Some("1"));
    assert_eq!(fact(&p, "Filas").as_deref(), Some("1000"));
    assert!(!p.fields.iter().any(|f| f.key == "settings_add"), "Atomic takes no MODIFY SETTING");

    let changes: BTreeMap<String, String> = [("comment".to_string(), "ventas 'históricas' \\ 2026".to_string())].into();
    eprintln!("{}", d.alter_database_script("dbine_props", &changes).unwrap());
    s.alter_database("dbine_props", &changes).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values["comment"], "ventas 'históricas' \\ 2026");

    // Atomic refuses settings: the server's error comes back.
    let bad: BTreeMap<String, String> = [("setting:x".to_string(), "1".to_string())].into();
    assert!(s.alter_database("dbine_props", &bad).await.is_err());
    // Two changes, the second fails: the error says how many ran.
    let two: BTreeMap<String, String> = [("comment".to_string(), String::new()), ("setting:x".to_string(), "1".to_string())].into();
    let e = s.alter_database("dbine_props", &two).await.unwrap_err().to_string();
    assert!(e.contains("se aplicaron 1 de 2"), "{e}");
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["comment"], "");

    s.drop_database("dbine_props").await.unwrap();
}

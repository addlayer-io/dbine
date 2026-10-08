//! "Propiedades" of the database against a real libSQL server
//! (`DBINE_TEST_LIBSQL_URL`, optionally `DBINE_TEST_LIBSQL_TOKEN`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 \
//!   cargo test -p dbine-driver-libsql --test properties -- --ignored
//! ```
//!
//! A server has one database, so the test changes `user_version` and puts
//! it back.

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_LIBSQL_URL").ok()?;
    let mut cfg = ConnectionConfig { driver: "libsql".into(), host: url, ..Default::default() };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    Some(cfg)
}

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
#[ignore]
async fn libsql_properties() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_libsql::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();

    let p = s.database_properties("main").await.unwrap();
    assert_eq!(p.fields.iter().map(|f| f.key).collect::<Vec<_>>(), vec!["user_version"]);
    let before = p.values.get("user_version").cloned().expect("user_version");
    assert!(p.info.iter().any(|i| i.label == "Modo del diario (journal_mode)" && i.value.starts_with("WAL")), "{:?}", p.info);
    assert!(p.info.iter().any(|i| i.label == "Tamaño"), "{:?}", p.info);

    let next = (before.parse::<i32>().unwrap() + 1).to_string();
    assert_eq!(d.alter_database_script("main", &changes(&[("user_version", &next)])).unwrap(), format!("PRAGMA main.user_version = {next};"));
    s.alter_database("main", &changes(&[("user_version", &next)])).await.unwrap();
    assert_eq!(s.database_properties("main").await.unwrap().values.get("user_version"), Some(&next));

    // What sqld manages is refused before it reaches the server.
    assert!(s.alter_database("main", &changes(&[("journal_mode", "DELETE")])).await.is_err());

    s.alter_database("main", &changes(&[("user_version", &before)])).await.unwrap();
    assert_eq!(s.database_properties("main").await.unwrap().values.get("user_version"), Some(&before));

    // Read-only connections refuse it.
    let mut ro = d.connect(&ConnectionConfig { read_only: true, ..cfg }, None).await.unwrap();
    assert!(ro.alter_database("main", &changes(&[("user_version", "1")])).await.is_err());
}

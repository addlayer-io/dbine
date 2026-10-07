//! "Nueva base de datos" with options, against a real server
//! (`DBINE_TEST_ORIENTDB_URL`, `user:pass@host:port`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn orientdb_options() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    let cfg = |db: Option<&str>| ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        database: db.unwrap_or_default().into(),
        ..Default::default()
    };
    let d = dbine_driver_orientdb::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&cfg(None), None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;

    let o: BTreeMap<String, String> = [("storage".to_string(), "memory".to_string())].into();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let mut db = d.connect(&cfg(Some("dbine_create_opts")), Some("dbine_create_opts")).await.unwrap();
    let mut out = QueryOutcome::default();
    db.execute("SELECT type FROM metadata:storage", 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], Value::String("memory".into()));
    drop(db);
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create, on disk.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

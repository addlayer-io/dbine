//! "Nueva base de datos" through ODBC, each test skipped without its
//! variable:
//!
//! - `DBINE_TEST_ODBC_CONN`: the generic preset (no options: the plain
//!   create), e.g. SQL Server through its ODBC driver.
//! - `DBINE_TEST_ODBC_ASE_CONN`: a Sybase ASE connection string, for the
//!   device options (there's no practical Docker image for it).
//!
//! ```sh
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25013;UID=sa;PWD={Pw_12345!};TrustServerCertificate=yes' \
//!   cargo test -p dbine-driver-odbc --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

fn cfg(preset: &str, env: &str) -> Option<ConnectionConfig> {
    let conn = std::env::var(env).ok()?;
    Some(ConnectionConfig {
        driver: preset.into(),
        options: [("connection_string".to_string(), conn)].into(),
        ..Default::default()
    })
}

async fn open(preset: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == preset).unwrap();
    d.connect(cfg, None).await.expect("connect")
}

#[tokio::test]
#[ignore]
async fn generic_plain_create() {
    let Some(cfg) = cfg("odbc", "DBINE_TEST_ODBC_CONN") else {
        eprintln!("DBINE_TEST_ODBC_CONN not set; skipping");
        return;
    };
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(d.create_database_fields().is_empty());
    let mut s = open("odbc", &cfg).await;
    let _ = s.drop_database("dbine_create_plain").await;
    assert!(s.create_database_with("dbine_create_plain", &[("x".to_string(), "y".to_string())].into()).await.is_err());
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    assert!(s.list_databases().await.unwrap().iter().any(|d| d == "dbine_create_plain"));
    s.drop_database("dbine_create_plain").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn ase_devices() {
    let Some(cfg) = cfg("sybase", "DBINE_TEST_ODBC_ASE_CONN") else {
        eprintln!("DBINE_TEST_ODBC_ASE_CONN not set; skipping");
        return;
    };
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "sybase").unwrap();
    let mut s = open("sybase", &cfg).await;
    let _ = s.drop_database("dbine_create_opts").await;
    let choices = s.create_database_choices().await.unwrap();
    let device = choices.iter().find(|c| c.key == "data_device").and_then(|c| c.values.first().cloned()).expect("a database device");
    let options: BTreeMap<String, String> =
        [("data_device", device.as_str()), ("data_size", "20M")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("sp_helpdb dbine_create_opts", 10, &mut out).await.unwrap();
    eprintln!("{:?}", out.results.iter().map(|r| &r.rows).collect::<Vec<_>>());
    s.drop_database("dbine_create_opts").await.unwrap();
}

//! `Session::permissions` against the emulator: the account key can't be
//! told apart (unknown), and DBine's read-only mode doesn't change that (it
//! blocks the writes on its own).
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Permissions};

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COSMOSDB_URL").ok()?;
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url, trust_server_certificate: true, read_only, ..Default::default() };
    c.options.insert("account_key".into(), key);
    Some(c)
}

#[tokio::test]
#[ignore]
async fn key_and_read_only() {
    let Some(c) = cfg(false) else {
        eprintln!("DBINE_TEST_COSMOSDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let p = s.permissions(None).await.unwrap();
    eprintln!("account key: {p:?}");
    assert_eq!(p, Permissions::default());
    let mut s = d.connect(&cfg(true).unwrap(), None).await.expect("connect read-only");
    let p = s.permissions(Some("x")).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    assert_eq!(p, Permissions::default());
    // The read-only mode still blocks the change.
    assert!(s.create_database("dbine_perm").await.is_err());
}

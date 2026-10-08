//! "Renombrar…" against a real libSQL server (`DBINE_TEST_LIBSQL_URL`,
//! optionally `DBINE_TEST_LIBSQL_TOKEN`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 \
//!   cargo test -p dbine-driver-libsql --test rename -- --ignored
//! ```
//!
//! The same flow as SQLite's (`../sqlite/tests/support/rename_flow.rs`):
//! the server refuses `PRAGMA legacy_alter_table`, which is off there, and
//! the script never sends it.

#[path = "../../sqlite/tests/support/rename_flow.rs"]
mod rename_flow;

use dbine_driver::ConnectionConfig;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_LIBSQL_URL").ok()?;
    let mut cfg = ConnectionConfig { driver: "libsql".into(), host: url, ..Default::default() };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    Some(cfg)
}

#[tokio::test]
#[ignore]
async fn libsql_rename_with_impact() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_libsql::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    rename_flow::rename_flow(d.as_ref(), s.as_mut()).await;
}

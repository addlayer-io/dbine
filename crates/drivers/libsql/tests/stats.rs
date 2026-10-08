//! Row estimates against a real libSQL server (`DBINE_TEST_LIBSQL_URL`,
//! optionally `DBINE_TEST_LIBSQL_TOKEN`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 \
//!   cargo test -p dbine-driver-libsql --test stats -- --ignored
//! ```
//!
//! sqld refuses `ANALYZE` ("unsupported statement") and reserves the name
//! `sqlite_stat1`, so a client can't create the statistics: they exist
//! only in a database file analyzed before it was loaded. The test checks
//! that, without them, the answer is empty and nothing is counted; reading
//! `sqlite_stat1` is shared with SQLite and tested there.

use dbine_driver::{ConnectionConfig, QueryOutcome};

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
async fn libsql_row_estimates() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_libsql::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS stats_plain;
         CREATE TABLE stats_plain (v TEXT);
         INSERT INTO stats_plain VALUES ('a'), ('b'), ('c');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(s.execute("ANALYZE stats_plain", 10, &mut out).await.is_err(), "sqld now runs ANALYZE: test the counts too");

    let stat1 = s.execute("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'", 10, &mut out).await;
    assert!(stat1.is_ok());
    let got = s.row_estimates().await.unwrap();
    assert!(got.iter().all(|e| e.object.name != "stats_plain"), "{got:?}");
    assert!(s.object_comments().await.unwrap().is_empty());

    s.execute("DROP TABLE stats_plain;", 10, &mut out).await.unwrap();
}

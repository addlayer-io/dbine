//! The function catalog (issue #97) against the Linux emulator (see
//! `integration.rs` for the container), skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25203 \
//!   cargo test -p dbine-driver-cosmosdb --test functions -- --ignored --nocapture
//! ```
//!
//! The catalog is static; this checks the session hands it out on a real
//! connection, and that the emulator runs a few of its functions.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};
use serde_json::json;

const EMULATOR_KEY: &str =
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

#[tokio::test]
#[ignore]
async fn cosmosdb_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else {
        eprintln!("DBINE_TEST_COSMOSDB_URL not set; skipping");
        return;
    };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let cfg = |db: &str| {
        let mut c = ConnectionConfig {
            driver: "cosmosdb".into(),
            host: url.clone(),
            database: db.into(),
            trust_server_certificate: true,
            ..Default::default()
        };
        c.options.insert("account_key".into(), key.clone());
        c
    };
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    let db = "dbine_functions";
    let mut admin = d.connect(&cfg(""), None).await.expect("connect");
    let _ = admin.drop_database(db).await;
    admin.create_database(db).await.unwrap();

    let mut s = d.connect(&cfg(db), Some(db)).await.unwrap();
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    let get = |n: &str| c.iter().find(|f| f.name == n);
    assert_eq!(get("COUNT").unwrap().kind, FunctionKind::Aggregate);
    assert!(get("ARRAY_CONTAINS").is_some());
    assert!(get("STRINGEQUALS").is_some());
    assert!(get("approx_distinct").is_none());
    assert!(get("JSON_VALUE").is_none());

    // A few of them, on a container of the test database.
    let mut out = QueryOutcome::default();
    let run = s
        .execute(
            "CREATE CONTAINER \"items\" {\"partitionKey\": {\"paths\": [\"/pk\"], \"kind\": \"Hash\"}};\n\
             INSERT INTO \"items\" {\"id\": \"1\", \"pk\": \"a\", \"tags\": [\"x\"], \"name\": \"Ana\"};\n\
             -- container: items\n\
             SELECT VALUE [ABS(-2), ARRAY_CONTAINS(c.tags, \"x\"), UPPER(c.name)] FROM c",
            10,
            &mut out,
        )
        .await;
    drop(s);
    admin.drop_database(db).await.unwrap();
    run.unwrap();
    let last = out.results.last().unwrap();
    // Nested values come as compact JSON.
    assert_eq!(
        last.rows[0][0],
        json!("[2,true,\"ANA\"]"),
        "{:?}",
        last.rows
    );
}

//! The function catalog (issue #97) against a real server (see
//! `integration.rs` for the container), skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn orientdb_catalog() {
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
    let name = "dbine_functions";
    let mut admin = d.connect(&cfg(None), None).await.unwrap();
    let _ = admin.drop_database(name).await;
    let o: BTreeMap<String, String> = [("storage".to_string(), "memory".to_string())].into();
    admin.create_database_with(name, &o).await.unwrap();

    // Without a database: the bundled functions only.
    let c = admin.function_catalog().await.unwrap();
    assert!(c.iter().any(|f| f.name == "shortestPath"));

    let mut s = d.connect(&cfg(Some(name)), Some(name)).await.unwrap();
    println!("{}", s.server_version().await.unwrap());
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE FUNCTION dbine_discount \"SELECT :price * (1 - :rate)\" \
         PARAMETERS [price, rate] LANGUAGE SQL",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let c = s.function_catalog().await;
    drop(s);
    admin.drop_database(name).await.unwrap();
    let c = c.unwrap();
    println!("{} functions", c.len());
    let get = |n: &str| c.iter().find(|f| f.name == n);
    assert_eq!(get("count").unwrap().kind, FunctionKind::Aggregate);
    assert!(get("outE").is_some());
    // 3.1.0 and later.
    assert!(get("strcmpci").is_some());
    // Not OrientDB's.
    assert!(get("approx_distinct").is_none());
    assert!(get("JSON_VALUE").is_none());
    // The database's own function, with its parameters.
    let f = get("dbine_discount").expect("the database's function");
    assert_eq!(f.args, "(price, rate)");
    assert_eq!(f.kind, FunctionKind::Scalar);
}

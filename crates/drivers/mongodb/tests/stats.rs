//! Row estimates and object comments against real servers, skipped without
//! their connection strings (`DBINE_TEST_MONGODB_URL`,
//! `DBINE_TEST_FERRETDB_URL`):
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL='mongodb://root:secret@localhost:25201/?authSource=admin' \
//! DBINE_TEST_FERRETDB_URL='mongodb://root:secret@localhost:25203/' \
//!   cargo test -p dbine-driver-mongodb --test stats -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

async fn check(env: &str, driver: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut c = ConnectionConfig { driver: driver.into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_stats").await;
    s.create_database("dbine_stats").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "use dbine_stats\ndb.items.insertMany([{ a: 1 }, { a: 2 }, { a: 3 }])\ndb.empty.insertOne({ a: 1 })\ndb.empty.deleteMany({})\n\
         db.createView('items_view', 'items', [{ $match: { a: { $gt: 1 } } }])",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let rows = s.row_estimates().await.unwrap();
    let get = |n: &str| rows.iter().find(|e| e.object.name == n).map(|e| (e.object.kind.clone(), e.rows));
    assert_eq!(get("items"), Some(("collection".to_string(), 3)), "{driver}: {rows:?}");
    assert_eq!(get("empty"), Some(("collection".to_string(), 0)), "{driver}: {rows:?}");
    if driver == "ferretdb" {
        // FerretDB takes `create` with `viewOn` but makes a plain, empty collection.
        assert_eq!(get("items_view"), Some(("collection".to_string(), 0)), "{driver}: {rows:?}");
    } else {
        assert_eq!(get("items_view"), None, "{driver}: views have no count");
    }
    assert!(s.object_comments().await.unwrap().is_empty());

    s.drop_database("dbine_stats").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mongodb_stats() {
    check("DBINE_TEST_MONGODB_URL", "mongodb").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn ferretdb_stats() {
    check("DBINE_TEST_FERRETDB_URL", "ferretdb").await;
}

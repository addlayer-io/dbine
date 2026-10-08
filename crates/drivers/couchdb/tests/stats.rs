//! Row estimates and object comments against a real server
//! (`DBINE_TEST_COUCHDB_URL`, `http://user:pass@host:port`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test stats -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn couchdb_stats() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchdb::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_stats").await;
    s.create_database("dbine_stats").await.unwrap();
    let mut out = QueryOutcome::default();
    for (id, n) in [("a", 1), ("b", 2), ("c", 3)] {
        s.execute(&format!("POST /dbine_stats {{\"_id\": \"{id}\", \"n\": {n}}}"), 10, &mut out).await.unwrap();
    }
    let mut s = d.connect(&c, Some("dbine_stats")).await.unwrap();

    let rows = s.row_estimates().await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].object.kind.as_str(), rows[0].object.name.as_str(), rows[0].rows), ("collection", "_all_docs", 3));
    assert!(s.object_comments().await.unwrap().is_empty());

    let mut admin = d.connect(&c, None).await.unwrap();
    admin.drop_database("dbine_stats").await.unwrap();
}

//! Object comments against a real TDengine (taosAdapter's REST port),
//! skipped without `DBINE_TEST_TDENGINE_URL`:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test stats -- --ignored --nocapture
//! ```
//!
//! TDengine keeps no row counts in its catalog, so `row_estimates` is empty.

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
#[ignore]
async fn tdengine_object_comments() {
    let Some(url) = std::env::var("DBINE_TEST_TDENGINE_URL").ok() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().into_iter().next().unwrap();
    let db = "dbine_stats";
    {
        let mut s = d.connect(&cfg, None).await.unwrap();
        let _ = s.drop_database(db).await;
        s.create_database(db).await.unwrap();
    }
    let mut s = d.connect(&cfg, Some(db)).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE STABLE m (ts TIMESTAMP, v INT) TAGS (d INT) COMMENT 'medidas';
         CREATE TABLE c1 USING m TAGS (1) COMMENT 'sensor uno';
         CREATE TABLE c2 USING m TAGS (2);
         INSERT INTO c1 VALUES (now, 1);",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let objects = s.list_objects().await.unwrap();
    let got: Vec<_> = s.object_comments().await.unwrap().into_iter().map(|c| (c.object.kind, c.object.name, c.comment)).collect();
    assert_eq!(got, vec![("subtable".to_string(), "c1".to_string(), "sensor uno".to_string())]);
    assert!(objects.iter().any(|o| o.kind == got[0].0 && o.name == got[0].1), "{objects:?}");
    assert!(s.row_estimates().await.unwrap().is_empty());

    s.drop_database(db).await.unwrap();
}

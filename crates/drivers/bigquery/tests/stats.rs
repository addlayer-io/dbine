//! Row estimates and descriptions from the REST API (`tables.get`,
//! `routines.get`) against bigquery-emulator:
//!
//! ```sh
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bigquery_stats() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url.clone());
    let mut s = dbine_driver_bigquery::drivers().pop().unwrap().connect(&c, Some("ds1")).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP VIEW IF EXISTS ds1.st_v", 10, &mut out).await;
    let _ = s.execute("DROP TABLE IF EXISTS ds1.st_t", 10, &mut out).await;
    s.execute("CREATE TABLE ds1.st_t (id INT64)", 10, &mut out).await.unwrap();
    s.execute("INSERT INTO ds1.st_t (id) VALUES (1), (2), (3)", 10, &mut out).await.unwrap();
    // The view and its description through the tables API.
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{url}/bigquery/v2/projects/test/datasets/ds1/tables"))
        .json(&json!({"tableReference": {"projectId": "test", "datasetId": "ds1", "tableId": "st_v"},
            "description": "ventas por id", "view": {"query": "SELECT id FROM ds1.st_t", "useLegacySql": false}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);

    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let t = rows.iter().find(|r| r.object.name == "st_t").expect("st_t");
    assert_eq!((t.object.kind.as_str(), t.rows), (kinds::TABLE, 3));
    assert!(rows.iter().all(|r| r.object.name != "st_v"));

    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    assert!(comments.iter().any(|c| c.object.kind == kinds::VIEW && c.object.name == "st_v" && c.comment == "ventas por id"), "{comments:?}");

    let _ = s.execute("DROP VIEW ds1.st_v", 10, &mut out).await;
    let _ = s.execute("DROP TABLE ds1.st_t", 10, &mut out).await;
}

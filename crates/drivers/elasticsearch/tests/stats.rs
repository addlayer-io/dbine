//! Row estimates and object comments against real servers, skipped without
//! their URLs:
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test stats -- --ignored
//! ```
//!
//! Point it only at a `dbine-test-*` server of your own (never
//! `dbine-test-shots-*`): it creates and deletes `dbine_stats*` objects.

use dbine_driver::{ConnectionConfig, QueryOutcome};

const SEED: &str = r#"
PUT /dbine_stats
{"mappings":{"properties":{"n":{"type":"integer"}}}}

POST /_bulk?refresh=true
{"index":{"_index":"dbine_stats","_id":"1"}}
{"n":1}
{"index":{"_index":"dbine_stats","_id":"2"}}
{"n":2}
{"index":{"_index":"dbine_stats","_id":"3"}}
{"n":3}

PUT /_index_template/dbine_stats_ds
{"index_patterns":["dbine-stats-ds*"],"data_stream":{},"priority":500}

POST /dbine-stats-ds/_doc?refresh=true
{"@timestamp":"2026-01-01T00:00:00Z","n":1}

POST /dbine-stats-ds/_doc?refresh=true
{"@timestamp":"2026-01-02T00:00:00Z","n":2}
"#;

const CLEANUP: &str = "DELETE /_data_stream/dbine-stats-ds\n\nDELETE /_index_template/dbine_stats_ds\n\nDELETE /dbine_stats\n";

async fn check(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    for stmt in CLEANUP.split("\n\n") {
        let _ = s.execute(stmt, 10, &mut QueryOutcome::default()).await;
    }
    s.execute(SEED, 10, &mut QueryOutcome::default()).await.unwrap();

    let rows = s.row_estimates().await.unwrap();
    let get = |n: &str| rows.iter().find(|e| e.object.name == n).map(|e| (e.object.kind.clone(), e.rows));
    assert_eq!(get("dbine_stats"), Some(("index".to_string(), 3)), "{id}: {rows:?}");
    assert_eq!(get("dbine-stats-ds"), Some(("stream".to_string(), 2)), "{id}: {rows:?}");
    // Backing indices are hidden (`.ds-…`), as in the object list.
    assert!(!rows.iter().any(|e| e.object.name.starts_with(".ds-dbine-stats-ds")), "{id}: {rows:?}");
    assert!(s.object_comments().await.unwrap().is_empty());

    s.execute(CLEANUP, 10, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch_stats() {
    check("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opensearch_stats() {
    check("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

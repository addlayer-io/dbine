//! Row estimates from the Drill Metastore against a real Apache Drill
//! (embedded mode, where the metastore is a local Iceberg one):
//!
//! ```sh
//! DBINE_TEST_DRILL_URL=http://localhost:25847 \
//!   cargo test -p dbine-driver-drill --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "drill".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn drill_stats() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DRILL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_drill::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&cfg, Some("dfs.tmp")).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS dfs.tmp.dbine_stats_t", 10, &mut out).await;
    run(&mut s, "CREATE TABLE dfs.tmp.dbine_stats_t AS SELECT * FROM (VALUES (1), (2), (3)) AS t(id)").await;

    // Without the metastore Drill keeps no count.
    let rows = s.row_estimates().await.unwrap();
    assert!(rows.iter().all(|r| r.object.name != "dbine_stats_t"), "{rows:?}");

    run(&mut s, "ALTER SESSION SET `metastore.enabled` = true").await;
    run(&mut s, "ANALYZE TABLE dfs.tmp.dbine_stats_t REFRESH METADATA").await;
    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let t = rows.iter().find(|r| r.object.name == "dbine_stats_t").expect("dbine_stats_t");
    assert_eq!((t.object.kind.as_str(), t.object.schema.as_deref(), t.rows), (kinds::TABLE, Some("dfs.tmp"), 3));
    assert!(s.object_comments().await.unwrap().is_empty());
    run(&mut s, "DROP TABLE dfs.tmp.dbine_stats_t").await;
}

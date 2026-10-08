//! Row estimates (`SHOW STATS`) and view comments against a real Trino,
//! on the memory connector, which keeps a row count per table:
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//!   cargo test -p dbine-driver-trino --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TRINO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "trino".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trino_stats() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_TRINO_URL not set; skipping");
        return;
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == "trino").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP SCHEMA memory.dbine_stats CASCADE", 10, &mut out).await;
    run(&mut s, "CREATE SCHEMA memory.dbine_stats").await;
    run(&mut s, "CREATE TABLE memory.dbine_stats.ventas AS SELECT * FROM (VALUES 1, 2, 3) AS t(id)").await;
    run(&mut s, "CREATE VIEW memory.dbine_stats.v AS SELECT id FROM memory.dbine_stats.ventas").await;
    run(&mut s, "COMMENT ON VIEW memory.dbine_stats.v IS 'ventas por id'").await;

    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let ventas = rows.iter().find(|r| r.object.name == "ventas").expect("ventas");
    assert_eq!((ventas.object.kind.as_str(), ventas.object.schema.as_deref(), ventas.rows), (kinds::TABLE, Some("dbine_stats"), 3));
    assert!(rows.iter().all(|r| r.object.name != "v"));

    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    assert!(comments.iter().any(|c| c.object.kind == kinds::VIEW && c.object.name == "v" && c.comment == "ventas por id"), "{comments:?}");
    run(&mut s, "DROP SCHEMA memory.dbine_stats CASCADE").await;
}

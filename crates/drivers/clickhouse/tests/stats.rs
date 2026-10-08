//! Row estimates and object comments against real servers, read from
//! `system.tables` (no table is read):
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
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
async fn clickhouse_stats() {
    let Some(cfg) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_stats").await;
    s.create_database("dbine_stats").await.unwrap();
    run(&mut s, "CREATE TABLE dbine_stats.ventas (id UInt64) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO dbine_stats.ventas SELECT number FROM numbers(1234)").await;
    run(&mut s, "CREATE TABLE dbine_stats.bitacora (id UInt64) ENGINE = Log").await;
    run(&mut s, "CREATE VIEW dbine_stats.v AS SELECT id FROM dbine_stats.ventas COMMENT 'ventas por id'").await;
    run(
        &mut s,
        "CREATE MATERIALIZED VIEW dbine_stats.mv ENGINE = MergeTree ORDER BY id AS SELECT id FROM dbine_stats.ventas COMMENT 'copia'",
    )
    .await;

    let mut db = d.connect(&cfg, Some("dbine_stats")).await.unwrap();
    let rows = db.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let ventas = rows.iter().find(|r| r.object.name == "ventas").expect("ventas");
    assert_eq!((ventas.object.kind.as_str(), ventas.rows), (kinds::TABLE, 1234));
    // Log keeps its count too (0 here); views have none.
    assert!(rows.iter().all(|r| r.object.name != "v" && r.object.name != "mv"), "{rows:?}");

    let comments = db.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    assert!(comments.iter().any(|c| c.object.kind == kinds::VIEW && c.object.name == "v" && c.comment == "ventas por id"));
    assert!(comments.iter().any(|c| c.object.kind == kinds::MATERIALIZED_VIEW && c.object.name == "mv" && c.comment == "copia"));
    drop(db);
    s.drop_database("dbine_stats").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timeplus_stats() {
    let Some(cfg) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        eprintln!("DBINE_TEST_TIMEPLUS_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "timeplus").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP STREAM IF EXISTS dbine_stats_s").await;
    run(&mut s, "CREATE STREAM dbine_stats_s (a int64)").await;
    run(&mut s, "INSERT INTO dbine_stats_s (a) VALUES (1), (2), (3)").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    assert!(rows.iter().any(|r| r.object.kind == kinds::STREAM && r.object.name == "dbine_stats_s" && r.rows == 3), "{rows:?}");
    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    run(&mut s, "DROP STREAM dbine_stats_s").await;
}

//! Schema sync against real servers (same containers as `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test sync -- --ignored
//! ```

use dbine_driver::{ColumnDef, ConnectionConfig, Driver, IndexDef, QueryOutcome, TableChange};
use std::sync::Arc;

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

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(id: &str, env: &str, db: &str, setup: &str, what: &str) {
    let Some(c) = cfg(env, id) else { return };
    let d = driver(id);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(setup, 100, &mut out).await.unwrap();
    let mut s = d.connect(&c, Some(db)).await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "dbine_t").cloned().unwrap();

    let mut new = old.clone();
    let i = new.columns.iter().position(|c| c.name == "v").unwrap();
    new.columns[i].data_type = if id == "clickhouse" { "Int64".into() } else { "int64".into() };
    let n = new.columns.iter().position(|c| c.name == "name").unwrap();
    new.columns[n].nullable = false;
    new.columns.retain(|c| c.name != "gone");
    new.columns.push(ColumnDef { name: "email".into(), data_type: if id == "clickhouse" { "String" } else { "string" }.into(), nullable: true, ..Default::default() });
    new.indexes = vec![IndexDef { name: "ix_email".into(), columns: vec!["email".into()], kind: Some("bloom_filter(0.01)".into()), ..Default::default() }];
    new.comment = Some("sincronizada".into());

    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    let script_warnings = script.warnings.len();
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 100, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "dbine_t").unwrap();
    let col = |n: &str| after.columns.iter().find(|c| c.name == n).cloned();
    assert!(col("email").unwrap().nullable);
    if id == "clickhouse" {
        assert!(col("gone").is_none(), "{what}: {after:?}");
        assert_eq!(col("v").unwrap().data_type.to_lowercase(), "int64");
        assert!(!col("name").unwrap().nullable);
    } else {
        // Streams only add columns; the rest are warnings.
        assert!(col("gone").is_some() && script_warnings >= 3, "{what}: {after:?}");
    }
    assert_eq!(after.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["ix_email"], "{what}");
    if id == "clickhouse" {
        assert_eq!(after.comment.as_deref(), Some("sincronizada"));
    }

    // Create and drop.
    let mut other = after.clone();
    other.name = "dbine_t2".into();
    other.indexes.clear();
    let script = d.sync_script(&[TableChange::Create { table: other.clone() }]).unwrap();
    for st in &script.statements {
        s.execute(st, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let script = d.sync_script(&[TableChange::Drop { table: other }]).unwrap();
    for st in &script.statements {
        s.execute(st, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    assert!(!s.database_schema().await.unwrap().iter().any(|t| t.name == "dbine_t2"));
}

#[tokio::test]
#[ignore]
async fn clickhouse_sync() {
    run(
        "clickhouse",
        "DBINE_TEST_CLICKHOUSE_URL",
        "dbine_sync",
        "DROP DATABASE IF EXISTS dbine_sync; CREATE DATABASE dbine_sync;
         CREATE TABLE dbine_sync.dbine_t (id UInt64, v Int32, name Nullable(String), gone Date, INDEX ix_gone gone TYPE minmax GRANULARITY 1) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_sync.dbine_t VALUES (1, 5, 'a', '2024-01-01'), (2, 6, NULL, '2024-01-02');",
        "clickhouse",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn timeplus_sync() {
    run(
        "timeplus",
        "DBINE_TEST_TIMEPLUS_URL",
        "default",
        "DROP STREAM IF EXISTS default.dbine_t2; DROP STREAM IF EXISTS default.dbine_t;
         CREATE STREAM default.dbine_t (id uint64, v int32, name nullable(string), gone date, INDEX ix_gone gone TYPE minmax GRANULARITY 1);",
        "timeplus",
    )
    .await;
}

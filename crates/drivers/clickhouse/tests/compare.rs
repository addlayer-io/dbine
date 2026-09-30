//! "Comparar esquemas" against a real server: two databases whose tables
//! differ in each thing the compare reads (data-skipping indexes of every
//! family, including the full-text `text` index, projections, CHECK and
//! ASSUME constraints). The sync script runs on one side and both must
//! then read the same.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

/// Tables by name, without the database (each side has its own).
async fn read(s: &mut Box<dyn Session>) -> BTreeMap<String, TableSchema> {
    s.database_schema()
        .await
        .unwrap()
        .into_iter()
        .map(|mut t| {
            t.schema = None;
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
            (t.name.clone(), t)
        })
        .collect()
}

const A: &str = "
CREATE TABLE eventos (
    id UInt64, x String, d Date, s String,
    INDEX ix x TYPE bloom_filter(0.01) GRANULARITY 3,
    INDEX ix_ng lower(x) TYPE ngrambf_v1(3, 256, 2, 0) GRANULARITY 1,
    INDEX ix_tok s TYPE tokenbf_v1(512, 3, 0) GRANULARITY 2,
    INDEX ix_d d TYPE minmax GRANULARITY 1,
    INDEX ix_set (id, d) TYPE set(100) GRANULARITY 4,
    INDEX ix_txt s TYPE text(tokenizer = 'splitByNonAlpha'),
    PROJECTION p_x (SELECT x, count() GROUP BY x),
    CONSTRAINT c_id CHECK id > 0 AND x != 'a,b',
    CONSTRAINT c_d ASSUME d > '2000-01-01'
) ENGINE = MergeTree ORDER BY id;
";

const B: &str = "
CREATE TABLE eventos (
    id UInt64, x String, d Date, s String,
    INDEX ix x TYPE bloom_filter(0.05) GRANULARITY 3,
    INDEX ix_d d TYPE set(10) GRANULARITY 2,
    INDEX ix_viejo s TYPE minmax GRANULARITY 1,
    PROJECTION p_x (SELECT * ORDER BY d),
    CONSTRAINT c_id CHECK id > 5,
    CONSTRAINT c_sobra CHECK id < 100
) ENGINE = MergeTree ORDER BY id;
INSERT INTO eventos VALUES (10, 'z', '2020-01-01', 'hola mundo');
";

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut admin = d.connect(&c, None).await.expect("connect");
    let (da, db) = ("dbine_cmp_a", "dbine_cmp_b");
    for n in [da, db] {
        let _ = admin.drop_database(n).await;
        admin.create_database(n).await.expect("create database");
    }
    let mut a = d.connect(&c, Some(da)).await.unwrap();
    let mut b = d.connect(&c, Some(db)).await.unwrap();
    run(&mut a, A).await;
    run(&mut b, B).await;

    let ta = read(&mut a).await;
    let tb = read(&mut b).await;
    let t = &ta["eventos"];
    let ix = |n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    assert_eq!(ix("p_x").kind.as_deref(), Some("PROJECTION"));
    assert_eq!(ix("p_x").columns, ["(SELECT x, count() GROUP BY x)"]);
    assert_eq!(ix("ix_ng").columns, ["lower(x)"]);
    assert_eq!(ix("ix_set").columns, ["id", "d"]);
    assert!(ix("ix_txt").kind.as_deref().is_some_and(|k| k.starts_with("text(")), "{:?}", ix("ix_txt"));
    assert_eq!(t.checks.len(), 1, "{:?}", t.checks);
    assert_eq!(t.checks[0].name.as_deref(), Some("c_id"));
    assert_eq!(t.options.get("assume:c_d").map(String::as_str), Some("d > '2000-01-01'"));

    let tables: Vec<TableChange> = ta
        .iter()
        .filter_map(|(n, t)| match tb.get(n) {
            None => Some(TableChange::Create { table: TableSchema { schema: Some(db.into()), ..t.clone() } }),
            Some(o) if o != t => Some(TableChange::Alter {
                old: TableSchema { schema: Some(db.into()), ..o.clone() },
                new: TableSchema { schema: Some(db.into()), ..t.clone() },
            }),
            Some(_) => None,
        })
        .collect();
    assert_eq!(tables.len(), 1);
    let script = d.sync_script(&tables).unwrap();
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    for s in &script.statements {
        run(&mut b, s).await;
    }
    let tb2 = read(&mut b).await;
    assert_eq!(tb2, ta);
    let mut out = QueryOutcome::default();
    b.execute("SELECT x FROM eventos", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!("z")]]);

    drop((a, b));
    for n in [da, db] {
        admin.drop_database(n).await.expect("drop database");
    }
}

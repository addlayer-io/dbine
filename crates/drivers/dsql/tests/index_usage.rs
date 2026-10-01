//! The indexes of a table (no usage counters on DSQL) and dropping one with
//! the schema sync script, through the password test hook against a plain
//! PostgreSQL (see tests/integration.rs):
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test index_usage -- --ignored

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, TableChange};

#[tokio::test]
#[ignore]
async fn lists_indexes_and_drops_one() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    assert!(d.supports_index_usage());
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS dq_iu;
         CREATE TABLE dq_iu (id int CONSTRAINT dq_iu_pk PRIMARY KEY, a int, b int, c int);
         CREATE INDEX dq_iu_a ON dq_iu (a) INCLUDE (c);
         CREATE INDEX dq_iu_b ON dq_iu (b DESC) WHERE b > 0;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let table = ObjectRef { kind: "table".into(), schema: Some("public".into()), name: "dq_iu".into() };
    let r = s.index_usage(&table).await.unwrap().unwrap().derived();
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["dq_iu_pk", "dq_iu_a", "dq_iu_b"]);
    assert!(r.indexes[0].primary_key && r.indexes[0].key_columns == ["id"]);
    assert_eq!((r.indexes[1].key_columns.clone(), r.indexes[1].included_columns.clone()), (vec!["a".to_string()], vec!["c".to_string()]));
    assert_eq!(r.indexes[2].key_columns, ["b DESC"]);
    assert!(r.indexes[2].filter.as_deref().is_some_and(|f| f.contains("b > 0")));
    assert!(!r.stats_available && r.note.is_some() && r.foreign_keys.is_empty());

    // Drop dq_iu_b through the schema sync script.
    let tables = s.database_schema().await.unwrap();
    let old = tables.iter().find(|t| t.name == "dq_iu").unwrap().clone();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "dq_iu_b");
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    assert!(script.statements.iter().any(|st| st.contains("DROP INDEX") && st.contains("dq_iu_b")), "{:?}", script.statements);
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 10, &mut out).await.unwrap();
        assert!(out.error.is_none(), "{st}: {:?}", out.error);
    }
    let r = s.index_usage(&table).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["dq_iu_pk", "dq_iu_a"]);
    s.execute("DROP TABLE dq_iu", 10, &mut QueryOutcome::default()).await.unwrap();
}

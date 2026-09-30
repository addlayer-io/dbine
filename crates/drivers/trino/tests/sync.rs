//! Schema sync against a real Trino (memory catalog) or Presto:
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 DBINE_TEST_PRESTO_URL=http://localhost:25181 \
//!   cargo test -p dbine-driver-trino --test sync -- --ignored --nocapture
//! ```

use dbine_driver::{ColumnDef, ConnectionConfig, QueryOutcome, TableChange};

async fn run(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: id.into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    // Presto's memory catalog has no NOT NULL columns.
    let not_null = if id == "presto" { "" } else { " NOT NULL" };
    s.execute(
        &format!(
            "CREATE SCHEMA IF NOT EXISTS memory.dbine_sync; DROP TABLE IF EXISTS memory.dbine_sync.t; DROP TABLE IF EXISTS memory.dbine_sync.t2;
             CREATE TABLE memory.dbine_sync.t (id integer{not_null}, v integer, gone date);
             INSERT INTO memory.dbine_sync.t VALUES (1, 2, DATE '2024-01-01')"
        ),
        100,
        &mut out,
    )
    .await
    .unwrap();
    let old = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "t" && t.schema.as_deref() == Some("dbine_sync")).unwrap();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "gone");
    new.columns.push(ColumnDef { name: "email".into(), data_type: "varchar".into(), nullable: true, ..Default::default() });
    new.columns[1].data_type = "bigint".into();
    new.columns[0].nullable = true;
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    println!("{id}: {:#?}", script);
    for st in &script.statements {
        let r = s.execute(st, 100, &mut QueryOutcome::default()).await;
        println!("{id}: {st} -> {:?}", r.as_ref().err());
        // The memory connector refuses some changes; the syntax has to be right.
        if let Err(e) = r {
            assert!(e.to_string().contains("does not support"), "{st}: {e}");
        }
    }
    let after = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "t" && t.schema.as_deref() == Some("dbine_sync")).unwrap();
    println!("{id}: {:?}", after.columns.iter().map(|c| (&c.name, &c.data_type, c.nullable)).collect::<Vec<_>>());
    // Trino's memory connector adds columns and drops NOT NULL; it can't drop
    // columns or change their type. Presto's can't alter columns at all.
    if id == "trino" {
        assert!(after.columns.iter().any(|c| c.name == "email"));
        assert!(after.columns.iter().find(|c| c.name == "id").unwrap().nullable);
    }

    let mut other = after.clone();
    other.name = "t2".into();
    for ch in [TableChange::Create { table: other.clone() }, TableChange::Drop { table: other }] {
        for st in d.sync_script(&[ch]).unwrap().statements {
            s.execute(&st, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{st}: {e}"));
        }
    }
}

#[tokio::test]
#[ignore]
async fn trino_sync() {
    run("trino", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test]
#[ignore]
async fn presto_sync() {
    run("presto", "DBINE_TEST_PRESTO_URL").await;
}

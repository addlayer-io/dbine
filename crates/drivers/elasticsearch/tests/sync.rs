//! Schema sync against real servers (same containers as `integration.rs`):
//! `DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521
//!  cargo test -p dbine-driver-elasticsearch --test sync -- --ignored --nocapture`

use dbine_driver::{ColumnDef, ConnectionConfig, QueryOutcome, TableChange};

const SEED: &str = r#"
DELETE /dbine_sync?ignore_unavailable=true

DELETE /dbine_sync2?ignore_unavailable=true

PUT /dbine_sync
{"settings": {"number_of_shards": 1, "number_of_replicas": 1}, "mappings": {"properties": {"nombre": {"type": "text"}, "edad": {"type": "integer"}, "dir": {"type": "nested", "properties": {"calle": {"type": "keyword"}}}}}, "aliases": {"dbine_a1": {}}}
"#;

async fn run(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else { return };
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    s.execute(SEED, 100, &mut QueryOutcome::default()).await.unwrap();
    let old = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "dbine_sync").unwrap();
    let mut new = old.clone();
    new.columns.iter_mut().find(|c| c.name == "edad").unwrap().data_type = "long".into();
    new.columns.iter_mut().find(|c| c.name == "nombre").unwrap().comment = Some("Nombre".into());
    new.columns.push(ColumnDef { name: "email".into(), data_type: "keyword".into(), ..Default::default() });
    new.columns.push(ColumnDef { name: "dir.cp".into(), data_type: "keyword".into(), ..Default::default() });
    new.options.insert("number_of_replicas".into(), "0".into());
    new.options.insert("aliases".into(), "dbine_a2".into());
    new.comment = Some("Clientes".into());
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    println!("{id}: {script:#?}");
    for st in &script.statements {
        s.execute(st, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "dbine_sync").unwrap();
    let col = |n: &str| after.columns.iter().find(|c| c.name == n).cloned().unwrap_or_else(|| panic!("{n}: {after:?}"));
    assert_eq!(col("email").data_type, "keyword");
    assert_eq!(col("dir").data_type, "nested");
    assert_eq!(col("dir.cp").data_type, "keyword");
    assert_eq!(col("nombre").comment.as_deref(), Some("Nombre"));
    assert_eq!(col("edad").data_type, "integer");
    assert_eq!(after.comment.as_deref(), Some("Clientes"));
    assert_eq!(after.options.get("number_of_replicas").map(String::as_str), Some("0"));
    assert_eq!(after.options.get("aliases").map(String::as_str), Some("dbine_a2"));

    let mut other = after.clone();
    other.name = "dbine_sync2".into();
    other.options.remove("aliases");
    for ch in [TableChange::Create { table: other.clone() }, TableChange::Drop { table: other }] {
        for st in d.sync_script(&[ch]).unwrap().statements {
            s.execute(&st, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{st}: {e}"));
        }
    }
    s.execute("DELETE /dbine_sync", 100, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_sync() {
    run("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opensearch_sync() {
    run("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

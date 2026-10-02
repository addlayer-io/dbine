//! Index usage against DynamoDB Local: a table with a composite key, a GSI
//! with INCLUDE and an LSI, listed without counters (DynamoDB counts none
//! per index). Then "Eliminar índice…": the schema sync script without the
//! GSI, run.
//!
//! ```sh
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300 \
//!   cargo test -p dbine-driver-dynamodb --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ColumnDef, ConnectionConfig, DdlParts, IndexDef, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "dynamodb".into(), ..Default::default() };
    for (k, v) in [("region", "us-east-1"), ("auth_mode", "keys"), ("access_key_id", "dummy"), ("secret_access_key", "dummy"), ("endpoint_url", url.as_str())] {
        c.options.insert(k.into(), v.into());
    }
    Some(c)
}

fn col(name: &str, ty: &str, key: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), options: BTreeMap::from([("key_type".to_string(), key.to_string())]), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut t = TableSchema {
        name: "IxuPedidos".into(),
        columns: vec![col("cliente", "S", "HASH"), col("fecha", "N", "RANGE"), col("estado", "S", "none"), col("total", "N", "none")],
        indexes: vec![
            IndexDef { name: "byEstado".into(), columns: vec!["estado".into(), "fecha".into()], kind: Some("GSI".into()), include: vec!["total".into()], ..Default::default() },
            IndexDef { name: "byTotal".into(), columns: vec!["cliente".into(), "total".into()], kind: Some("LSI".into()), ..Default::default() },
        ],
        ..Default::default()
    };
    t.options.insert("billing_mode".into(), "PAY_PER_REQUEST".into());
    run(&mut s, &d.table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap()).await;
    let obj = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "IxuPedidos".into() };
    let r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["PRIMARY", "byEstado", "byTotal"]);
    assert!(r.indexes[0].primary_key);
    assert_eq!(r.indexes[0].key_columns, ["cliente", "fecha"]);
    assert_eq!(r.indexes[1].included_columns, ["total"]);
    assert!(r.indexes[1].kind.starts_with("GSI") && r.indexes[2].kind.starts_with("LSI"));

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "IxuPedidos").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "byEstado");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "byEstado"));
    run(&mut s, &d.table_ddl(&t, DdlParts { drop: true, ..Default::default() }).unwrap()).await;
}

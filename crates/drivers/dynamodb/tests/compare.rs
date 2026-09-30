//! "Comparar esquemas" against DynamoDB Local: index projections (INCLUDE
//! with its attributes, KEYS_ONLY, ALL) read back as they were created, and
//! a GSI whose projection differs is dropped and made again by the sync.
//!
//! ```sh
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300 \
//!   cargo test -p dbine-driver-dynamodb --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, IndexDef, QueryOutcome, Session, TableChange, TableSchema};
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

async fn read(s: &mut Box<dyn Session>) -> TableSchema {
    let mut t = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "CmpOrders").expect("CmpOrders");
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
#[ignore]
async fn projections_compare_and_sync() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let mut t = TableSchema {
        name: "CmpOrders".into(),
        columns: vec![col("customer", "S", "HASH"), col("date", "N", "RANGE"), col("status", "S", "none"), col("total", "N", "none")],
        indexes: vec![
            IndexDef { name: "byStatus".into(), columns: vec!["status".into(), "date".into()], kind: Some("GSI".into()), include: vec!["total".into()], ..Default::default() },
            IndexDef { name: "byTotal".into(), columns: vec!["total".into()], kind: Some("LSI".into()), ..Default::default() },
        ],
        ..Default::default()
    };
    t.indexes[1].options.insert("ProjectionType".into(), "KEYS_ONLY".into());
    t.options.insert("billing_mode".into(), "PAY_PER_REQUEST".into());
    run(&mut s, &d.table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap()).await;
    let src = read(&mut s).await;
    println!("{:#?}", src.indexes);
    let by = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap();
    assert_eq!(by(&src, "byStatus").include, vec!["total"]);
    assert_eq!(by(&src, "byTotal").options.get("ProjectionType").map(String::as_str), Some("KEYS_ONLY"));

    // The GSI goes to ALL, and back.
    let mut all = src.clone();
    all.indexes.iter_mut().find(|i| i.name == "byStatus").unwrap().include.clear();
    let script = d.sync_script(&[TableChange::Alter { old: src.clone(), new: all.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await;
    }
    assert!(read(&mut s).await.indexes.iter().any(|i| i.name == "byStatus" && i.include.is_empty()));
    let back = d.sync_script(&[TableChange::Alter { old: all, new: src.clone() }]).unwrap();
    for st in &back.statements {
        run(&mut s, st).await;
    }
    assert_eq!(read(&mut s).await.indexes, src.indexes);
    run(&mut s, &d.table_ddl(&t, DdlParts { drop: true, ..Default::default() }).unwrap()).await;
}

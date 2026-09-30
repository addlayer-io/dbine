//! Schema sync against a real IoTDB:
//! `DBINE_TEST_IOTDB_URL=http://localhost:25405 cargo test -p dbine-driver-iotdb --test sync -- --ignored --nocapture`

use dbine_driver::{ColumnDef, ConnectionConfig, QueryOutcome, TableChange};

#[tokio::test]
#[ignore]
async fn iotdb_sync() {
    let Ok(url) = std::env::var("DBINE_TEST_IOTDB_URL") else { return };
    let cfg = ConnectionConfig { driver: "iotdb".into(), host: url, username: Some("root".into()), password: Some("root".into()), ..Default::default() };
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.execute("DELETE DATABASE root.dbine_sync", 100, &mut QueryOutcome::default()).await;
    s.execute(
        "CREATE DATABASE root.dbine_sync;
         CREATE TIMESERIES root.dbine_sync.d1.temp WITH DATATYPE=FLOAT;
         CREATE TIMESERIES root.dbine_sync.d1.gone WITH DATATYPE=BOOLEAN;
         CREATE ALIGNED TIMESERIES root.dbine_sync.d2(temp FLOAT, gone BOOLEAN);",
        100,
        &mut QueryOutcome::default(),
    )
    .await
    .unwrap();
    let mut s = d.connect(&cfg, Some("root.dbine_sync")).await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let mut changes = Vec::new();
    for name in ["d1", "d2"] {
        let old = schema.iter().find(|t| t.name == name).cloned().unwrap_or_else(|| panic!("{name}: {schema:?}"));
        let mut new = old.clone();
        new.columns.retain(|c| c.name != "gone");
        new.columns.push(ColumnDef { name: "hum".into(), data_type: "INT32".into(), ..Default::default() });
        changes.push(TableChange::Alter { old, new });
    }
    let script = d.sync_script(&changes).unwrap();
    println!("{script:#?}");
    for q in &script.statements {
        s.execute(q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let after = s.database_schema().await.unwrap();
    for name in ["d1", "d2"] {
        let t = after.iter().find(|t| t.name == name).unwrap();
        let cols: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(cols.contains(&"hum") && cols.contains(&"temp") && !cols.contains(&"gone"), "{name}: {cols:?}");
        assert_eq!(t.options.get("aligned").is_some(), name == "d2");
    }
    let mut other = after.iter().find(|t| t.name == "d1").cloned().unwrap();
    other.name = "d3".into();
    for ch in [TableChange::Create { table: other.clone() }, TableChange::Drop { table: other }] {
        for q in d.sync_script(&[ch]).unwrap().statements {
            s.execute(&q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }
    s.execute("DELETE DATABASE root.dbine_sync", 100, &mut QueryOutcome::default()).await.unwrap();
}

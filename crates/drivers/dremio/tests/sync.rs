//! Schema sync against a real Dremio OSS ($scratch holds Iceberg tables);
//! run `integration.rs` once first so the user exists:
//! `DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test sync -- --ignored --nocapture`

use dbine_driver::{kinds, ColumnDef, ConnectionConfig, ObjectRef, QueryOutcome, TableChange, TableSchema};

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
}

#[tokio::test]
#[ignore]
async fn dremio_sync() {
    let Ok(url) = std::env::var("DBINE_TEST_DREMIO_URL") else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        password: Some("secreto123".into()),
        ..Default::default()
    };
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_sync; DROP TABLE IF EXISTS \"$scratch\".dbine_sync2;
         CREATE TABLE \"$scratch\".dbine_sync (id INT, v FLOAT, gone DATE);
         INSERT INTO \"$scratch\".dbine_sync VALUES (1, 1.5, DATE '2024-01-01')",
        100,
        &mut QueryOutcome::default(),
    )
    .await
    .unwrap();
    let old = TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some("$scratch".into()),
        name: "dbine_sync".into(),
        columns: vec![col("id", "INT"), col("v", "FLOAT"), col("gone", "DATE")],
        ..Default::default()
    };
    let new = TableSchema { columns: vec![col("id", "BIGINT"), col("v", "DOUBLE"), col("nota", "VARCHAR"), col("n", "INT")], ..old.clone() };
    let script = d.sync_script(&[TableChange::Alter { old, new: new.clone() }]).unwrap();
    println!("{script:#?}");
    for q in &script.statements {
        s.execute(q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let cols = s.columns(&ObjectRef { kind: kinds::TABLE.into(), schema: Some("$scratch".into()), name: "dbine_sync".into() }).await.unwrap();
    let got: Vec<(String, String)> = cols.iter().map(|c| (c.name.clone(), c.data_type.to_uppercase())).collect();
    println!("{got:?}");
    assert!(!got.iter().any(|(n, _)| n == "gone"), "{got:?}");
    assert!(got.iter().any(|(n, t)| n == "id" && t.contains("BIGINT")), "{got:?}");
    assert!(got.iter().any(|(n, t)| n == "v" && t.contains("DOUBLE")), "{got:?}");
    assert!(got.iter().any(|(n, _)| n == "nota") && got.iter().any(|(n, _)| n == "n"), "{got:?}");

    let other = TableSchema { name: "dbine_sync2".into(), ..new };
    for ch in [TableChange::Create { table: other.clone() }, TableChange::Drop { table: other }] {
        for q in d.sync_script(&[ch]).unwrap().statements {
            s.execute(&q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }
    s.execute("DROP TABLE \"$scratch\".dbine_sync", 100, &mut QueryOutcome::default()).await.unwrap();
}

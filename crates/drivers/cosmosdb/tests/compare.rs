//! "Comparar esquemas" against the emulator: a container made from a
//! schema with unique keys, a composite and a spatial index reads back the
//! same (what the compare compares), and a differing one is reported by the
//! sync as a warning (Cosmos DB can't change a container from a script).
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, DdlParts, IndexDef, QueryOutcome, TableChange, TableSchema};

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

#[tokio::test]
#[ignore]
async fn indexes_round_trip() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else { return };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url, trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database("dbine_cmp").await;
    admin.create_database("dbine_cmp").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_cmp")).await.unwrap();

    let mut t = TableSchema { kind: "collection".into(), name: "docs".into(), ..Default::default() };
    t.options.insert("partition_key".into(), "/cat".into());
    t.indexes = vec![
        IndexDef { name: "unique_1".into(), columns: vec!["email".into()], unique: true, ..Default::default() },
        IndexDef { name: "composite_1".into(), columns: vec!["a".into(), "b DESC".into()], kind: Some("composite".into()), ..Default::default() },
        IndexDef {
            name: "spatial_1".into(),
            columns: vec!["loc/*".into()],
            kind: Some("SPATIAL".into()),
            options: [("types".to_string(), "[\"Point\"]".to_string())].into(),
            ..Default::default()
        },
    ];
    let ddl = d.table_ddl(&t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ddl, 10, &mut out).await.unwrap_or_else(|e| panic!("{ddl}: {e}"));
    let back = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "docs").unwrap();
    println!("{:#?}", back.indexes);
    let mut got = back.indexes.clone();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    let mut want = t.indexes.clone();
    want.sort_by(|a, b| a.name.cmp(&b.name));
    // The Linux (vNext) emulator keeps no custom indexing policy: there only
    // the unique keys come back.
    if !got.iter().any(|i| i.kind.as_deref() == Some("composite")) {
        println!("el emulador no guarda la política de indexación: solo se comparan las claves únicas");
        want.retain(|i| i.unique);
    }
    assert_eq!(got, want);

    let mut other = back.clone();
    other.indexes.retain(|i| i.kind.as_deref() != Some("SPATIAL"));
    let mut new = other.clone();
    new.indexes.push(t.indexes[2].clone());
    let script = d.sync_script(&[TableChange::Alter { old: other, new }]).unwrap();
    assert!(script.statements.is_empty() && script.warnings.iter().any(|w| w.contains("indexación")), "{script:?}");
    admin.drop_database("dbine_cmp").await.unwrap();
}

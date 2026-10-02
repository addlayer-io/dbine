//! Index usage against a real server: a collection with two indexes, five
//! lookups through one of them, none through the other. MongoDB counts no
//! writes per index: no writes, nothing "sin uso", a note saying why. Then
//! "Eliminar índice…": the schema sync script without the unread index, run.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let mut c = ConnectionConfig { driver: driver.into(), database: "dbine_ix_usage".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg("DBINE_TEST_MONGODB_URL", "mongodb") else { return };
    check(c, true).await;
}

/// FerretDB: `DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/`.
#[tokio::test]
#[ignore]
async fn index_usage_ferretdb_live() {
    let Some(c) = cfg("DBINE_TEST_FERRETDB_URL", "ferretdb") else { return };
    check(c, false).await;
}

/// `counters`: the server has `$indexStats` (FerretDB may not: then the
/// indexes are listed with a note).
async fn check(c: ConnectionConfig, counters: bool) {
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == c.driver).unwrap();
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "db.dropDatabase()").await;
    run(
        &mut s,
        r#"db.createCollection("pedidos")
db.pedidos.insertMany([{ _id: 1, cliente: "a", fecha: 1 }, { _id: 2, cliente: "b", fecha: 2 }, { _id: 3, cliente: "c", fecha: 3 }])
db.pedidos.createIndex({ cliente: 1 }, { name: "ix_cliente" })
db.pedidos.createIndex({ fecha: -1 }, { name: "ix_fecha", partialFilterExpression: { fecha: { $gt: 0 } } })"#,
    )
    .await;
    for c in ["a", "b", "c", "a", "b"] {
        run(&mut s, &format!(r#"db.pedidos.find({{ cliente: "{c}" }}).hint("ix_cliente")"#)).await;
    }
    let obj = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "pedidos".into() };
    let r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n}"));
    assert!(get("_id_").primary_key);
    assert!(r.foreign_keys.is_empty() && !r.seek_scan_split);
    if !counters && !r.stats_available {
        assert!(r.note.is_some());
        assert_eq!(get("ix_fecha").key_columns, ["fecha DESC"]);
    } else {
    assert!(r.stats_available);
    assert!(r.since.is_some());
    assert_eq!(get("ix_cliente").seeks, 5);
    assert_eq!(get("ix_cliente").key_columns, ["cliente"]);
    assert!(!get("ix_cliente").unused);
    assert_eq!(get("ix_fecha").seeks, 0);
    assert_eq!(get("ix_fecha").key_columns, ["fecha DESC"]);
    assert!(get("ix_fecha").filter.is_some());
    // No per-index write counter: writes unknown (a dash), never "sin uso".
    assert!(!r.writes_counted);
    assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused && i.writes_per_read.is_none()));
    assert!(r.note.as_deref().is_some_and(|n| n.contains("no cuenta escrituras por índice")), "{:?}", r.note);
    assert!(get("ix_cliente").size_kb.is_some());
    assert_eq!(get("ix_cliente").seek_health, None);
    }

    // Drop the unused index the way the UI does: the sync script of the
    // collection without it.
    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "pedidos").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "ix_fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "ix_fecha"));
    assert!(r.indexes.iter().any(|i| i.name == "ix_cliente"));
    run(&mut s, "db.dropDatabase()").await;
}

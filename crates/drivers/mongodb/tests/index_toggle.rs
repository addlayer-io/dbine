//! Disable / enable an index against a real server: a collection with an
//! index, hidden through `index_toggle_script` run with `Session::execute`,
//! reported `disabled` by `index_usage`, the collection still readable, then
//! unhidden. `_id_` is refused. FerretDB has no hidden indexes: the driver
//! doesn't offer the toggle.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/ \
//!   cargo test -p dbine-driver-mongodb --test index_toggle -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, Error, IndexUsage, ObjectRef, QueryOutcome, Session};

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let mut c = ConnectionConfig { driver: driver.into(), database: "dbine_ix_toggle".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
    out
}

async fn index(s: &mut Box<dyn Session>, obj: &ObjectRef, name: &str) -> IndexUsage {
    let r = s.index_usage(obj).await.unwrap().expect("report");
    r.indexes.into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name}"))
}

#[tokio::test]
#[ignore]
async fn index_toggle_live() {
    let Some(c) = cfg("DBINE_TEST_MONGODB_URL", "mongodb") else { return };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == c.driver).unwrap();
    assert!(d.supports_index_toggle());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "db.dropDatabase()").await;
    run(
        &mut s,
        r#"db.createCollection("pedidos")
db.pedidos.insertMany([{ _id: 1, cliente: "a" }, { _id: 2, cliente: "b" }, { _id: 3, cliente: "a" }])
db.pedidos.createIndex({ cliente: 1 }, { name: "ix_cliente" })"#,
    )
    .await;
    let obj = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "pedidos".into() };
    let ix = index(&mut s, &obj, "ix_cliente").await;
    assert!(!ix.disabled);

    let off = d.index_toggle_script(&obj, &ix, false).unwrap();
    println!("disable: {:?} {:?}", off.statements, off.warnings);
    assert_eq!(off.statements, [r#"db.getCollection("pedidos").hideIndex("ix_cliente")"#]);
    assert_eq!(off.warnings.len(), 1);
    for st in &off.statements {
        run(&mut s, st).await;
    }
    let ix = index(&mut s, &obj, "ix_cliente").await;
    assert!(ix.disabled, "{ix:?}");
    assert!(ix.kind.contains("HIDDEN"), "{}", ix.kind);

    // Still readable, the planner just doesn't pick the index.
    let out = run(&mut s, r#"db.pedidos.find({ cliente: "a" })"#).await;
    let rows: usize = out.results.iter().map(|r| r.rows.len()).sum();
    assert_eq!(rows, 2, "{out:?}");

    let on = d.index_toggle_script(&obj, &ix, true).unwrap();
    println!("enable: {:?} {:?}", on.statements, on.warnings);
    assert_eq!(on.statements, [r#"db.getCollection("pedidos").unhideIndex("ix_cliente")"#]);
    for st in &on.statements {
        run(&mut s, st).await;
    }
    assert!(!index(&mut s, &obj, "ix_cliente").await.disabled);

    let id = index(&mut s, &obj, "_id_").await;
    assert!(matches!(d.index_toggle_script(&obj, &id, false), Err(Error::Unsupported(_))));

    run(&mut s, "db.dropDatabase()").await;
}

/// FerretDB answers `collMod` with "'collMod.index.hidden' is not supported
/// yet": no toggle, and no index reads as disabled.
#[tokio::test]
#[ignore]
async fn index_toggle_ferretdb_live() {
    let Some(c) = cfg("DBINE_TEST_FERRETDB_URL", "ferretdb") else { return };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == c.driver).unwrap();
    assert!(!d.supports_index_toggle());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "db.dropDatabase()").await;
    run(
        &mut s,
        r#"db.createCollection("pedidos")
db.pedidos.insertOne({ _id: 1, cliente: "a" })
db.pedidos.createIndex({ cliente: 1 }, { name: "ix_cliente" })"#,
    )
    .await;
    let obj = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "pedidos".into() };
    let ix = index(&mut s, &obj, "ix_cliente").await;
    assert!(!ix.disabled);
    assert!(matches!(d.index_toggle_script(&obj, &ix, false), Err(Error::Unsupported(_))));
    // The server really refuses it.
    let mut out = QueryOutcome::default();
    let r = s.execute(r#"db.pedidos.hideIndex("ix_cliente")"#, 10, &mut out).await;
    let msg = r.err().map(|e| e.to_string()).or(out.error.clone());
    println!("ferretdb hideIndex: {msg:?}");
    assert!(msg.is_some_and(|m| m.contains("not supported")));
    run(&mut s, "db.dropDatabase()").await;
}

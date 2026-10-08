//! "Renombrar…" against a real server, as the app runs it: the dependents
//! found by "Ver dependencias", rewritten with `rewrite_references`
//! (pipelines), dropped before the rename and created after it.
//!
//! Fixture (MongoDB has no foreign keys, checks or routines: the validator
//! and the indexes stand for them): `clientes` (`_id`, `pepe`) with a unique
//! index on `pepe`, a partial one filtering on it and a validator requiring
//! it; `pedidos` naming its client; views `v_clientes` (`viewOn`),
//! `v_pedidos` (`$lookup.from`), `v_union` (`$unionWith.coll`) and
//! `v_sobre_v` on `v_clientes`; `otros`, another collection with a field
//! `pepe`, and a view on it.
//!
//! 1. The collection `clientes` becomes `Clientes Nuevos`: the three views
//!    on it are rewritten and still answer; the others are untouched.
//! 2. The view `v_clientes` becomes `vClientes`: `v_sobre_v` follows.
//! 3. The field `pepe` becomes `apodo`: every document, the unique and the
//!    partial index, the validator; `otros.pepe` stays.
//!
//! FerretDB: collection and field (it has no views, and ignores validators).
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/ \
//!   cargo test -p dbine-driver-mongodb --test rename -- --ignored --nocapture
//! ```

use dbine_driver::rename::{rewrite_references, RenameRequest, RenameTarget, RewriteOptions};
use dbine_driver::{kinds, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, Session};
use std::sync::Arc;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let mut c = ConnectionConfig { driver: driver.into(), database: "dbine_rename".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
    out
}

async fn count(s: &mut Box<dyn Session>, text: &str) -> usize {
    run(s, text).await.results[0].rows.len()
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

/// What the app does: dependents rewritten, dropped first and created
/// after the driver's rename. Returns the rewritten dependents' names.
async fn rename(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, target: RenameTarget, new: &str) -> Vec<String> {
    let spec = d.rename_spec().expect("spec");
    assert!(spec.allows(&target));
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, false);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let column = matches!(target, RenameTarget::Column { .. });
    let mut rewritten = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        if column {
            continue; // pipelines' fields are left to the user
        }
        let body = s.definition(&obj(&dep.kind, &dep.name)).await.unwrap().expect("definition");
        let r = rewrite_references(&body, &dialect, &target.rewrite_target(), new, &spec, &RewriteOptions::default());
        assert!(r.unresolved.is_empty(), "{}: {:?}", dep.name, r.unresolved);
        if !r.edits.is_empty() {
            println!("{} -> {}", dep.name, r.text);
            rewritten.push((dep.name.clone(), r.text));
        }
    }
    let (definition, table) = match &target {
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => (s.definition(object).await.unwrap(), None),
        RenameTarget::Column { table, .. } => (None, s.database_schema().await.unwrap().into_iter().find(|t| t.name == table.name)),
        _ => (None, None),
    };
    let req = RenameRequest { target, new_name: new.into(), table, definition };
    let middle = d.rename_script(&req).unwrap();
    println!("rename: {:?}\nwarnings: {:?}", middle.statements, middle.warnings);
    for (name, _) in &rewritten {
        run(s, &format!("db.getCollection({}).drop()", serde_json::to_string(name).unwrap())).await;
    }
    for st in &middle.statements {
        run(s, st).await;
    }
    for (_, text) in &rewritten {
        run(s, text).await;
    }
    let mut names: Vec<String> = rewritten.into_iter().map(|(n, _)| n).collect();
    names.sort();
    names
}

async fn fixture(s: &mut Box<dyn Session>, views: bool) {
    run(s, "db.dropDatabase()").await;
    run(
        s,
        r#"db.createCollection("clientes", { validator: { $jsonSchema: { required: ["pepe"], properties: { pepe: { bsonType: "string" } } } } })
db.clientes.insertMany([{ _id: 1, pepe: "a", activo: true }, { _id: 2, pepe: "b", activo: false }, { _id: 3, pepe: "c", activo: true }])
db.clientes.createIndex({ pepe: 1 }, { name: "ux_pepe", unique: true })
db.clientes.createIndex({ activo: 1 }, { name: "ix_activo", partialFilterExpression: { pepe: { $exists: true } } })
db.pedidos.insertMany([{ _id: 10, cliente: 1 }, { _id: 11, cliente: 3 }])
db.otros.insertMany([{ _id: 1, pepe: "x" }])"#,
    )
    .await;
    if views {
        run(
            s,
            r#"db.createView("v_clientes", "clientes", [{ $match: { activo: true } }])
db.createView("v_pedidos", "pedidos", [{ $lookup: { from: "clientes", localField: "cliente", foreignField: "_id", as: "c" } }])
db.createView("v_union", "otros", [{ $unionWith: { coll: "clientes", pipeline: [] } }])
db.createView("v_sobre_v", "v_clientes", [{ $project: { pepe: 1 } }])
db.createView("v_otros", "otros", [{ $match: { pepe: "x" } }])"#,
        )
        .await;
    }
}

/// `validator`: the server keeps validators (FerretDB ignores them).
async fn field(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, collection: &str, validator: bool) {
    let t = RenameTarget::Column { table: obj(kinds::COLLECTION, collection), column: "pepe".into() };
    let q = serde_json::to_string(collection).unwrap();
    rename(d, s, t, "apodo").await;
    assert_eq!(count(s, &format!("db.getCollection({q}).find({{ apodo: {{ $exists: true }} }})")).await, 3);
    assert_eq!(count(s, &format!("db.getCollection({q}).find({{ pepe: {{ $exists: true }} }})")).await, 0);
    assert_eq!(count(s, "db.otros.find({ pepe: \"x\" })").await, 1);
    let ix = run(s, &format!("db.getCollection({q}).getIndexes()")).await;
    let text = serde_json::to_string(&ix.results[0].rows).unwrap();
    println!("indexes: {text}");
    assert!(text.contains("ux_pepe") && text.contains("apodo") && !text.contains("\\\"pepe\\\""), "{text}");
    // The unique index is there again, on the new field.
    assert!(fails(s, &format!("db.getCollection({q}).insertOne({{ _id: 9, apodo: \"a\" }})")).await, "duplicate accepted");
    // The validator asks for the new field.
    assert!(!validator || fails(s, &format!("db.getCollection({q}).insertOne({{ _id: 8, pepe: \"z\" }})")).await, "validator not changed");
    run(s, &format!("db.getCollection({q}).insertOne({{ _id: 7, apodo: \"z\" }})")).await;
}

async fn fails(s: &mut Box<dyn Session>, text: &str) -> bool {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.is_err() || out.error.is_some()
}

#[tokio::test]
#[ignore]
async fn rename_mongodb_live() {
    let Some(c) = cfg("DBINE_TEST_MONGODB_URL", "mongodb") else { return };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == c.driver).unwrap();
    let mut s = d.connect(&c, None).await.expect("connect");
    fixture(&mut s, true).await;

    // 1. The collection, with the views that read it.
    let t = RenameTarget::Object { object: obj(kinds::COLLECTION, "clientes"), parent: None };
    let names = rename(&d, &mut s, t, "Clientes Nuevos").await;
    assert_eq!(names, ["v_clientes", "v_pedidos", "v_union"]);
    assert_eq!(count(&mut s, "db.v_clientes.find({})").await, 2);
    assert_eq!(count(&mut s, "db.v_pedidos.find({ \"c.0\": { $exists: true } })").await, 2);
    assert_eq!(count(&mut s, "db.v_union.find({})").await, 4);
    assert_eq!(count(&mut s, "db.v_sobre_v.find({})").await, 2);
    assert_eq!(count(&mut s, "db.v_otros.find({})").await, 1);
    assert_eq!(count(&mut s, "db.getCollection(\"Clientes Nuevos\").find({})").await, 3);

    // 2. A view: dropped and created; the view on it follows.
    let t = RenameTarget::Object { object: obj(kinds::VIEW, "v_clientes"), parent: None };
    let names = rename(&d, &mut s, t, "vClientes").await;
    assert_eq!(names, ["v_sobre_v"]);
    assert_eq!(count(&mut s, "db.vClientes.find({})").await, 2);
    assert_eq!(count(&mut s, "db.v_sobre_v.find({})").await, 2);
    let listed: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert!(!listed.contains(&"v_clientes".to_string()), "{listed:?}");

    // 3. A field, in every document.
    field(&d, &mut s, "Clientes Nuevos", true).await;
    run(&mut s, "db.dropDatabase()").await;
}

#[tokio::test]
#[ignore]
async fn rename_ferretdb_live() {
    let Some(c) = cfg("DBINE_TEST_FERRETDB_URL", "ferretdb") else { return };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == c.driver).unwrap();
    assert!(!d.rename_spec().unwrap().kinds.contains(&kinds::VIEW.to_string()));
    let mut s = d.connect(&c, None).await.expect("connect");
    fixture(&mut s, false).await;
    let t = RenameTarget::Object { object: obj(kinds::COLLECTION, "clientes"), parent: None };
    rename(&d, &mut s, t, "Clientes Nuevos").await;
    assert_eq!(count(&mut s, "db.getCollection(\"Clientes Nuevos\").find({})").await, 3);
    field(&d, &mut s, "Clientes Nuevos", false).await;
    run(&mut s, "db.dropDatabase()").await;
}

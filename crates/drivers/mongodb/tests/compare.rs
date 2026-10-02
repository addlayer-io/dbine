//! "Comparar esquemas" against a real server: two databases whose
//! collection differs in its validator (the CHECK) and in every kind of
//! index (text with weights and language, collation, TTL, sparse, hidden,
//! partial, wildcard with projection, 2dsphere, 2d with bounds, hashed),
//! plus a view only one side has. The sync script and the view's
//! definition are run on the target and both sides must then read the same.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};

fn cfg(db: &str) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_MONGODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "mongodb".into(), database: db.into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

const SOURCE: &str = r#"
db.createCollection("docs", { validator: { $jsonSchema: { bsonType: "object", required: ["titulo"], properties: { titulo: { bsonType: "string" } } } }, validationLevel: "moderate", validationAction: "warn" })
db.docs.insertOne({ titulo: "a", cuerpo: "b", email: "x@y", loc: { type: "Point", coordinates: [1, 2] }, p: [1, 2], h: 1, at: ISODate("2024-01-01T00:00:00Z") })
db.docs.createIndex({ cat: 1, titulo: "text", cuerpo: "text" }, { name: "ft", weights: { titulo: 10 }, default_language: "spanish", language_override: "idioma" })
db.docs.createIndex({ email: 1 }, { name: "email_ci", unique: true, sparse: true, collation: { locale: "es", strength: 2 } })
db.docs.createIndex({ at: 1 }, { name: "exp", expireAfterSeconds: 3600, hidden: true, partialFilterExpression: { h: { $gt: 0 } } })
db.docs.createIndex({ "$**": 1 }, { name: "wild", wildcardProjection: { cuerpo: 0 } })
db.docs.createIndex({ loc: "2dsphere" }, { name: "geo" })
db.docs.createIndex({ p: "2d" }, { name: "plano", bits: 20, min: -90, max: 90 })
db.docs.createIndex({ h: "hashed" }, { name: "hash" })
db.createView("recientes", "docs", [{ $match: { h: { $gt: 0 } } }, { $project: { titulo: 1 } }], { collation: { locale: "es" } })
"#;

const TARGET: &str = r#"
db.createCollection("docs")
db.docs.insertOne({ titulo: "a" })
db.docs.createIndex({ cuerpo: "text" }, { name: "ft" })
db.docs.createIndex({ email: 1 }, { name: "email_ci", unique: true })
db.docs.createIndex({ at: 1 }, { name: "exp", expireAfterSeconds: 60 })
db.docs.createIndex({ viejo: 1 }, { name: "viejo" })
"#;

fn docs(schema: &[TableSchema]) -> TableSchema {
    let mut t = schema.iter().find(|t| t.name == "docs").expect("docs").clone();
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
#[ignore]
async fn compare_and_sync() {
    let (Some(ca), Some(cb)) = (cfg("dbine_cmp_src"), cfg("dbine_cmp_dst")) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut a = d.connect(&ca, None).await.expect("connect");
    let mut b = d.connect(&cb, None).await.expect("connect");
    run(&mut a, "db.dropDatabase()").await;
    run(&mut b, "db.dropDatabase()").await;
    run(&mut a, SOURCE).await;
    run(&mut b, TARGET).await;

    let src = docs(&a.database_schema().await.unwrap());
    let dst = docs(&b.database_schema().await.unwrap());
    println!("{:#?}\n{:#?}", src.indexes, src.checks);
    assert_eq!(src.checks.len(), 1);
    assert!(src.checks[0].expression.contains("moderate") && src.checks[0].expression.contains("warn"));
    assert!(dst.checks.is_empty());
    let ft = src.indexes.iter().find(|i| i.name == "ft").unwrap();
    assert_eq!(ft.kind.as_deref(), Some("FULLTEXT"));
    assert_eq!(ft.options.get("default_language").map(String::as_str), Some("spanish"));
    assert!(ft.options.contains_key("weights"));
    let exp = src.indexes.iter().find(|i| i.name == "exp").unwrap();
    assert!(exp.filter.is_some() && exp.options.contains_key("hidden"));
    assert!(src.indexes.iter().find(|i| i.name == "email_ci").unwrap().options.contains_key("collation"));
    assert_ne!(src.indexes, dst.indexes);

    // The view is a code object with a runnable definition.
    let objects = a.list_objects().await.unwrap();
    let view = objects.iter().find(|o| o.kind == kinds::VIEW && o.name == "recientes").unwrap();
    let r = ObjectRef { kind: view.kind.clone(), schema: None, name: view.name.clone() };
    let def = a.definition(&r).await.unwrap().unwrap();
    assert!(def.starts_with("db.createView(\"recientes\""), "{def}");

    let script = d.sync_script(&[TableChange::Alter { old: dst.clone(), new: src.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut b, st).await;
    }
    run(&mut b, &def).await;

    let after = docs(&b.database_schema().await.unwrap());
    assert_eq!(after.indexes, src.indexes);
    assert_eq!(after.checks, src.checks);
    assert_eq!(b.definition(&r).await.unwrap().unwrap(), def);
    // Nothing left to do.
    let again = d.sync_script(&[TableChange::Alter { old: after, new: src }]).unwrap();
    assert!(again.statements.is_empty(), "{again:#?}");

    run(&mut a, "db.dropDatabase()").await;
    run(&mut b, "db.dropDatabase()").await;
}

// --- "Eliminar" in the compare ---------------------------------------------
//
// The UI drops an element by taking it out of a side's model and sending the
// difference (`changesOf` in CompareView.vue): a collection that's gone is a
// `Drop`, one that lost an index or its validator is an `Alter { old, new }`,
// a view that's gone is dropped with `drop_other` (src-tauri) before the
// collections. After the run the side is read again and must be its edited
// model.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
struct Model {
    tables: BTreeMap<String, TableSchema>,
    /// Views, by name, with their definition.
    views: BTreeMap<String, String>,
}

async fn model(s: &mut Box<dyn Session>) -> Model {
    let tables = s
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .map(|mut t| {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            (t.name.clone(), t)
        })
        .collect();
    let mut views = BTreeMap::new();
    for o in s.list_objects().await.unwrap().into_iter().filter(|o| o.kind == kinds::VIEW) {
        let r = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
        views.insert(o.name, s.definition(&r).await.unwrap().expect("definition"));
    }
    Model { tables, views }
}

/// What src-tauri's `drop_other` writes for a MongoDB view.
fn drop_view(name: &str) -> String {
    format!("db.getCollection({}).drop()", serde_json::to_string(name).unwrap())
}

/// Drops on one side what `edit` takes out of its model; returns the
/// script's warnings and the model read back.
async fn drop_on(d: &dyn dbine_driver::Driver, s: &mut Box<dyn Session>, edit: impl Fn(&mut Model)) -> (Vec<String>, Model) {
    let orig = model(s).await;
    let mut work = orig.clone();
    edit(&mut work);
    assert_ne!(work, orig, "the edit drops something");
    let mut tables = Vec::new();
    for (n, t) in &orig.tables {
        match work.tables.get(n) {
            None => tables.push(TableChange::Drop { table: t.clone() }),
            Some(w) if w != t => tables.push(TableChange::Alter { old: t.clone(), new: w.clone() }),
            Some(_) => {}
        }
    }
    let script = d.sync_script(&tables).unwrap();
    println!("{script:#?}");
    let views = orig.views.keys().filter(|v| !work.views.contains_key(*v)).map(|v| drop_view(v));
    for st in views.chain(script.statements) {
        run(s, &st).await;
    }
    // A view is listed twice (as a view-kind table and as a view object):
    // dropping either one takes both.
    let gone: Vec<String> = orig
        .tables
        .iter()
        .filter(|(n, t)| t.kind == kinds::VIEW && (!work.tables.contains_key(*n) || !work.views.contains_key(*n)))
        .map(|(n, _)| n.clone())
        .collect();
    for v in gone {
        work.tables.remove(&v);
        work.views.remove(&v);
    }
    let after = model(s).await;
    assert_eq!(after, work, "the side reads as its edited model");
    (script.warnings, after)
}

fn table<'a>(m: &'a mut Model, t: &str) -> &'a mut TableSchema {
    m.tables.get_mut(t).unwrap()
}

const BOTH: &str = r#"
db.createCollection("pedidos", { validator: { $jsonSchema: { bsonType: "object", required: ["cliente"] } }, validationLevel: "moderate" })
db.pedidos.insertOne({ cliente: "a", total: 5, extra: "x" })
db.pedidos.createIndex({ cliente: 1 }, { name: "ix_cliente" })
db.pedidos.createIndex({ total: -1 }, { name: "ix_total", sparse: true })
db.createCollection("clientes")
db.clientes.insertOne({ nombre: "a" })
db.createView("grandes", "pedidos", [{ $match: { total: { $gt: 1 } } }])
"#;

#[tokio::test]
#[ignore]
async fn drop_from_compare() {
    let (Some(ca), Some(cb)) = (cfg("dbine_drop_a"), cfg("dbine_drop_b")) else { return };
    let d = dbine_driver_mongodb::drivers().remove(0);
    let d = d.as_ref();
    let mut a = d.connect(&ca, None).await.expect("connect");
    let mut b = d.connect(&cb, None).await.expect("connect");
    for s in [&mut a, &mut b] {
        run(s, "db.dropDatabase()").await;
        run(s, BOTH).await;
    }
    let (ma, mb) = (model(&mut a).await, model(&mut b).await);
    println!("{ma:#?}");
    assert_eq!(ma, mb);
    assert!(ma.views.contains_key("grandes"));

    // An index on one side: the sides differ only there.
    let (_, ma) = drop_on(d, &mut a, |m| table(m, "pedidos").indexes.retain(|i| i.name != "ix_total")).await;
    assert!(!ma.tables["pedidos"].indexes.iter().any(|i| i.name == "ix_total"));
    let mb = model(&mut b).await;
    assert_ne!(ma, mb);
    assert_eq!(ma.tables["pedidos"].checks, mb.tables["pedidos"].checks);
    // The same index on the other side: equal again.
    let (_, mb) = drop_on(d, &mut b, |m| table(m, "pedidos").indexes.retain(|i| i.name != "ix_total")).await;
    assert_eq!(ma, mb);

    // The validator (the collection's CHECK), on both sides.
    for s in [&mut a, &mut b] {
        let (_, m) = drop_on(d, s, |m| table(m, "pedidos").checks.clear()).await;
        assert!(m.tables["pedidos"].checks.is_empty());
    }
    // An index on both sides.
    for s in [&mut a, &mut b] {
        drop_on(d, s, |m| table(m, "pedidos").indexes.retain(|i| i.name != "ix_cliente")).await;
    }
    assert_eq!(model(&mut a).await, model(&mut b).await);

    // A field: documents have no fixed schema, so it's only a warning.
    let orig = model(&mut a).await;
    let mut work = orig.clone();
    table(&mut work, "pedidos").columns.retain(|c| c.name != "extra");
    assert_ne!(work, orig, "the field is in the model");
    let script = d.sync_script(&[TableChange::Alter { old: orig.tables["pedidos"].clone(), new: work.tables["pedidos"].clone() }]).unwrap();
    assert!(script.statements.is_empty(), "{script:#?}");
    assert!(script.warnings.iter().any(|w| w.contains("pedidos.extra")), "{script:#?}");

    // A view and a whole collection, on both sides: on one through the
    // view's object row, on the other through its table row.
    for (s, by_object) in [(&mut a, true), (&mut b, false)] {
        let (warnings, m) = drop_on(d, s, |m| {
            if by_object {
                m.views.remove("grandes");
            } else {
                m.tables.remove("grandes");
            }
            m.tables.remove("clientes");
        })
        .await;
        assert!(warnings.iter().any(|w| w.contains("clientes")), "{warnings:?}");
        assert!(m.views.is_empty() && !m.tables.contains_key("clientes") && !m.tables.contains_key("grandes"));
    }
    assert_eq!(model(&mut a).await, model(&mut b).await);

    run(&mut a, "db.dropDatabase()").await;
    run(&mut b, "db.dropDatabase()").await;
}

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

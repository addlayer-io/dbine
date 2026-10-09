//! "Renombrar…" on a database against a real MongoDB, as the app runs it:
//! the objects read from the old database (`list_objects`, a view's
//! `definition`), the script from `rename_database_script`, and each
//! statement run on a session opened in `database_from` (`schema_sync_run`).
//!
//! Fixture `dbine_dbr_old`: `clientes` (3 documents, unique index
//! `ux_email`), `pedidos` (2 documents) and the view `v_activos` on
//! `clientes`. After the rename, `dbine_dbr_new` has the documents, the
//! index (still unique) and the view answering; `dbine_dbr_old` is gone.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test rename_database -- --ignored --nocapture
//! ```

use dbine_driver::rename::DatabaseObject;
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};

const OLD: &str = "dbine_dbr_old";
const NEW: &str = "dbine_dbr_new";

fn cfg(url: &str, database: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig { driver: "mongodb".into(), database: database.into(), ..Default::default() };
    c.options.insert("connection_string".into(), url.into());
    c
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

async fn databases(s: &mut Box<dyn Session>) -> Vec<String> {
    s.list_databases().await.unwrap()
}

#[tokio::test]
#[ignore]
async fn rename_database_mongodb_live() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_URL") else { return };
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == "mongodb").unwrap();
    let spec = d.rename_spec().unwrap();
    assert!(spec.databases && spec.database_moves);
    let from = spec.database_from.clone().unwrap();

    // Fixture, from scratch.
    let mut admin = d.connect(&cfg(&url, &from), None).await.expect("connect");
    for db in [OLD, NEW] {
        run(&mut admin, &format!("use {db}")).await;
        run(&mut admin, "db.dropDatabase()").await;
    }
    run(&mut admin, &format!("use {from}")).await;
    let mut s = d.connect(&cfg(&url, OLD), None).await.expect("connect");
    run(
        &mut s,
        r#"db.clientes.insertMany([{ _id: 1, email: "a@x", activo: true }, { _id: 2, email: "b@x", activo: false }, { _id: 3, email: "c@x", activo: true }])
db.clientes.createIndex({ email: 1 }, { name: "ux_email", unique: true })
db.pedidos.insertMany([{ _id: 10, cliente: 1 }, { _id: 11, cliente: 3 }])
db.createView("v_activos", "clientes", [{ $match: { activo: true } }])"#,
    )
    .await;

    // What the app reads from the old database.
    let mut objects = Vec::new();
    for o in s.list_objects().await.unwrap() {
        let definition = if o.kind == kinds::VIEW {
            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            Some(s.definition(&r).await.unwrap().expect("view definition"))
        } else {
            assert_eq!(o.kind, kinds::COLLECTION);
            None
        };
        objects.push(DatabaseObject { kind: o.kind, schema: o.schema, name: o.name, definition });
    }
    assert_eq!(objects.len(), 3, "{objects:?}");
    // The app closes its own sessions on the old database first.
    drop(s);

    let script = d.rename_database_script(OLD, NEW, &objects).unwrap();
    println!("statements:\n{}\nwarnings: {:?}", script.statements.join("\n"), script.warnings);
    // `schema_sync_run`: each statement on one session opened in `database_from`.
    for st in &script.statements {
        run(&mut admin, st).await;
    }

    // A fresh session: `list_databases` adds the session's own database,
    // and the script's `use` left this one in the old one.
    drop(admin);
    let mut admin = d.connect(&cfg(&url, &from), None).await.expect("connect");
    let names = databases(&mut admin).await;
    assert!(names.iter().any(|n| n == NEW), "{names:?}");
    assert!(!names.iter().any(|n| n == OLD), "old database still there: {names:?}");

    let mut n = d.connect(&cfg(&url, NEW), None).await.expect("connect");
    assert_eq!(count(&mut n, "db.clientes.find({})").await, 3);
    assert_eq!(count(&mut n, "db.pedidos.find({})").await, 2);
    assert_eq!(count(&mut n, "db.v_activos.find({})").await, 2);
    let listed: Vec<(String, String)> = n.list_objects().await.unwrap().into_iter().map(|o| (o.kind, o.name)).collect();
    assert!(listed.contains(&(kinds::VIEW.into(), "v_activos".into())), "{listed:?}");
    let ix = run(&mut n, "db.clientes.getIndexes()").await;
    let text = serde_json::to_string(&ix.results[0].rows).unwrap();
    // Rows of `v, key, name, unique`.
    assert!(text.contains(r#""{\"email\":1}","ux_email",true"#), "{text}");
    // Still unique.
    let mut out = QueryOutcome::default();
    let dup = n.execute(r#"db.clientes.insertOne({ _id: 9, email: "a@x" })"#, 10, &mut out).await;
    assert!(dup.is_err() || out.error.is_some(), "duplicate accepted");

    // Clean up.
    run(&mut n, "db.dropDatabase()").await;
    assert!(!databases(&mut admin).await.iter().any(|x| x == NEW || x == OLD));
}

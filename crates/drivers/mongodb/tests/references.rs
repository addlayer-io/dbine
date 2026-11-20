//! "Ver dependencias…", what an object uses (`Session::references`),
//! against a real server.
//!
//! Fixture, in a database of its own (`dbine_refs_<pid>`): `clientes`,
//! `pedidos` and `productos`; `v_pedidos` on `pedidos` with a `$lookup`
//! on `clientes` whose sub-pipeline has a `$unionWith` on `productos`;
//! `v_sobre_v` on `v_pedidos` with a `$graphLookup` on `clientes`. A view
//! can't read another database (`$lookup` `{db, coll}` is refused for user
//! databases) nor hold `$out` / `$merge`: the external sources are covered
//! by the unit tests. The database is dropped at the end, also when an
//! assertion fails.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test references -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, DependencyTarget, Driver, ObjectRef,
    QueryOutcome, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

fn cfg(database: &str) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_MONGODB_URL").ok()?;
    let mut c = ConnectionConfig {
        driver: "mongodb".into(),
        database: database.into(),
        ..Default::default()
    };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

fn target(kind: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: None,
    }
}

async fn checks(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>) {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), false);
    let r = s
        .references(&target(kinds::VIEW, "v_pedidos"), &scan)
        .await
        .unwrap();
    println!("v_pedidos: {:#?}", r.items);
    let got: Vec<(&str, &str, Option<&str>, Confidence)> = r
        .items
        .iter()
        .map(|i| {
            (
                i.kind.as_str(),
                i.name.as_str(),
                i.detail.as_deref(),
                i.confidence,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                kinds::COLLECTION,
                "pedidos",
                Some("viewOn"),
                Confidence::Confirmed
            ),
            (
                kinds::COLLECTION,
                "clientes",
                Some("$lookup"),
                Confidence::Confirmed
            ),
            (
                kinds::COLLECTION,
                "productos",
                Some("$unionWith"),
                Confidence::Confirmed
            ),
        ]
    );
    assert!(r.external.is_empty() && r.note.is_none());

    let r = s
        .references(&target(kinds::VIEW, "v_sobre_v"), &scan)
        .await
        .unwrap();
    println!("v_sobre_v: {:#?}", r.items);
    let got: Vec<(&str, &str)> = r
        .items
        .iter()
        .map(|i| (i.kind.as_str(), i.name.as_str()))
        .collect();
    assert_eq!(
        got,
        [(kinds::VIEW, "v_pedidos"), (kinds::COLLECTION, "clientes")]
    );

    // A collection uses nothing.
    let r = s
        .references(&target(kinds::COLLECTION, "pedidos"), &scan)
        .await
        .unwrap();
    assert!(r.items.is_empty() && r.external.is_empty(), "{r:?}");
}

#[tokio::test]
#[ignore]
async fn references_mongodb_live() {
    let db = format!("dbine_refs_{}", std::process::id());
    let Some(c) = cfg(&db) else {
        return;
    };
    let d = dbine_driver_mongodb::drivers()
        .into_iter()
        .find(|d| d.info().id == c.driver)
        .unwrap();
    let mut s = d.connect(&c, None).await.expect("connect");
    run(
        &mut s,
        r#"db.clientes.insertOne({ _id: 1, nombre: "Ana", jefe: null })
db.pedidos.insertOne({ _id: 1, cliente: 1, producto: 1 })
db.productos.insertOne({ _id: 1, nombre: "Té" })
db.createView("v_pedidos", "pedidos", [
  { $lookup: { from: "clientes", localField: "cliente", foreignField: "_id", as: "c",
               pipeline: [{ $unionWith: { coll: "productos", pipeline: [{ $match: { _id: 0 } }] } }] } }
])
db.createView("v_sobre_v", "v_pedidos", [
  { $graphLookup: { from: "clientes", startWith: "$cliente", connectFromField: "jefe", connectToField: "_id", as: "cadena" } }
])"#,
    )
    .await;
    let outcome = AssertUnwindSafe(checks(&d, &mut s)).catch_unwind().await;
    run(&mut s, "db.dropDatabase()").await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

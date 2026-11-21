//! "Linaje" of a whole database (`Session::lineage_graph`) against a real
//! MongoDB: collections as nodes (also one nobody reads), views over a
//! collection with a `$lookup` and a `$unionWith` in a sub-pipeline, and a
//! view over a view. A view can't read another database, so the external
//! sources are covered by the unit tests. The database is dropped at the
//! end, also when an assertion fails.
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test lineage -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{Confidence, LineageScope};
use dbine_driver::{ConnectionConfig, DependencyScan, Driver, QueryOutcome, Session};
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

async fn checks(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, db: &str) {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), false);
    let g = s
        .lineage_graph(db, &LineageScope::default(), &scan)
        .await
        .unwrap();
    let mut got: Vec<String> = g
        .edges
        .iter()
        .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
        .collect();
    got.sort();
    println!("{got:#?}");
    assert_eq!(
        got,
        [
            "clientes>v_pedidos:Confirmed",
            "pedidos>v_pedidos:Confirmed",
            "productos>v_pedidos:Confirmed",
            "v_pedidos>v_sobre_v:Confirmed",
        ]
    );
    assert!(
        g.objects.iter().any(|o| o.name == "suelta"),
        "{:?}",
        g.objects
    );
    assert!(g
        .edges
        .iter()
        .all(|e| e.confidence == Confidence::Confirmed));
    assert_eq!(g.scanned, 2);
    // A scope of views keeps no collection.
    let only = LineageScope {
        kinds: vec!["view".into()],
        ..Default::default()
    };
    let g = s.lineage_graph(db, &only, &scan).await.unwrap();
    assert_eq!(g.objects.len(), 2);
    assert_eq!(g.edges.len(), 1);
}

#[tokio::test]
#[ignore]
async fn lineage_mongodb_live() {
    let db = format!("dbine_lin_{}", std::process::id());
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
        r#"db.clientes.insertOne({ _id: 1, nombre: "Ana" })
db.pedidos.insertOne({ _id: 1, cliente: 1 })
db.productos.insertOne({ _id: 1, nombre: "Té" })
db.suelta.insertOne({ _id: 1 })
db.createView("v_pedidos", "pedidos", [
  { $lookup: { from: "clientes", localField: "cliente", foreignField: "_id", as: "c",
               pipeline: [{ $unionWith: { coll: "productos", pipeline: [{ $match: { _id: 0 } }] } }] } }
])
db.createView("v_sobre_v", "v_pedidos", [{ $match: {} }])"#,
    )
    .await;
    let outcome = AssertUnwindSafe(checks(&d, &mut s, &db))
        .catch_unwind()
        .await;
    run(&mut s, "db.dropDatabase()").await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

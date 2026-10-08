//! "Renombrar…" a field against a real Couchbase Server (already
//! initialized, as `integration.rs` leaves it). Couchbase renames no bucket,
//! scope or collection, has no foreign keys, checks, views or triggers: the
//! fixture is a collection `clientes` (`pepe` in every document but one),
//! an index on `pepe`, a partial index filtering on it, an index on the
//! nested `dir.pepe`, a primary index, and `otros`, another collection with
//! a field `pepe`. The field becomes `apodo`: every document changes, the
//! indexes are created again on it and answer, the nested field and
//! `otros.pepe` stay.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test rename -- --ignored --nocapture
//! ```

use dbine_driver::rename::{RenameRequest, RenameTarget};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
    out
}

async fn scalar(s: &mut Box<dyn Session>, text: &str) -> serde_json::Value {
    run(s, text).await.results[0].rows[0][0].clone()
}

async fn wait(s: &mut Box<dyn Session>, keyspace: &str) {
    for _ in 0..40 {
        if s.execute(&format!("SELECT RAW 1 FROM {keyspace} LIMIT 1"), 1, &mut QueryOutcome::default()).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("{keyspace} no quedó lista");
}

#[tokio::test]
#[ignore]
async fn rename_field_live() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    if s.list_databases().await.unwrap().contains(&"dbine_rename".to_string()) {
        s.drop_database("dbine_rename").await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    s.create_database("dbine_rename").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_rename")).await.unwrap();
    for coll in ["clientes", "otros"] {
        run(&mut s, &format!("CREATE COLLECTION dbine_rename._default.{coll}")).await;
        wait(&mut s, &format!("dbine_rename._default.{coll}")).await;
    }
    run(
        &mut s,
        "INSERT INTO dbine_rename._default.clientes (KEY, VALUE) VALUES ('c1', {'pepe': 'a', 'activo': true, 'dir': {'pepe': 'n1'}}), \
         ('c2', {'pepe': 'b', 'activo': false}), ('c3', {'pepe': 'c', 'activo': true}), ('c4', {'activo': true});
         INSERT INTO dbine_rename._default.otros (KEY, VALUE) VALUES ('o1', {'pepe': 'x'});
         CREATE PRIMARY INDEX ON dbine_rename._default.clientes;
         CREATE PRIMARY INDEX ON dbine_rename._default.otros;
         CREATE INDEX ix_pepe ON dbine_rename._default.clientes(pepe, activo);
         CREATE INDEX ix_activo ON dbine_rename._default.clientes(activo) WHERE pepe IS NOT MISSING;
         CREATE INDEX ix_dir ON dbine_rename._default.clientes(dir.pepe);",
    )
    .await;

    let spec = d.rename_spec().unwrap();
    let table = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("dbine_rename._default".into()), name: "clientes".into() };
    let target = RenameTarget::Column { table: table.clone(), column: "pepe".into() };
    assert!(spec.allows(&target));
    assert!(!spec.allows(&RenameTarget::Object { object: table.clone(), parent: None }));
    let schema = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "clientes" && t.schema == table.schema);
    assert!(schema.is_some());
    let req = RenameRequest { target, new_name: "apodo".into(), table: schema, definition: None };
    let script = d.rename_script(&req).unwrap();
    println!("{:#?}", script);
    for st in &script.statements {
        run(&mut s, st).await;
    }

    let ks = "dbine_rename._default.clientes";
    assert_eq!(scalar(&mut s, &format!("SELECT COUNT(*) FROM {ks} WHERE apodo IS NOT MISSING")).await, 3);
    assert_eq!(scalar(&mut s, &format!("SELECT COUNT(*) FROM {ks} WHERE pepe IS NOT MISSING")).await, 0);
    assert_eq!(scalar(&mut s, &format!("SELECT RAW dir.pepe FROM {ks} USE KEYS 'c1'")).await, "n1");
    assert_eq!(scalar(&mut s, "SELECT RAW pepe FROM dbine_rename._default.otros USE KEYS 'o1'").await, "x");
    // The indexes are on the new field, and the planner uses them.
    let keys = run(&mut s, "SELECT name, index_key, `condition` FROM system:indexes WHERE bucket_id = 'dbine_rename' AND keyspace_id = 'clientes' ORDER BY name").await;
    let text = serde_json::to_string(&keys.results[0].rows).unwrap();
    println!("{text}");
    assert!(text.contains("ix_pepe") && text.contains("ix_activo") && !text.contains("(`pepe`") && !text.contains("[\"`pepe`"), "{text}");
    let plan = run(&mut s, &format!("EXPLAIN SELECT activo FROM {ks} WHERE apodo = 'b'")).await;
    assert!(serde_json::to_string(&plan.results).unwrap().contains("ix_pepe"));
    assert_eq!(scalar(&mut s, &format!("SELECT COUNT(*) FROM {ks} WHERE apodo = 'b'")).await, 1);

    let mut s = d.connect(&c, None).await.unwrap();
    s.drop_database("dbine_rename").await.unwrap();
}

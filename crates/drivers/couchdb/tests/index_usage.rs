//! Index usage against a real CouchDB: `_all_docs` and two Mango indexes,
//! listed without counters (CouchDB counts none per index). Then "Eliminar
//! índice…": the schema sync script without one of them, run.
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig { driver: "couchdb".into(), host: h.into(), port: p.parse().ok()?, username: Some(user.into()), password: Some(pass.into()), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    let mut out = QueryOutcome::default();
    s.execute(q, 10, &mut out).await.unwrap_or_else(|e| panic!("{q}: {e}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchdb::drivers().remove(0);
    assert!(d.supports_index_usage());
    let mut admin = d.connect(&c, None).await.unwrap();
    let _ = admin.drop_database("dbine_ixu").await;
    admin.create_database("dbine_ixu").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_ixu")).await.unwrap();
    run(&mut s, r#"POST _index {"index": {"fields": ["cliente"]}, "name": "ix_cliente", "ddoc": "ix_cliente", "type": "json"}"#).await;
    run(&mut s, r#"POST _index {"index": {"fields": [{"fecha": "desc"}], "partial_filter_selector": {"fecha": {"$gt": 0}}}, "name": "ix_fecha", "ddoc": "ix_fecha", "type": "json"}"#).await;
    run(&mut s, r#"POST _bulk_docs {"docs": [{"_id": "p1", "cliente": "a", "fecha": 1}, {"_id": "p2", "cliente": "b", "fecha": 2}]}"#).await;
    run(&mut s, r#"{"selector": {"cliente": "a"}, "use_index": "ix_cliente"}"#).await;
    let obj = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "_all_docs".into() };
    let r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["_all_docs", "ix_cliente", "ix_fecha"]);
    assert!(r.indexes[0].primary_key);
    assert_eq!(r.indexes[2].key_columns, ["fecha DESC"]);
    assert!(r.indexes[2].filter.is_some());
    assert!(r.indexes[1].size_kb.is_some());

    let table = s.database_schema().await.unwrap().remove(0);
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "ix_fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "ix_fecha"));
    admin.drop_database("dbine_ixu").await.unwrap();
}

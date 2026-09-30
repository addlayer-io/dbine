//! "Comparar esquemas" against a real server: two databases, one with a
//! validation function (the CHECK) and Mango indexes (partial, descending),
//! the other without. The sync script is run on the target and both then
//! read the same indexes and CHECKs.
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    let mut out = QueryOutcome::default();
    s.execute(q, 10, &mut out).await.unwrap_or_else(|e| panic!("{q}: {e}"));
}

async fn schema(s: &mut Box<dyn Session>) -> TableSchema {
    let mut t = s.database_schema().await.unwrap().remove(0);
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
#[ignore]
async fn compare_and_sync() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchdb::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.unwrap();
    for db in ["dbine_cmp_src", "dbine_cmp_dst"] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.unwrap();
    }
    let mut a = d.connect(&c, Some("dbine_cmp_src")).await.unwrap();
    let mut b = d.connect(&c, Some("dbine_cmp_dst")).await.unwrap();
    run(&mut a, r#"PUT _design/reglas
{"validate_doc_update": "function (n) { if (!n._deleted && !n.tipo) { throw({ forbidden: 'falta tipo' }); } }"}"#).await;
    run(&mut a, r#"POST _index
{"index": {"fields": [{"tipo": "desc"}, {"fecha": "desc"}], "partial_filter_selector": {"activo": true}}, "name": "por_tipo", "type": "json"}"#).await;
    run(&mut b, r#"POST _index
{"index": {"fields": ["otro"]}, "name": "por_otro", "type": "json"}"#).await;

    let src = schema(&mut a).await;
    let dst = schema(&mut b).await;
    assert_eq!(src.checks.len(), 1);
    assert!(dst.checks.is_empty());
    // What the compare carries: the source's CHECK and index into the target.
    let mut new = dst.clone();
    new.checks = src.checks.clone();
    new.indexes.extend(src.indexes.iter().cloned());
    let script = d.sync_script(&[TableChange::Alter { old: dst, new }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut b, st).await;
    }
    let after = schema(&mut b).await;
    assert_eq!(after.checks, src.checks);
    assert!(src.indexes.iter().all(|i| after.indexes.contains(i)), "{:?}", after.indexes);
    // The validation now applies.
    let mut out = QueryOutcome::default();
    assert!(b.execute(r#"POST /dbine_cmp_dst {"x": 1}"#, 10, &mut out).await.is_err() || out.error.is_some());

    for db in ["dbine_cmp_src", "dbine_cmp_dst"] {
        admin.drop_database(db).await.unwrap();
    }
}

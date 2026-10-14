//! The data grid's column sort against a real server (ignored by default),
//! the container of `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, Error, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};

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
        database: "dbine_sort".into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{q}"));
    out
}

fn ids(out: &QueryOutcome) -> Vec<Value> {
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name == "_id").unwrap();
    r.rows.iter().map(|row| row[i].clone()).collect()
}

#[tokio::test]
#[ignore]
async fn couchdb_sorts_by_id_on_the_server() {
    let Some(c) = cfg() else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("DELETE /dbine_sort", 10, &mut out).await.ok();
    run(&mut s, "PUT /dbine_sort").await;
    run(
        &mut s,
        r#"POST /dbine_sort/_bulk_docs
{ "docs": [
  { "_id": "d1", "g": "a", "n": 5 },
  { "_id": "d2", "g": "a", "n": 2 },
  { "_id": "d3", "g": "b", "n": 9 },
  { "_id": "d4", "g": "a", "n": 2 }
] }"#,
    )
    .await;
    let mut s = d.connect(&c, Some("dbine_sort")).await.expect("connect to the database");
    let obj = ObjectRef { kind: "table".into(), schema: None, name: "dbine_sort".into() };
    let browse = s.browse_query(&obj, 50);
    let filtered = d.filtered_browse(&browse, &[ColumnFilter { column: "g".into(), op: FilterOp::Eq, values: vec![json!("a")], sql: None }]).unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let q = d.sorted_browse(&filtered, &[k("_id", true)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), vec![json!("d4"), json!("d2"), json!("d1")]);
    let q = d.sorted_browse(&browse, &[k("_id", false)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), vec![json!("d1"), json!("d2"), json!("d3"), json!("d4")]);
    // Without a Mango index over it, any other field is the grid's.
    assert!(matches!(d.sorted_browse(&filtered, &[k("n", false)]), Err(Error::Unsupported(_))));
    let mut out = QueryOutcome::default();
    s.execute("DELETE /dbine_sort", 10, &mut out).await.ok();
}

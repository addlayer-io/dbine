//! The data grid's column sort against real servers (ignored by default),
//! the same containers as `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};

const INDEX: &str = "dbine_sort";

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{q}"));
    out
}

fn column(out: &QueryOutcome, name: &str) -> Vec<Value> {
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no {name}"));
    r.rows.iter().map(|row| row[i].clone()).collect()
}

async fn check(id: &str, var: &str) {
    let Ok(url) = std::env::var(var) else { return };
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute(&format!("DELETE /{INDEX}"), 10, &mut out).await.ok();
    run(
        &mut s,
        &format!(
            r#"PUT /{INDEX}
{{ "mappings": {{ "properties": {{ "k": {{ "type": "integer" }}, "g": {{ "type": "keyword" }}, "n": {{ "type": "integer" }}, "we\"ird": {{ "type": "keyword" }}, "t": {{ "type": "text" }} }} }} }}

POST /{INDEX}/_bulk?refresh=true
{{ "index": {{ "_id": "1" }} }}
{{ "k": 1, "g": "a", "n": 5, "we\"ird": "c", "t": "x" }}
{{ "index": {{ "_id": "2" }} }}
{{ "k": 2, "g": "a", "n": 2, "we\"ird": "a", "t": "y" }}
{{ "index": {{ "_id": "3" }} }}
{{ "k": 3, "g": "b", "n": 9, "we\"ird": "b", "t": "z" }}
{{ "index": {{ "_id": "4" }} }}
{{ "k": 4, "g": "a", "n": 2, "we\"ird": "b", "t": "w" }}
{{ "index": {{ "_id": "5" }} }}
{{ "k": 5, "g": "a", "n": 7, "we\"ird": "0", "t": "v" }}"#
        ),
    )
    .await;
    let obj = ObjectRef { kind: "index".into(), schema: None, name: INDEX.into() };
    let browse = s.browse_query(&obj, 50);
    let filtered = d.filtered_browse(&browse, &[ColumnFilter { column: "g".into(), op: FilterOp::Eq, values: vec![json!("a")], sql: None }]).unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let q = d.sorted_browse(&filtered, &[k("n", true), k("k", true)]).unwrap();
    assert_eq!(column(&run(&mut s, &q).await, "k"), vec![json!(5), json!(1), json!(4), json!(2)]);
    let q = d.sorted_browse(&filtered, &[k("n", false), k("k", false)]).unwrap();
    assert_eq!(column(&run(&mut s, &q).await, "k"), vec![json!(2), json!(4), json!(1), json!(5)]);
    let q = d.sorted_browse(&filtered, &[k("we\"ird", true)]).unwrap();
    assert_eq!(column(&run(&mut s, &q).await, "k"), vec![json!(1), json!(4), json!(2), json!(5)]);
    // A text field without field data: the server refuses it (the driver
    // can't see the mapping when it builds the sort).
    let q = d.sorted_browse(&filtered, &[k("t", false)]).unwrap();
    let mut out = QueryOutcome::default();
    let err = s.execute(&q, 100, &mut out).await.expect_err("text sort");
    eprintln!("{id}: text field sort → {err}");
    let mut out = QueryOutcome::default();
    s.execute(&format!("DELETE /{INDEX}"), 10, &mut out).await.ok();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_sorts_on_the_server() {
    check("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opensearch_sorts_on_the_server() {
    check("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

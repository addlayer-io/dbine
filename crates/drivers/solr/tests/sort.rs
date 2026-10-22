//! The data grid's column sort against a real server (ignored by default),
//! the standalone container of `integration.rs` (core `testcore`, a
//! schemaless configset):
//!
//! ```sh
//! DBINE_TEST_SOLR_URL=http://localhost:25522 cargo test -p dbine-driver-solr --test sort -- --ignored
//! ```

use dbine_driver::{
    ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey,
};
use serde_json::{json, Value};

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{q}"));
    out
}

fn ids(out: &QueryOutcome) -> Vec<Value> {
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name == "id").unwrap();
    r.rows.iter().map(|row| row[i].clone()).collect()
}

#[tokio::test]
#[ignore]
async fn solr_sorts_on_the_server() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else {
        return;
    };
    let d = &dbine_driver_solr::drivers()[0];
    let cfg = ConnectionConfig {
        driver: "solr".into(),
        host: url,
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let clean =
        "POST /solr/testcore/update?commit=true\n{ \"delete\": { \"query\": \"id:dbsort*\" } }";
    run(&mut s, clean).await;
    run(
        &mut s,
        r#"POST /solr/testcore/update?commit=true
[
  { "id": "dbsort1", "dbsort_g": "a", "dbsort_n": 5, "dbsort.w-x": 3 },
  { "id": "dbsort2", "dbsort_g": "a", "dbsort_n": 2, "dbsort.w-x": 1 },
  { "id": "dbsort3", "dbsort_g": "b", "dbsort_n": 9, "dbsort.w-x": 2 },
  { "id": "dbsort4", "dbsort_g": "a", "dbsort_n": 2, "dbsort.w-x": 2 },
  { "id": "dbsort5", "dbsort_g": "a", "dbsort_n": 7, "dbsort.w-x": 0 }
]"#,
    )
    .await;
    let obj = ObjectRef {
        kind: "core".into(),
        schema: None,
        name: "testcore".into(),
    };
    let browse = s.browse_query(&obj, 50);
    let filtered = d
        .filtered_browse(
            &browse,
            &[
                ColumnFilter {
                    column: "id".into(),
                    op: FilterOp::StartsWith,
                    values: vec![json!("dbsort")],
                    sql: None,
                },
                ColumnFilter {
                    column: "dbsort_g".into(),
                    op: FilterOp::Eq,
                    values: vec![json!("a")],
                    sql: None,
                },
            ],
        )
        .unwrap();
    let k = |c: &str, desc: bool| SortKey {
        column: c.into(),
        descending: desc,
    };
    let want = |v: &[u8]| {
        v.iter()
            .map(|n| json!(format!("dbsort{n}")))
            .collect::<Vec<_>>()
    };
    let q = d
        .sorted_browse(&filtered, &[k("dbsort_n", true), k("id", true)])
        .unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 1, 4, 2]));
    let q = d
        .sorted_browse(&filtered, &[k("dbsort_n", false), k("id", false)])
        .unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[2, 4, 1, 5]));
    let q = d
        .sorted_browse(&filtered, &[k("dbsort.w-x", false)])
        .unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 2, 4, 1]));
    // A guessed text field (multi-valued text_general): the server refuses
    // it (the driver can't see the schema when it builds the sort).
    let q = d.sorted_browse(&filtered, &[k("dbsort_g", false)]).unwrap();
    let mut out = QueryOutcome::default();
    let err = s.execute(&q, 100, &mut out).await.expect_err("text sort");
    eprintln!("solr: text field sort → {err}");
    run(&mut s, clean).await;
}

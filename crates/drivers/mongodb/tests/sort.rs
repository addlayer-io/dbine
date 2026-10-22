//! The data grid's column sort against real servers (ignored by default),
//! the same containers as `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/ \
//!   cargo test -p dbine-driver-mongodb --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, SortKey};
use serde_json::{json, Value};

async fn ids(s: &mut Box<dyn dbine_driver::Session>, q: &str) -> Vec<Value> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{q}"));
    let r = &out.results[0];
    let i = r.columns.iter().position(|c| c.name == "_id").unwrap();
    r.rows.iter().map(|row| row[i].clone()).collect()
}

async fn check(driver: usize, url_var: &str) {
    let Ok(url) = std::env::var(url_var) else {
        return;
    };
    let d = &dbine_driver_mongodb::drivers()[driver];
    let mut c = ConnectionConfig {
        driver: d.info().id.into(),
        database: "dbine_it".into(),
        ..Default::default()
    };
    c.options.insert("connection_string".into(), url);
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.getCollection(\"so rt\").drop()", 10, &mut out)
        .await
        .ok();
    let mut out = QueryOutcome::default();
    s.execute(
        r#"db.getCollection("so rt").insertMany([
             { _id: 1, g: "a", n: 5, "we\"ird": 3 },
             { _id: 2, g: "a", n: 2, "we\"ird": 1 },
             { _id: 3, g: "b", n: 9, "we\"ird": 2 },
             { _id: 4, g: "a", n: 2, "we\"ird": 2 },
             { _id: 5, g: "a", n: 7, "we\"ird": 0 }
           ])"#,
        10,
        &mut out,
    )
    .await
    .expect("insert");

    let obj = ObjectRef {
        kind: "collection".into(),
        schema: None,
        name: "so rt".into(),
    };
    let browse = s.browse_query(&obj, 50);
    let filtered = d
        .filtered_browse(
            &browse,
            &[ColumnFilter {
                column: "g".into(),
                op: FilterOp::Eq,
                values: vec![json!("a")],
                sql: None,
            }],
        )
        .unwrap();
    let k = |c: &str, desc: bool| SortKey {
        column: c.into(),
        descending: desc,
    };
    // n desc, then _id desc for the tie at 2; with the filter on.
    let q = d
        .sorted_browse(&filtered, &[k("n", true), k("_id", true)])
        .unwrap();
    assert_eq!(
        ids(&mut s, &q).await,
        vec![json!(5), json!(1), json!(4), json!(2)]
    );
    let q = d
        .sorted_browse(&filtered, &[k("n", false), k("_id", false)])
        .unwrap();
    assert_eq!(
        ids(&mut s, &q).await,
        vec![json!(2), json!(4), json!(1), json!(5)]
    );
    // A field name with a quote.
    let q = d.sorted_browse(&filtered, &[k("we\"ird", false)]).unwrap();
    assert_eq!(
        ids(&mut s, &q).await,
        vec![json!(5), json!(2), json!(4), json!(1)]
    );
    let mut out = QueryOutcome::default();
    s.execute("db.getCollection(\"so rt\").drop()", 10, &mut out)
        .await
        .ok();
}

#[tokio::test]
#[ignore]
async fn mongodb_sorts_on_the_server() {
    check(0, "DBINE_TEST_MONGODB_URL").await;
}

#[tokio::test]
#[ignore]
async fn ferretdb_sorts_on_the_server() {
    check(1, "DBINE_TEST_FERRETDB_URL").await;
}

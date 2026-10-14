//! The data grid's column sort against a real TDengine (same container and
//! variable as `integration.rs`):
//! `DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine --test sort -- --ignored`.

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

#[tokio::test]
#[ignore]
async fn tdengine_sorts_on_the_server() {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").unwrap()).unwrap();
    let c = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP DATABASE IF EXISTS dbine_sort; CREATE DATABASE dbine_sort").await;
    run(
        &mut s,
        "CREATE STABLE dbine_sort.st (ts TIMESTAMP, v DOUBLE, `odd name` INT) TAGS (loc VARCHAR(20));
         CREATE TABLE dbine_sort.d1 USING dbine_sort.st TAGS ('norte');
         CREATE TABLE dbine_sort.d2 USING dbine_sort.st TAGS ('sur');
         INSERT INTO dbine_sort.d1 VALUES ('2024-01-01 00:00:00.000', 3, 1) ('2024-01-01 00:00:01.000', 1, 2) ('2024-01-01 00:00:02.000', 2, 3);
         INSERT INTO dbine_sort.d2 VALUES ('2024-01-01 00:00:00.000', 2, 4) ('2024-01-01 00:00:01.000', 9, 5);",
    )
    .await;
    let obj = ObjectRef { kind: "stable".into(), schema: Some("dbine_sort".into()), name: "st".into() };
    let browse = s.browse_query(&obj, 200);
    let filtered = d.filtered_browse(&browse, &[ColumnFilter { column: "v".into(), op: FilterOp::Lt, values: vec![json!(5)], sql: None }]).unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let q = d.sorted_browse(&filtered, &[k("v", true), k("odd name", false)]).unwrap();
    println!("{q}");
    let out = run(&mut s, &q).await;
    let r = &out.results[0];
    let col = |name: &str| -> Vec<Value> {
        let i = r.columns.iter().position(|c| c.name == name).unwrap();
        r.rows.iter().map(|row| row[i].clone()).collect()
    };
    assert_eq!(col("v"), [json!(3), json!(2), json!(2), json!(1)], "{r:?}");
    assert_eq!(col("odd name"), [json!(1), json!(3), json!(4), json!(2)], "{r:?}");
    // By a tag, then time descending.
    let q = d.sorted_browse(&filtered, &[k("loc", true), k("ts", true)]).unwrap();
    let out = run(&mut s, &q).await;
    let r = &out.results[0];
    let i = r.columns.iter().position(|c| c.name == "odd name").unwrap();
    assert_eq!(r.rows.iter().map(|row| row[i].clone()).collect::<Vec<_>>(), [json!(4), json!(3), json!(2), json!(1)], "{r:?}");
    run(&mut s, "DROP DATABASE IF EXISTS dbine_sort").await;
}

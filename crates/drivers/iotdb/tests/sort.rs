//! The data grid's column sort against real servers (same containers and
//! variables as `integration.rs`):
//! `DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 cargo test -p dbine-driver-iotdb --test sort -- --ignored`.

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

async fn sorts(url: &str) {
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    let cfg = ConnectionConfig { driver: "iotdb".into(), host: url.into(), username: Some("root".into()), password: Some("root".into()), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    println!("{}", s.server_version().await.unwrap());
    let mut out = QueryOutcome::default();
    let _ = s.execute("DELETE DATABASE root.dbine_sort", 10, &mut out).await;
    // (time, temp, `odd name`, on): temp 3, 1, 2, 9 and 2 again.
    let mut script = String::from("CREATE DATABASE root.dbine_sort;\n");
    for (i, (temp, odd, on)) in [(3.0, 1, true), (1.0, 2, true), (2.0, 3, true), (9.0, 4, false), (2.0, 5, true)].iter().enumerate() {
        script.push_str(&format!(
            "INSERT INTO root.dbine_sort.d1(timestamp, temp, `odd name`, on) VALUES ({}, {temp}, {odd}, {on});\n",
            1_706_708_700_000i64 + i as i64 * 1000
        ));
    }
    run(&mut s, &script).await;
    let mut s = d.connect(&cfg, Some("root.dbine_sort")).await.unwrap();
    let obj = ObjectRef { kind: "device".into(), schema: None, name: "d1".into() };
    let browse = s.browse_query(&obj, 200);
    let filtered = d
        .filtered_browse(&browse, &[ColumnFilter { column: "root.dbine_sort.d1.on".into(), op: FilterOp::IsTrue, values: vec![], sql: None }])
        .unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let col = |r: &dbine_driver::StatementResult, name: &str| -> Vec<Value> {
        let i = r.columns.iter().position(|c| c.name.ends_with(name)).unwrap_or_else(|| panic!("{name}: {:?}", r.columns));
        r.rows.iter().map(|row| row[i].clone()).collect()
    };
    // temp descending, then the odd-named series descending on ties.
    let q = d.sorted_browse(&filtered, &[k("root.dbine_sort.d1.temp", true), k("root.dbine_sort.d1.`odd name`", true)]).unwrap();
    println!("{q}");
    let out = run(&mut s, &q).await;
    let r = &out.results[0];
    assert_eq!(col(r, "temp"), [json!(3.0), json!(2.0), json!(2.0), json!(1.0)], "{r:?}");
    assert_eq!(col(r, "odd name`"), [json!(1.0), json!(5.0), json!(3.0), json!(2.0)], "{r:?}");
    // Time ascending.
    let q = d.sorted_browse(&filtered, &[k("Time", false)]).unwrap();
    let out = run(&mut s, &q).await;
    assert_eq!(col(&out.results[0], "temp"), [json!(3.0), json!(1.0), json!(2.0), json!(2.0)]);
    run(&mut s, "DELETE DATABASE root.dbine_sort").await;
}

#[tokio::test]
#[ignore]
async fn iotdb1_sorts_on_the_server() {
    sorts(&std::env::var("DBINE_TEST_IOTDB_URL").unwrap()).await;
}

#[tokio::test]
#[ignore]
async fn iotdb2_sorts_on_the_server() {
    sorts(&std::env::var("DBINE_TEST_IOTDB2_URL").unwrap()).await;
}

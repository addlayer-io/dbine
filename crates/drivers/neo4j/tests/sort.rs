//! The data grid's column sort against real servers (same containers and
//! variables as `integration.rs`):
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 cargo test -p dbine-driver-neo4j --test sort -- --ignored`.

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

async fn sorts(id: &str, url: &str) {
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let mut s = d.connect(&cfg(id, url), None).await.unwrap();
    run(&mut s, "MATCH (n:DbineSort) DETACH DELETE n").await;
    run(
        &mut s,
        "CREATE (:DbineSort {name: 'a', `the score`: 3, grp: 1}), (:DbineSort {name: 'b', `the score`: 1, grp: 1}),
                (:DbineSort {name: 'c', `the score`: 2, grp: 2}), (:DbineSort {name: 'd', `the score`: 2, grp: 1})",
    )
    .await;
    let obj = ObjectRef { kind: "label".into(), schema: None, name: "DbineSort".into() };
    let browse = s.browse_query(&obj, 200);
    let filtered = d
        .filtered_browse(&browse, &[ColumnFilter { column: "grp".into(), op: FilterOp::Eq, values: vec![json!(1)], sql: None }])
        .unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let q = d.sorted_browse(&filtered, &[k("the score", true), k("name", false)]).unwrap();
    println!("{id}: {q}");
    let out = run(&mut s, &q).await;
    let r = &out.results[0];
    let names: Vec<Value> = r
        .rows
        .iter()
        .map(|row| {
            let node: Value = serde_json::from_str(row[0].as_str().unwrap()).unwrap();
            node["~properties"]["name"].clone()
        })
        .collect();
    assert_eq!(names, [json!("a"), json!("d"), json!("b")], "{r:?}");
    run(&mut s, "MATCH (n:DbineSort) DETACH DELETE n").await;
}

#[tokio::test]
#[ignore]
async fn neo4j_sorts_on_the_server() {
    sorts("neo4j", &std::env::var("DBINE_TEST_NEO4J_URL").unwrap()).await;
}

#[tokio::test]
#[ignore]
async fn memgraph_sorts_on_the_server() {
    sorts("memgraph", &std::env::var("DBINE_TEST_MEMGRAPH_URL").unwrap()).await;
}

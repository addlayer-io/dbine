//! The data grid's column sort against a real server (ignored by default),
//! the node of `integration.rs` once initialized (it uses a collection of
//! its own in the `dbine_script0` bucket that test leaves):
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

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
async fn couchbase_sorts_on_the_server() {
    let Some(c) = cfg() else { return };
    let d = &dbine_driver_couchbase::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    let coll = "dbine_script0._default.dbine_sort";
    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP COLLECTION {coll}"), 10, &mut out).await.ok();
    run(&mut s, &format!("CREATE COLLECTION {coll}")).await;
    // The new collection takes a moment to accept an index.
    let start = Instant::now();
    loop {
        let mut out = QueryOutcome::default();
        match s.execute(&format!("CREATE PRIMARY INDEX ON {coll}"), 10, &mut out).await {
            Ok(()) => break,
            Err(e) if start.elapsed() < Duration::from_secs(60) => {
                eprintln!("waiting: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    run(
        &mut s,
        &format!(
            r#"INSERT INTO {coll} (KEY, VALUE) VALUES
  ("d1", {{ "g": "a", "n": 5, "we`ird \"n\"": 3 }}),
  ("d2", {{ "g": "a", "n": 2, "we`ird \"n\"": 1 }}),
  ("d3", {{ "g": "b", "n": 9, "we`ird \"n\"": 2 }}),
  ("d4", {{ "g": "a", "n": 2, "we`ird \"n\"": 2 }}),
  ("d5", {{ "g": "a", "n": 7, "we`ird \"n\"": 0 }})"#
        ),
    )
    .await;
    let obj = ObjectRef { kind: "collection".into(), schema: Some("dbine_script0._default".into()), name: "dbine_sort".into() };
    let browse = s.browse_query(&obj, 50);
    let filtered = d.filtered_browse(&browse, &[ColumnFilter { column: "g".into(), op: FilterOp::Eq, values: vec![json!("a")], sql: None }]).unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let want = |v: &[u8]| v.iter().map(|n| json!(format!("d{n}"))).collect::<Vec<_>>();
    let q = d.sorted_browse(&filtered, &[k("n", true), k("_id", true)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 1, 4, 2]));
    let q = d.sorted_browse(&filtered, &[k("n", false), k("_id", false)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[2, 4, 1, 5]));
    let q = d.sorted_browse(&filtered, &[k("we`ird \"n\"", false)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 2, 4, 1]));
    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP COLLECTION {coll}"), 10, &mut out).await.ok();
}

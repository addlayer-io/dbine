//! Column sort of the data grid against the Spanner emulator: the browse
//! query, filtered (`filtered_browse`) and then sorted (`sorted_browse`)
//! with two keys, one of them descending and on a column whose name needs
//! quoting (backticks). Skipped without the variable:
//!
//! ```sh
//! docker start dbine-test-spanner   # -p 25303:9020
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use serde_json::json;
use std::sync::Arc;

async fn connect(url: &str, db: &str) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.delete(format!("{url}/v1/projects/test/instances/i1/databases/{db}")).send().await;
    http.post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": format!("CREATE DATABASE `{db}`") }))
        .send()
        .await
        .unwrap();
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: db.into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url)] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    (d, s)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

fn ids(out: &QueryOutcome) -> Vec<String> {
    let r = out.results.iter().find(|r| !r.columns.is_empty()).expect("a result set");
    let i = r.columns.iter().position(|c| c.name.eq_ignore_ascii_case("id")).expect("the id column");
    r.rows.iter().map(|row| match &row[i] { serde_json::Value::String(s) => s.clone(), v => v.to_string() }).collect()
}

/// Rows (id, "Grupo Mixto", "Valor"): (1, b, 10), (2, a, 5), (3, b, 30),
/// (4, a, 20), (5, c, 1), (6, a, 15). Filtered on Valor >= 5 and sorted on
/// "Grupo Mixto" ASC, Valor DESC; the limit applies after the sort.
async fn check(d: &dyn Driver, s: &mut Box<dyn Session>, obj: &ObjectRef, group: &str, value: &str) {
    let filter = [ColumnFilter { column: value.into(), op: FilterOp::Ge, values: vec![serde_json::json!(5)], sql: None }];
    let sort = [SortKey { column: group.into(), descending: false }, SortKey { column: value.into(), descending: true }];
    for (limit, want) in [(100, vec!["4", "6", "2", "3", "1"]), (3, vec!["4", "6", "2"])] {
        let q = d.filtered_browse(&s.browse_query(obj, limit), &filter).unwrap();
        let q = d.sorted_browse(&q, &sort).unwrap();
        assert_eq!(ids(&run(s, &q).await), want, "{q}");
    }
    let sort = [SortKey { column: group.into(), descending: true }, SortKey { column: "id".into(), descending: false }];
    let q = d.sorted_browse(&s.browse_query(obj, 100), &sort).unwrap();
    assert_eq!(ids(&run(s, &q).await), ["5", "1", "3", "2", "4", "6"], "{q}");
}

#[tokio::test]
#[ignore]
async fn spanner_sorts_the_filtered_browse() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let (d, mut s) = connect(&url, "dbine_sort").await;
    run(&mut s, "CREATE TABLE T (id INT64 NOT NULL, `GrupoMixto` STRING(10), `Valor` INT64) PRIMARY KEY (id)").await;
    run(&mut s, "INSERT INTO T (id, GrupoMixto, Valor) VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef { kind: "table".into(), schema: None, name: "T".into() };
    check(d.as_ref(), &mut s, &obj, "GrupoMixto", "Valor").await;
    // A key that isn't a plain identifier is quoted, not run: the server
    // refuses the unknown column instead of reading `Valor DESC, id`.
    let q = d.sorted_browse(&s.browse_query(&obj, 10), &[SortKey { column: "Valor DESC, id".into(), descending: false }]).unwrap();
    let mut out = QueryOutcome::default();
    let failed = s.execute(&q, 10, &mut out).await.is_err() || out.error.is_some();
    assert!(failed, "{q}");
}

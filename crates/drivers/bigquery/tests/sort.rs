//! Column sort of the data grid against bigquery-emulator: the browse
//! query, filtered (`filtered_browse`) and then sorted (`sorted_browse`)
//! with two keys, one of them descending and on a column whose name needs
//! quoting (backticks). Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 cargo test -p dbine-driver-bigquery --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};

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
async fn bigquery_sorts_the_filtered_browse() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else { return };
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    let mut s = d.connect(&c, Some("ds1")).await.unwrap();
    let _ = s.execute("DROP TABLE IF EXISTS ds1.dbine_sort", 10, &mut QueryOutcome::default()).await;
    run(&mut s, "CREATE TABLE ds1.dbine_sort (id INT64, `Grupo Mixto` STRING, `Valor` INT64)").await;
    run(&mut s, "INSERT INTO ds1.dbine_sort (id, `Grupo Mixto`, `Valor`) VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef { kind: "table".into(), schema: Some("ds1".into()), name: "dbine_sort".into() };
    check(d.as_ref(), &mut s, &obj, "Grupo Mixto", "Valor").await;
    run(&mut s, "DROP TABLE ds1.dbine_sort").await;
}

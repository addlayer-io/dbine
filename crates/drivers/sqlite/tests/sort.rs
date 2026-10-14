//! Column sort of the data grid on a temporary file (no server needed, so
//! it runs by default): the browse query, filtered (`filtered_browse`) and
//! then sorted (`sorted_browse`) with two keys, one of them descending and
//! on a column whose name needs quoting.

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
async fn sqlite_sorts_the_filtered_browse() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-sort-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE TABLE dbine_sort (id INTEGER PRIMARY KEY, \"Grupo Mixto\" TEXT, \"Valor\" INTEGER)").await;
    run(&mut s, "INSERT INTO dbine_sort VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef { kind: "table".into(), schema: None, name: "dbine_sort".into() };
    check(d.as_ref(), &mut s, &obj, "Grupo Mixto", "Valor").await;
    drop(s);
    let _ = std::fs::remove_file(&path);
}

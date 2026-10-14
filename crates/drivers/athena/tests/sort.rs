//! Column sort of the data grid on Athena's browse query (no server here):
//! filtered (`filtered_browse`) and then sorted (`sorted_browse`) with
//! two keys, one of them descending and on a column whose name needs
//! quoting. The order goes after the filter's `WHERE` and before the
//! `LIMIT`, so the limit takes the first rows of the order.

use dbine_driver::sql::{select_top, Limit, Quote};
use dbine_driver::{ColumnFilter, Error, FilterOp, SortKey};

#[test]
fn sorts_the_filtered_browse() {
    let d = dbine_driver_athena::drivers().remove(0);
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let browse = select_top(Quote::Double, Limit::Limit, Some("S"), "T", 200);
    assert_eq!(d.sorted_browse(&browse, &[]).unwrap(), browse);
    assert_eq!(d.sorted_browse(&browse, &[k("a", false)]).unwrap(), "SELECT *\nFROM \"S\".\"T\"\nORDER BY \"a\" ASC\nLIMIT 200");
    let f = [ColumnFilter { column: "Valor".into(), op: FilterOp::Ge, values: vec![serde_json::json!(5)], sql: None }];
    let w = d.filtered_browse(&browse, &f).unwrap();
    assert_eq!(
        d.sorted_browse(&w, &[k("Grupo Mixto", false), k("Va\"lor", true)]).unwrap(),
        "SELECT *\nFROM \"S\".\"T\"\nWHERE \"Valor\" >= 5\nORDER BY \"Grupo Mixto\" ASC, \"Va\"\"lor\" DESC\nLIMIT 200"
    );
    // A filter value that looks like clauses stays inside its literal.
    let f = [ColumnFilter { column: "n".into(), op: FilterOp::Eq, values: vec![serde_json::json!("x' LIMIT 1 --")], sql: None }];
    let w = d.filtered_browse(&browse, &f).unwrap();
    assert!(d.sorted_browse(&w, &[k("a", true)]).unwrap().ends_with("\nORDER BY \"a\" DESC\nLIMIT 200"));
    assert!(matches!(d.sorted_browse("SHOW TABLES", &[k("a", false)]), Err(Error::Unsupported(_))));
}

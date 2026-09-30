//! `EXPLAIN QUERY PLAN` rows (`id`, `parent`, `detail`) as a
//! [`PlanNode`] tree under a synthetic `QUERY PLAN` root. SQLite reports
//! no costs nor row counts, only the access path of each step.

use dbine_driver::{Plan, PlanNode};

/// Statements EXPLAIN QUERY PLAN describes (reads and DML).
pub fn explainable(stmt: &str) -> bool {
    let kw: String = stmt.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    matches!(
        kw.to_ascii_lowercase().as_str(),
        "select" | "with" | "values" | "insert" | "update" | "delete" | "replace"
    )
}

/// The statement, cut to a line for messages.
pub fn short(stmt: &str) -> String {
    let one: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(60).collect::<String>())
    } else {
        one
    }
}

/// `SEARCH t USING INDEX i (a=?)` → op `SEARCH`, object `t`, detail
/// `USING INDEX i (a=?)`; other steps keep their text as the operator.
fn node(detail: &str) -> PlanNode {
    let mut n = PlanNode { props: vec![("detail".into(), detail.to_string())], ..Default::default() };
    let mut words = detail.splitn(3, ' ');
    match (words.next(), words.next(), words.next()) {
        (Some(op @ ("SCAN" | "SEARCH")), Some(object), rest) if object != "CONSTANT" => {
            n.op = op.into();
            n.object = Some(object.into());
            n.detail = rest.unwrap_or("").to_string();
        }
        _ => n.op = detail.to_string(),
    }
    if detail.contains("AUTOMATIC") {
        n.warnings.push("Índice automático: SQLite lo arma en cada ejecución; conviene crearlo".into());
    }
    n
}

/// Rows `(id, parent, detail)` in output order into the tree.
pub fn query_plan(statement: &str, rows: &[(i64, i64, String)]) -> Plan {
    fn attach(parent: i64, rows: &[(i64, i64, String)]) -> Vec<PlanNode> {
        rows.iter()
            .filter(|(id, p, _)| *p == parent && *id != parent)
            .map(|(id, _, d)| {
                let mut n = node(d);
                n.children = attach(*id, rows);
                n
            })
            .collect()
    }
    let root = PlanNode { op: "QUERY PLAN".into(), children: attach(0, rows), ..Default::default() };
    let mut raw = String::from("QUERY PLAN\n");
    draw(&root.children, "", &mut raw);
    Plan { statement: statement.into(), root, actual: false, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// The sqlite3 shell's drawing: `|--` and `` `-- ``.
fn draw(children: &[PlanNode], prefix: &str, out: &mut String) {
    for (i, c) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        let text = c.props.first().map_or(c.op.as_str(), |(_, v)| v.as_str());
        out.push_str(&format!("{prefix}{}{text}\n", if last { "`--" } else { "|--" }));
        draw(&c.children, &format!("{prefix}{}", if last { "   " } else { "|  " }), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_become_a_tree() {
        let rows = vec![
            (2, 0, "SCAN a".to_string()),
            (5, 0, "SEARCH b USING AUTOMATIC COVERING INDEX (a_id=?)".to_string()),
            (9, 0, "CORRELATED SCALAR SUBQUERY 1".to_string()),
            (12, 9, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)".to_string()),
            (20, 0, "USE TEMP B-TREE FOR ORDER BY".to_string()),
        ];
        let p = query_plan("q", &rows);
        assert_eq!(p.root.op, "QUERY PLAN");
        assert_eq!(p.root.children.len(), 4);
        let a = &p.root.children[0];
        assert_eq!((a.op.as_str(), a.object.as_deref(), a.detail.as_str()), ("SCAN", Some("a"), ""));
        assert!(!p.root.children[1].warnings.is_empty());
        let sub = &p.root.children[2];
        assert_eq!(sub.children[0].detail, "USING INTEGER PRIMARY KEY (rowid=?)");
        assert_eq!(p.raw.lines().nth(4), Some("|  `--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"));
        assert!(!p.actual);
        assert!(explainable(" delete from t") && !explainable("create table t (a)"));
    }
}

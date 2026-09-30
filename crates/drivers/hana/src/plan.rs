//! HANA plans: `EXPLAIN PLAN SET STATEMENT_NAME = … FOR <stmt>` fills
//! `EXPLAIN_PLAN_TABLE` with one row per operator, linked by
//! `OPERATOR_ID` / `PARENT_OPERATOR_ID`.

use dbine_driver::PlanNode;

/// One `EXPLAIN_PLAN_TABLE` row.
#[derive(Debug, Default, Clone)]
pub struct Op {
    pub id: i64,
    pub parent: Option<i64>,
    pub name: String,
    pub details: String,
    pub schema: String,
    pub table: String,
    pub table_type: String,
    pub output_size: Option<f64>,
    pub subtree_cost: Option<f64>,
    pub engine: String,
}

pub fn tree(ops: &[Op]) -> PlanNode {
    fn node(op: &Op, ops: &[Op]) -> PlanNode {
        let object = (!op.table.is_empty()).then(|| {
            if op.schema.is_empty() {
                op.table.clone()
            } else {
                format!("{}.{}", op.schema, op.table)
            }
        });
        let mut n = PlanNode {
            op: op.name.clone(),
            detail: op.details.split_whitespace().collect::<Vec<_>>().join(" "),
            object,
            total_cost: op.subtree_cost,
            est_rows: op.output_size,
            ..Default::default()
        };
        for (k, v) in [("Motor", &op.engine), ("Tipo de tabla", &op.table_type)] {
            if !v.is_empty() {
                n.props.push((k.to_string(), v.clone()));
            }
        }
        n.props.push(("Operador".into(), op.id.to_string()));
        if op.name.contains("TABLE SCAN") {
            n.warnings.push("Recorre la tabla completa".into());
        }
        if op.name.contains("CROSS JOIN") || op.name.contains("PRODUCT") {
            n.warnings.push("Producto cartesiano".into());
        }
        n.children = ops.iter().filter(|c| c.parent == Some(op.id) && c.id != op.id).map(|c| node(c, ops)).collect();
        n
    }
    let ids: Vec<i64> = ops.iter().map(|o| o.id).collect();
    let mut roots: Vec<PlanNode> =
        ops.iter().filter(|o| o.parent.is_none_or(|p| !ids.contains(&p))).map(|o| node(o, ops)).collect();
    if roots.len() == 1 {
        roots.pop().expect("one")
    } else {
        PlanNode { op: "PLAN".into(), children: roots, ..Default::default() }
    }
}

/// The rows as text (the plan's raw form).
pub fn raw(ops: &[Op]) -> String {
    fn walk(ops: &[Op], id: i64, depth: usize, out: &mut String) {
        for o in ops.iter().filter(|o| o.parent == Some(id) && o.id != id) {
            line(o, depth, out);
            walk(ops, o.id, depth + 1, out);
        }
    }
    fn line(o: &Op, depth: usize, out: &mut String) {
        let mut l = format!("{}{}", "  ".repeat(depth), o.name);
        if !o.details.is_empty() {
            l.push_str(&format!(" ({})", o.details.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        if !o.table.is_empty() {
            l.push_str(&format!(" [{}]", o.table));
        }
        if let Some(r) = o.output_size {
            l.push_str(&format!(" rows={r}"));
        }
        if let Some(c) = o.subtree_cost {
            l.push_str(&format!(" cost={c}"));
        }
        out.push_str(&l);
        out.push('\n');
    }
    let ids: Vec<i64> = ops.iter().map(|o| o.id).collect();
    let mut out = String::new();
    for o in ops.iter().filter(|o| o.parent.is_none_or(|p| !ids.contains(&p))) {
        line(o, 0, &mut out);
        walk(ops, o.id, 1, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: i64, parent: Option<i64>, name: &str, table: &str, rows: f64, cost: f64) -> Op {
        Op {
            id,
            parent,
            name: name.into(),
            table: table.into(),
            schema: if table.is_empty() { String::new() } else { "S".into() },
            output_size: Some(rows),
            subtree_cost: Some(cost),
            engine: "HEX".into(),
            ..Default::default()
        }
    }

    #[test]
    fn rows_link_by_parent_id() {
        let ops = [
            op(1, None, "PROJECT", "", 10.0, 3.0),
            op(2, Some(1), "HASH JOIN", "", 10.0, 2.5),
            op(3, Some(2), "COLUMN TABLE", "A", 100.0, 1.0),
            op(4, Some(2), "TABLE SCAN", "B", 50.0, 1.0),
        ];
        let root = tree(&ops);
        assert_eq!(root.op, "PROJECT");
        assert_eq!(root.total_cost, Some(3.0));
        let join = &root.children[0];
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].object.as_deref(), Some("S.A"));
        assert_eq!(join.children[1].warnings, ["Recorre la tabla completa"]);
        assert!(raw(&ops).contains("    COLUMN TABLE [A] rows=100 cost=1"));
    }
}

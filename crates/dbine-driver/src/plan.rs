//! Helpers for execution plans ([`crate::Session::explain`]).

use crate::model::{Plan, PlanNode};

/// Build a plan tree from an indented text plan, the format many engines
/// print (`EXPLAIN` in Hive, Databricks, CockroachDB, Teradata, Vertica…):
/// each line an operator, nesting by indentation. Tree-drawing prefixes
/// (`│ ├ └ ─ + - > * •` and the like) are stripped from the operator text.
/// Lines that look like `key: value` under an operator become its props.
/// Several top-level lines get a synthetic "PLAN" root.
pub fn tree_from_indented_text(text: &str) -> PlanNode {
    const DRAW: &[char] = &['│', '├', '└', '─', '|', '+', '-', '>', '*', '•', '└', '┌', '┐', '┘', '┬', '┴', '┼', '`'];
    // (indent, node) stack; the node at each level collects its children.
    let mut stack: Vec<(usize, PlanNode)> = vec![(0, PlanNode { op: "PLAN".into(), ..Default::default() })];
    let fold = |stack: &mut Vec<(usize, PlanNode)>, to: usize| {
        while stack.len() > to {
            let (_, done) = stack.pop().expect("non-empty");
            stack.last_mut().expect("root").1.children.push(done);
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let body_start = line.find(|c: char| !c.is_whitespace() && !DRAW.contains(&c)).unwrap_or(line.len());
        let indent = line[..body_start].chars().count() + 1;
        let body = line[body_start..].trim();
        if body.is_empty() {
            continue;
        }
        // A property of the current operator: "key: value", not marked as an
        // operator (•, ->, *, +-), at or below the operator's column.
        let prefix = &line[..body_start];
        let marked = prefix.contains('•') || prefix.contains("->") || prefix.contains('*') || prefix.contains("+-");
        if stack.len() > 1 && !marked && indent >= stack.last().expect("top").0 {
            if let Some((k, v)) = body.split_once(": ").filter(|(k, _)| !k.contains(' ') || k.len() < 32) {
                if !k.is_empty() && !k.contains('(') {
                    stack.last_mut().expect("top").1.props.push((k.trim().to_string(), v.trim().to_string()));
                    continue;
                }
            }
        }
        let depth = stack.iter().rposition(|(i, _)| *i < indent).map_or(1, |p| p + 1);
        fold(&mut stack, depth);
        stack.push((indent, PlanNode { op: body.to_string(), ..Default::default() }));
    }
    fold(&mut stack, 1);
    let mut root = stack.pop().expect("root").1;
    if root.children.len() == 1 && root.props.is_empty() {
        root = root.children.pop().expect("one child");
    }
    root
}

/// A [`Plan`] for engines that only give text: the tree above, the text as
/// raw.
pub fn plan_from_text(statement: &str, text: &str, actual: bool) -> Plan {
    Plan {
        statement: statement.to_string(),
        root: tree_from_indented_text(text),
        actual,
        raw_format: "text".into(),
        raw: text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_becomes_nesting() {
        let t = "\
Sort
  Sort Key: total
  -> Hash Join
       Hash Cond: (a.id = b.id)
       -> Seq Scan on a
       -> Hash
            -> Seq Scan on b
";
        let root = tree_from_indented_text(t);
        assert_eq!(root.op, "Sort");
        assert_eq!(root.props, vec![("Sort Key".to_string(), "total".to_string())]);
        let join = &root.children[0];
        assert_eq!(join.op, "Hash Join");
        assert_eq!(join.props[0].0, "Hash Cond");
        assert_eq!(join.children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["Seq Scan on a", "Hash"]);
        assert_eq!(join.children[1].children[0].op, "Seq Scan on b");
    }

    #[test]
    fn box_drawing_trees_and_several_roots() {
        let t = "• scan\n│ table: t\n└── • filter\n\nother root";
        let root = tree_from_indented_text(t);
        assert_eq!(root.op, "PLAN");
        assert_eq!(root.children[0].op, "scan");
        assert_eq!(root.children[0].children[0].op, "filter");
        assert_eq!(root.children[1].op, "other root");
    }
}

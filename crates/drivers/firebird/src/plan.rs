//! Firebird plans. The wire client doesn't expose `isc_info_sql_get_plan`,
//! so the explained plan (Firebird 3+) is read from
//! `MON$STATEMENTS.MON$EXPLAINED_PLAN` while the statement is prepared (not
//! executed), in a transaction of its own so the monitoring snapshot is
//! fresh:
//!
//! ```text
//! Select Expression
//!     -> Sort (record length: 84, key length: 12)
//!         -> Nested Loop Join (inner)
//!             -> Table "T" as "A" Full Scan
//! ```

use dbine_driver::plan::tree_from_indented_text;
use dbine_driver::PlanNode;

/// The explained plan's text as a tree, with tables, indexes and warnings.
pub fn tree(text: &str) -> PlanNode {
    let mut root = tree_from_indented_text(text);
    enrich(&mut root);
    root
}

fn enrich(n: &mut PlanNode) {
    // `Table "T" as "A" Full Scan`, `Index "PK" Unique Scan`.
    for kind in ["Table ", "Index ", "Procedure "] {
        if let Some(rest) = n.op.strip_prefix(kind) {
            let mut quoted = rest.split('"').skip(1).step_by(2);
            if let Some(name) = quoted.next() {
                n.object = Some(match quoted.next().filter(|_| rest.contains(" as ")) {
                    Some(alias) if alias != name => format!("{name} ({alias})"),
                    _ => name.to_string(),
                });
                let tail = rest.rsplit('"').next().unwrap_or_default().trim();
                n.op = format!("{}{}", kind, if tail.is_empty() { "" } else { tail }).trim().to_string();
            }
        }
    }
    // `Sort (record length: 84, key length: 12)` → props.
    if let Some((op, args)) = n.op.split_once(" (").filter(|(_, a)| a.ends_with(')') && a.contains(": ")) {
        let args = args.trim_end_matches(')').to_string();
        n.op = op.to_string();
        for a in args.split(", ") {
            if let Some((k, v)) = a.split_once(": ") {
                n.props.push((k.to_string(), v.to_string()));
            }
        }
    }
    if n.op.contains("Full Scan") {
        n.warnings.push("Recorre la tabla completa".into());
    }
    for c in &mut n.children {
        enrich(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explained_plans() {
        let t = "\nSelect Expression\n    -> Sort (record length: 84, key length: 12)\n        -> Nested Loop Join (inner)\n            -> Filter\n                -> Table \"MON$STATEMENTS\" as \"S\" Full Scan\n            -> Filter\n                -> Table \"T\" Access By ID\n                    -> Bitmap\n                        -> Index \"RDB$PRIMARY1\" Unique Scan\n";
        let root = tree(t);
        assert_eq!(root.op, "Select Expression");
        let sort = &root.children[0];
        assert_eq!(sort.op, "Sort");
        assert_eq!(sort.props[0], ("record length".to_string(), "84".to_string()));
        let join = &sort.children[0];
        assert_eq!(join.op, "Nested Loop Join (inner)");
        let scan = &join.children[0].children[0];
        assert_eq!(scan.op, "Table Full Scan");
        assert_eq!(scan.object.as_deref(), Some("MON$STATEMENTS (S)"));
        assert_eq!(scan.warnings.len(), 1);
        let by_id = &join.children[1].children[0];
        assert_eq!(by_id.object.as_deref(), Some("T"));
        assert_eq!(by_id.op, "Table Access By ID");
        assert_eq!(by_id.children[0].children[0].object.as_deref(), Some("RDB$PRIMARY1"));
    }
}

//! Execution plans as operator trees:
//! - Neo4j: the `plan` / `profile` map in the result summary of `EXPLAIN` /
//!   `PROFILE` (operator, arguments, estimated rows, db hits, rows, time).
//! - Memgraph: the text rows `EXPLAIN` (`QUERY PLAN`) and `PROFILE`
//!   (`OPERATOR`, `ACTUAL HITS`, `RELATIVE TIME`, `ABSOLUTE TIME`) return,
//!   drawn as ` * Op` lines with `|\` branches.
//! - Neptune: the box-drawn table `explain=static|dynamic` returns (one per
//!   subquery), linked by the `Out #1` / `Out #2` columns.

use crate::packstream::Value;
use crate::value::to_json;
use dbine_driver::{Plan, PlanNode};

fn text(v: &Value) -> String {
    match to_json(v) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// A Neo4j plan / profile map.
pub fn neo4j_node(v: &Value, actual: bool) -> PlanNode {
    let op = v.get("operatorType").and_then(Value::as_str).unwrap_or("?");
    let op = op.split('@').next().unwrap_or(op).to_string();
    let args = v.get("args");
    let arg = |k: &str| args.and_then(|a| a.get(k));
    let mut node = PlanNode {
        op,
        detail: arg("Details").map(text).unwrap_or_default(),
        est_rows: arg("EstimatedRows").and_then(Value::as_f64),
        ..Default::default()
    };
    if actual {
        node.actual_rows = v.get("rows").and_then(Value::as_f64);
        node.self_cost = v.get("dbHits").and_then(Value::as_f64);
        // `time` is in nanoseconds (only with the pipelined/slotted runtimes' timing).
        node.actual_ms = v.get("time").and_then(Value::as_f64).filter(|t| *t > 0.0).map(|t| t / 1e6);
        for (k, label) in [("dbHits", "DB hits"), ("pageCacheHits", "Page cache hits"), ("pageCacheMisses", "Page cache misses")] {
            if let Some(x) = v.get(k).and_then(Value::as_i64) {
                node.props.push((label.into(), x.to_string()));
            }
        }
    }
    if let Some(Value::Map(m)) = args {
        for (k, x) in m {
            if k != "Details" {
                node.props.push((k.clone(), text(x)));
            }
        }
    }
    let ids: Vec<String> = v.get("identifiers").map(Value::as_list).unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
    if !ids.is_empty() {
        node.props.push(("Identificadores".into(), ids.join(", ")));
    }
    node.children = v.get("children").map(Value::as_list).unwrap_or_default().iter().map(|c| neo4j_node(c, actual)).collect();
    if actual {
        let below: f64 = node.children.iter().filter_map(|c| c.total_cost).sum();
        node.total_cost = Some(node.self_cost.unwrap_or(0.0) + below);
    }
    node
}

pub fn neo4j(statement: &str, summary: &Value) -> Option<Plan> {
    let (p, actual) = match (summary.get("profile"), summary.get("plan")) {
        (Some(p), _) => (p, true),
        (None, Some(p)) => (p, false),
        _ => return None,
    };
    Some(Plan {
        statement: statement.to_string(),
        root: neo4j_node(p, actual),
        actual,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&to_json(p)).unwrap_or_default(),
    })
}

/// Memgraph's drawn plan. Each row: the operator text (` * Expand (a)-[r]->(b)`
/// or `|\`) plus, for PROFILE, `(hits, relative %, absolute ms)`.
pub fn memgraph(statement: &str, rows: &[(String, Option<(f64, f64, f64)>)], actual: bool) -> Plan {
    // Arena of nodes; `last[col]` is the latest node drawn at that column.
    let mut nodes: Vec<(PlanNode, Option<usize>)> = Vec::new();
    let mut last: Vec<(usize, usize)> = Vec::new();
    for (line, figures) in rows {
        let Some(star) = line.find('*') else {
            if let Some(col) = line.find('\\') {
                last.retain(|(c, _)| *c < col);
            }
            continue;
        };
        let col = line[..star].chars().count();
        let parent = last.iter().rev().find(|(c, _)| *c == col).or_else(|| last.iter().rev().find(|(c, _)| *c < col)).map(|x| x.1);
        let body = line[star + 1..].trim();
        let (op, detail) = match body.split_once(' ') {
            Some((o, d)) => (o.to_string(), d.trim().to_string()),
            None => (body.to_string(), String::new()),
        };
        let mut n = PlanNode { op, detail, ..Default::default() };
        if let Some((hits, rel, ms)) = figures {
            n.actual_rows = Some(*hits);
            n.actual_ms = Some(*ms);
            n.self_cost = Some(*rel);
            n.props.push(("Tiempo relativo".into(), format!("{rel:.2} %")));
        }
        nodes.push((n, parent));
        let idx = nodes.len() - 1;
        last.retain(|(c, _)| *c < col);
        last.push((col, idx));
    }
    // Attach children to parents, deepest first.
    let mut children: Vec<Vec<PlanNode>> = vec![Vec::new(); nodes.len()];
    let mut root = None;
    for i in (0..nodes.len()).rev() {
        let (mut n, parent) = std::mem::take(&mut nodes[i]);
        let mut kids = std::mem::take(&mut children[i]);
        kids.reverse();
        n.children = kids;
        if actual {
            let below: f64 = n.children.iter().filter_map(|c| c.total_cost).sum();
            n.total_cost = Some(n.self_cost.unwrap_or(0.0) + below);
        }
        match parent {
            Some(p) => children[p].push(n),
            None => root = Some(n),
        }
    }
    let raw = rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n");
    Plan {
        statement: statement.to_string(),
        root: root.unwrap_or_else(|| PlanNode { op: "PLAN".into(), ..Default::default() }),
        actual,
        raw_format: "text".into(),
        raw,
    }
}

/// `"  3.48 %"` / `" 0.002 ms"` → number.
pub fn leading_number(s: &str) -> Option<f64> {
    s.trim().split_whitespace().next()?.parse().ok()
}

/// Neptune's `explain` text (static or dynamic).
pub fn neptune(statement: &str, text: &str, actual: bool) -> Plan {
    struct Row {
        id: String,
        outs: Vec<String>,
        cells: Vec<(String, String)>,
    }
    // (section name, rows)
    let mut tables: Vec<(String, Vec<Row>)> = Vec::new();
    let mut header: Vec<String> = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('╔') || t.starts_with('╚') || t.starts_with('╟') || t.starts_with('╠') {
            continue;
        }
        if !t.starts_with('║') {
            section = t.to_string();
            continue;
        }
        let cells: Vec<String> = t.trim_matches('║').split('│').map(|c| c.trim().to_string()).collect();
        if cells.first().map(String::as_str) == Some("ID") {
            header = cells;
            tables.push((std::mem::take(&mut section), Vec::new()));
            continue;
        }
        let Some((_, rows)) = tables.last_mut() else { continue };
        let get = |name: &str| header.iter().position(|h| h == name).and_then(|i| cells.get(i)).cloned().unwrap_or_default();
        if get("ID").is_empty() {
            // Wrapped arguments of the row above.
            if let Some(r) = rows.last_mut() {
                for (i, h) in header.iter().enumerate() {
                    if let (Some(c), Some(slot)) = (cells.get(i).filter(|c| !c.is_empty()), r.cells.iter_mut().find(|x| &x.0 == h)) {
                        slot.1.push(' ');
                        slot.1.push_str(c);
                    }
                }
            }
            continue;
        }
        rows.push(Row {
            id: get("ID"),
            outs: [get("Out #1"), get("Out #2")].into_iter().filter(|o| !o.is_empty() && o != "-").collect(),
            cells: header.iter().cloned().zip(cells.iter().cloned()).collect(),
        });
    }
    fn build(id: &str, rows: &[Row], subs: &[(String, PlanNode)], actual: bool, depth: usize) -> PlanNode {
        let r = rows.iter().find(|r| r.id == id).expect("row");
        let cell = |k: &str| r.cells.iter().find(|c| c.0 == k).map(|c| c.1.clone()).unwrap_or_default();
        let num = |k: &str| cell(k).parse::<f64>().ok();
        let args = cell("Arguments");
        let mut n = PlanNode { op: cell("Name"), detail: args.clone(), ..Default::default() };
        if actual {
            n.actual_rows = num("Units Out");
            n.actual_ms = num("Time (ms)");
            n.self_cost = n.actual_ms;
        }
        for (k, v) in &r.cells {
            if !matches!(k.as_str(), "ID" | "Out #1" | "Out #2" | "Name" | "Arguments") && v != "-" && !v.is_empty() {
                n.props.push((k.clone(), v.clone()));
            }
        }
        if depth < 200 {
            n.children = rows.iter().filter(|c| c.outs.contains(&r.id)).map(|c| build(&c.id, rows, subs, actual, depth + 1)).collect();
        }
        for (name, sub) in subs {
            if args.contains(&format!("subQuery={name}")) || args.contains(&format!("={name}")) {
                n.children.push(sub.clone());
            }
        }
        if actual {
            let below: f64 = n.children.iter().filter_map(|c| c.total_cost).sum();
            n.total_cost = Some(n.self_cost.unwrap_or(0.0) + below);
        }
        n
    }
    fn tree(rows: &[Row], subs: &[(String, PlanNode)], actual: bool) -> PlanNode {
        let roots: Vec<&Row> = rows.iter().filter(|r| r.outs.is_empty()).collect();
        match roots.as_slice() {
            [one] => build(&one.id, rows, subs, actual, 0),
            _ => PlanNode {
                op: "PLAN".into(),
                children: roots.iter().map(|r| build(&r.id, rows, subs, actual, 0)).collect(),
                ..Default::default()
            },
        }
    }
    // Subqueries first (they're listed after the main table).
    let mut subs: Vec<(String, PlanNode)> = Vec::new();
    for (name, rows) in tables.iter().skip(1).rev() {
        let t = tree(rows, &subs, actual);
        subs.push((name.clone(), t));
    }
    let root = match tables.first() {
        Some((_, rows)) if !rows.is_empty() => tree(rows, &subs, actual),
        _ => dbine_driver::plan::tree_from_indented_text(text),
    };
    Plan { statement: statement.to_string(), root, actual, raw_format: "text".into(), raw: text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packstream::map;

    #[test]
    fn neo4j_profile() {
        let leaf = map([
            ("operatorType", "NodeByLabelScan@neo4j".into()),
            ("args", map([("EstimatedRows", Value::Float(10.0)), ("Details", "n:Person".into())])),
            ("identifiers", Value::List(vec!["n".into()])),
            ("dbHits", Value::Int(11)),
            ("rows", Value::Int(10)),
            ("children", Value::List(vec![])),
        ]);
        let root = map([
            ("operatorType", "ProduceResults@neo4j".into()),
            ("args", map([("planner", "COST".into())])),
            ("dbHits", Value::Int(0)),
            ("rows", Value::Int(10)),
            ("time", Value::Int(2_000_000)),
            ("children", Value::List(vec![leaf])),
        ]);
        let p = neo4j("MATCH (n:Person) RETURN n", &map([("profile", root)])).unwrap();
        assert!(p.actual);
        assert_eq!(p.root.op, "ProduceResults");
        assert_eq!(p.root.actual_ms, Some(2.0));
        assert_eq!(p.root.total_cost, Some(11.0));
        let c = &p.root.children[0];
        assert_eq!((c.op.as_str(), c.detail.as_str(), c.est_rows, c.actual_rows), ("NodeByLabelScan", "n:Person", Some(10.0), Some(10.0)));
        assert!(neo4j("x", &map([])).is_none());
    }

    #[test]
    fn memgraph_branches() {
        let rows: Vec<(String, Option<(f64, f64, f64)>)> =
            [" * Produce {a, b}", " * Cartesian {a : b}", " |\\ ", " | * ScanAll (b)", " | * Once", " * ScanAll (a)", " * Once"]
                .iter()
                .map(|l| (l.to_string(), None))
                .collect();
        let p = memgraph("q", &rows, false);
        assert_eq!(p.root.op, "Produce");
        let cart = &p.root.children[0];
        assert_eq!(cart.op, "Cartesian");
        assert_eq!(cart.children.len(), 2);
        assert_eq!(cart.children[0].detail, "(b)");
        assert_eq!(cart.children[0].children[0].op, "Once");
        assert_eq!(cart.children[1].detail, "(a)");
        assert_eq!(cart.children[1].children[0].op, "Once");
        let prof = vec![("* Produce {n}".to_string(), Some((2.0, 10.0, 0.5))), ("* ScanAll (n)".to_string(), Some((3.0, 90.0, 1.0)))];
        let p = memgraph("q", &prof, true);
        assert_eq!(p.root.total_cost, Some(100.0));
        assert_eq!(p.root.children[0].actual_rows, Some(3.0));
        assert_eq!(leading_number("  3.481894 %"), Some(3.481894));
    }

    #[test]
    fn neptune_table() {
        let t = "\
Query:
MATCH (n) RETURN n LIMIT 1

╔════╤════════╤════════╤═══════════════════╤════════════════════╤══════╤══════════╤═══════════╤═══════╤═══════════╗
║ ID │ Out #1 │ Out #2 │ Name              │ Arguments          │ Mode │ Units In │ Units Out │ Ratio │ Time (ms) ║
╠════╪════════╪════════╪═══════════════════╪════════════════════╪══════╪══════════╪═══════════╪═══════╪═══════════╣
║ 0  │ 1      │ -      │ SolutionInjection │ solutions=[{}]     │ -    │ 0        │ 1         │ 0.00  │ 0         ║
╟────┼────────┼────────┼───────────────────┼────────────────────┼──────┼──────────┼───────────┼───────┼───────────╢
║ 1  │ 2      │ -      │ DFESubquery       │ subQuery=subQuery1 │ -    │ 0        │ 1         │ 0.00  │ 4.00      ║
║    │        │        │                   │ extra=1            │      │          │           │       │           ║
╟────┼────────┼────────┼───────────────────┼────────────────────┼──────┼──────────┼───────────┼───────┼───────────╢
║ 2  │ -      │ -      │ TermResolution    │ vars=[?n]          │ -    │ 1        │ 1         │ 1.00  │ 1.00      ║
╚════╧════════╧════════╧═══════════════════╧════════════════════╧══════╧══════════╧═══════════╧═══════╧═══════════╝

subQuery1
╔════╤════════╤════════╤═══════════╤═══════════╤══════╤══════════╤═══════════╤═══════╤═══════════╗
║ ID │ Out #1 │ Out #2 │ Name      │ Arguments │ Mode │ Units In │ Units Out │ Ratio │ Time (ms) ║
╠════╪════════╪════════╪═══════════╪═══════════╪══════╪══════════╪═══════════╪═══════╪═══════════╣
║ 0  │ 1      │ -      │ DFEScan   │ n         │ -    │ 0        │ 5         │ 0.00  │ 2.00      ║
║ 1  │ -      │ -      │ DFELimit  │ limit=1   │ -    │ 5        │ 1         │ 0.20  │ 0.50      ║
╚════╧════════╧════════╧═══════════╧═══════════╧══════╧══════════╧═══════════╧═══════╧═══════════╝
";
        let p = neptune("q", t, true);
        assert_eq!(p.root.op, "TermResolution");
        let sub = &p.root.children[0];
        assert_eq!(sub.op, "DFESubquery");
        assert_eq!(sub.detail, "subQuery=subQuery1 extra=1");
        assert_eq!(sub.children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["SolutionInjection", "DFELimit"]);
        assert_eq!(sub.children[1].children[0].op, "DFEScan");
        assert_eq!(p.root.actual_rows, Some(1.0));
    }
}

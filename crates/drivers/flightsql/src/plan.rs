//! JSON plans as DuckDB gives them (`EXPLAIN (FORMAT JSON)` and
//! `EXPLAIN (ANALYZE, FORMAT JSON)`): operators with `name` /
//! `operator_name`, `children`, `extra_info` and, when analyzed,
//! `operator_cardinality` and `operator_timing` (seconds).

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_start_matches('~').replace(',', "").split_whitespace().next()?.parse().ok(),
        _ => None,
    }
}

fn node(v: &Value) -> PlanNode {
    let name = ["name", "operator_name", "operator_type"].iter().find_map(|k| v.get(*k)).map(text).unwrap_or_default();
    let mut n = PlanNode { op: name.trim().to_string(), ..Default::default() };
    match v.get("extra_info") {
        Some(Value::Object(m)) => {
            for (k, val) in m {
                match k.as_str() {
                    "Estimated Cardinality" => n.est_rows = number(val),
                    "Table" => n.object = Some(text(val)),
                    _ => {}
                }
                let shown = match val {
                    Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(", "),
                    v => text(v),
                };
                if n.detail.is_empty() && matches!(k.as_str(), "Table" | "Join Type" | "Conditions" | "Aggregates" | "Filters" | "Projections") {
                    n.detail = shown.clone();
                }
                n.props.push((k.clone(), shown));
            }
        }
        Some(Value::String(s)) if !s.is_empty() => n.detail = s.trim().to_string(),
        _ => {}
    }
    if let Some(c) = v.get("operator_cardinality").and_then(number) {
        n.actual_rows = Some(c);
    }
    if let Some(t) = v.get("operator_timing").and_then(number) {
        n.actual_ms = Some(t * 1000.0);
    }
    n.children = v.get("children").and_then(Value::as_array).map(|c| c.iter().map(node).collect()).unwrap_or_default();
    n
}

pub fn from_json(stmt: &str, v: &Value, actual: bool) -> Plan {
    let mut roots: Vec<PlanNode> = match v {
        Value::Array(a) => a.iter().map(node).collect(),
        other => vec![node(other)],
    };
    // ANALYZE wraps the tree in an unnamed query node.
    while roots.len() == 1 && roots[0].op.is_empty() && roots[0].children.len() == 1 {
        let mut r = roots.remove(0);
        roots.push(r.children.remove(0));
    }
    let root = if roots.len() == 1 { roots.remove(0) } else { PlanNode { op: "QUERY".into(), children: roots, ..Default::default() } };
    Plan { statement: stmt.to_string(), root, actual, raw_format: "json".into(), raw: serde_json::to_string_pretty(v).unwrap_or_default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duckdb_trees() {
        let est = json!([{"name": "PROJECTION", "children": [{"name": "SEQ_SCAN ", "children": [], "extra_info": {"Table": "t", "Projections": ["a"], "Estimated Cardinality": "3"}}], "extra_info": {"Projections": "a"}}]);
        let p = from_json("q", &est, false);
        assert_eq!(p.root.op, "PROJECTION");
        let scan = &p.root.children[0];
        assert_eq!((scan.op.as_str(), scan.object.as_deref(), scan.est_rows), ("SEQ_SCAN", Some("t"), Some(3.0)));
        let act = json!({"latency": 0.01, "children": [{"operator_name": "PROJECTION", "operator_cardinality": 3, "operator_timing": 0.002, "children": []}]});
        let p = from_json("q", &act, true);
        assert_eq!((p.root.op.as_str(), p.root.actual_rows, p.root.actual_ms), ("PROJECTION", Some(3.0), Some(2.0)));
    }
}

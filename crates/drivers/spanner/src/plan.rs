//! Spanner query plans (`executeSql` with `queryMode: PLAN` / `PROFILE`)
//! as [`PlanNode`] trees.
//!
//! `stats.queryPlan.planNodes` is a flat list; each node links to its
//! children by index (`childLinks[].childIndex`, with an optional `type`
//! such as "Split Range" or "Seek Condition" and a `variable`). Relational
//! nodes are operators; scalar nodes (expressions) become props of the
//! relational node that uses them. With PROFILE each operator carries
//! `executionStats` (rows, latency, cpu_time, execution_summary) and the
//! response `stats.queryStats` the totals.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).filter(|n| n.is_finite())
}

/// `{"total": "1.36", "unit": "msecs"}` → milliseconds.
fn stat_ms(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    let n = num(v.get("total"))?;
    Some(
        n * match v.get("unit").and_then(Value::as_str).unwrap_or("msecs") {
            "usecs" => 1e-3,
            "secs" => 1000.0,
            "mins" => 60_000.0,
            _ => 1.0,
        },
    )
}

/// `"672.001us"`, `"1.2 msecs"`, `"0.05 secs"` (queryStats) → milliseconds.
fn duration_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let n: f64 = s[..split].parse().ok()?;
    Some(
        n * match s[split..].trim() {
            "us" | "usecs" | "µs" => 1e-3,
            "ms" | "msecs" => 1.0,
            "s" | "secs" => 1000.0,
            _ => return None,
        },
    )
}

/// The plan of one `executeSql` response (`None` when it has no
/// `stats.queryPlan`).
pub(crate) fn from_response(statement: &str, r: &Value, actual: bool) -> Option<Plan> {
    let plan = r.pointer("/stats/queryPlan")?;
    let nodes = plan.get("planNodes").and_then(Value::as_array)?;
    let mut root = if nodes.is_empty() { PlanNode { op: "Query".into(), ..Default::default() } } else { node(nodes, 0, 0) };
    if nodes.len() == 1 && nodes[0].get("displayName").and_then(Value::as_str) == Some("No query plan") {
        root.warnings.push("El servidor no devolvió un plan (el emulador de Spanner no genera planes)".into());
    }
    // Query totals (PROFILE): elapsed_time, cpu_time, rows_scanned…
    if let Some(Value::Object(qs)) = r.pointer("/stats/queryStats") {
        let mut top = Vec::new();
        for (k, v) in qs {
            let v = text(v);
            if k == "elapsed_time" && root.actual_ms.is_none() {
                root.actual_ms = duration_ms(&v);
            }
            if k == "rows_returned" && root.actual_rows.is_none() {
                root.actual_rows = v.parse().ok();
            }
            top.push((k.clone(), v));
        }
        root.props.splice(0..0, top);
    }
    if let Some(n) = r.pointer("/stats/rowCountExact").and_then(|v| num(Some(v))) {
        root.props.insert(0, ("Filas modificadas".into(), format!("{n}")));
    }
    Some(Plan {
        statement: statement.into(),
        root,
        actual,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(plan).unwrap_or_default(),
    })
}

fn is_scalar(n: &Value) -> bool {
    n.get("kind").and_then(Value::as_str) == Some("SCALAR")
}

/// The node at `i` (by its `index`, which is also its position).
fn at(nodes: &[Value], i: usize) -> Option<&Value> {
    nodes.iter().find(|n| n.get("index").and_then(|v| num(Some(v))).unwrap_or(0.0) as usize == i).or_else(|| nodes.get(i))
}

/// A scalar node as text: its short representation, or its name with the
/// scalar nodes under it.
fn scalar_text(nodes: &[Value], i: usize, depth: usize) -> String {
    let Some(n) = at(nodes, i) else { return String::new() };
    if let Some(d) = n.pointer("/shortRepresentation/description").and_then(Value::as_str) {
        return d.to_string();
    }
    let name = n.get("displayName").map(text).unwrap_or_default();
    if depth > 20 {
        return name;
    }
    let args: Vec<String> = links(n).iter().map(|(c, _, _)| scalar_text(nodes, *c, depth + 1)).collect();
    if args.is_empty() {
        name
    } else {
        format!("{name}({})", args.join(", "))
    }
}

/// `(childIndex, type, variable)` of a node's links.
fn links(n: &Value) -> Vec<(usize, String, String)> {
    n.get("childLinks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let i = num(l.get("childIndex"))? as usize;
                    Some((i, l.get("type").map(text).unwrap_or_default(), l.get("variable").map(text).unwrap_or_default()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn node(nodes: &[Value], i: usize, depth: usize) -> PlanNode {
    let Some(v) = at(nodes, i) else { return PlanNode { op: "?".into(), ..Default::default() } };
    let meta = v.get("metadata").and_then(Value::as_object);
    let m = |k: &str| meta.and_then(|m| m.get(k)).map(text).unwrap_or_default();
    let mut n = PlanNode { op: v.get("displayName").map(text).unwrap_or_default(), ..Default::default() };

    let mut detail = Vec::new();
    for k in ["scan_type", "call_type", "join_type", "iterator_type", "aggregation_type", "execution_method"] {
        if !m(k).is_empty() {
            detail.push(m(k));
        }
    }
    n.detail = detail.join(" · ");
    for k in ["scan_target", "distribution_table", "table"] {
        if !m(k).is_empty() {
            n.object = Some(m(k));
            break;
        }
    }
    if m("Full scan") == "true" {
        n.warnings.push(format!("Recorrido completo de {}", n.object.as_deref().unwrap_or("la tabla")));
    }
    if let Some(meta) = meta {
        n.props.extend(meta.iter().map(|(k, v)| (k.clone(), text(v))));
    }

    if let Some(stats) = v.get("executionStats") {
        n.actual_rows = num(stats.pointer("/rows/total"));
        n.actual_ms = stat_ms(stats.get("latency"));
        n.executions = num(stats.pointer("/execution_summary/num_executions"));
        if let Some(cpu) = stat_ms(stats.get("cpu_time")) {
            n.props.push(("CPU (ms)".into(), format!("{cpu}")));
        }
        for (label, key) in [("Filas leídas", "scanned_rows"), ("Filas filtradas", "filtered_rows"), ("Bytes devueltos", "returned_bytes")] {
            if let Some(x) = num(stats.pointer(&format!("/{key}/total"))) {
                n.props.push((label.into(), format!("{x}")));
            }
        }
    }

    for (c, ty, var) in links(v) {
        let Some(child) = at(nodes, c) else { continue };
        if is_scalar(child) || depth > 200 {
            let label = match (ty.is_empty(), var.is_empty()) {
                (false, false) => format!("{ty} ({var})"),
                (false, true) => ty,
                (true, false) => var,
                (true, true) => child.get("displayName").map(text).unwrap_or_default(),
            };
            let t = scalar_text(nodes, c, 0);
            if !t.is_empty() {
                n.props.push((label, t));
            }
        } else {
            let mut k = node(nodes, c, depth + 1);
            if !ty.is_empty() {
                k.props.insert(0, ("Enlace".into(), ty));
            }
            n.children.push(k);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PROFILE response of `SELECT s.FirstName, a.AlbumTitle FROM Singers s
    /// JOIN Albums a ON s.SingerId = a.SingerId WHERE a.MarketingBudget > 1000`
    /// (shape as Cloud Spanner returns it).
    const PROFILE: &str = r#"{
 "metadata": {"rowType": {"fields": [{"name": "FirstName", "type": {"code": "STRING"}}]}},
 "rows": [["Marc"]],
 "stats": {
  "queryPlan": {"planNodes": [
   {"displayName": "Distributed Union", "kind": "RELATIONAL",
    "childLinks": [{"childIndex": 1}, {"childIndex": 12, "type": "Split Range"}],
    "metadata": {"subquery_cluster_node": "1", "distribution_table": "Singers"},
    "executionStats": {"latency": {"total": "3.2", "unit": "msecs"}, "rows": {"total": "1", "unit": "rows"},
                       "cpu_time": {"total": "2.9", "unit": "msecs"}, "execution_summary": {"num_executions": "1"}}},
   {"index": 1, "displayName": "Serialize Result", "kind": "RELATIONAL",
    "childLinks": [{"childIndex": 2}, {"childIndex": 11}],
    "executionStats": {"rows": {"total": "1", "unit": "rows"}, "latency": {"total": "3", "unit": "msecs"}}},
   {"index": 2, "displayName": "Cross Apply", "kind": "RELATIONAL",
    "childLinks": [{"childIndex": 3, "type": "Input"}, {"childIndex": 7, "type": "Map"}],
    "executionStats": {"rows": {"total": "1", "unit": "rows"}, "latency": {"total": "2.5", "unit": "msecs"}}},
   {"index": 3, "displayName": "Scan", "kind": "RELATIONAL",
    "childLinks": [{"childIndex": 4, "variable": "SingerId"}, {"childIndex": 5, "variable": "FirstName"}],
    "metadata": {"Full scan": "true", "scan_target": "Singers", "scan_type": "TableScan", "scan_method": "Automatic"},
    "executionStats": {"rows": {"total": "5", "unit": "rows"}, "latency": {"total": "0.8", "unit": "msecs"},
                       "scanned_rows": {"total": "5", "unit": "rows"}, "execution_summary": {"num_executions": "1"}}},
   {"index": 4, "displayName": "Reference", "kind": "SCALAR", "shortRepresentation": {"description": "SingerId"}},
   {"index": 5, "displayName": "Reference", "kind": "SCALAR", "shortRepresentation": {"description": "FirstName"}},
   {"index": 6, "displayName": "Constant", "kind": "SCALAR", "shortRepresentation": {"description": "1000"}},
   {"index": 7, "displayName": "Filter Scan", "kind": "RELATIONAL",
    "childLinks": [{"childIndex": 8}, {"childIndex": 9, "type": "Seek Condition"}],
    "executionStats": {"rows": {"total": "1", "unit": "rows"}, "latency": {"total": "1.1", "unit": "msecs"},
                       "execution_summary": {"num_executions": "5"}}},
   {"index": 8, "displayName": "Scan", "kind": "RELATIONAL",
    "metadata": {"scan_target": "AlbumsByBudget", "scan_type": "IndexScan", "scan_method": "Row"},
    "executionStats": {"rows": {"total": "1", "unit": "rows"}, "latency": {"total": "0.9", "unit": "usecs"},
                       "execution_summary": {"num_executions": "5"}}},
   {"index": 9, "displayName": "Function", "kind": "SCALAR", "childLinks": [{"childIndex": 10}, {"childIndex": 6}],
    "shortRepresentation": {"description": "($MarketingBudget > 1000)"}},
   {"index": 10, "displayName": "Reference", "kind": "SCALAR", "shortRepresentation": {"description": "$MarketingBudget"}},
   {"index": 11, "displayName": "Reference", "kind": "SCALAR", "shortRepresentation": {"description": "$FirstName"}},
   {"index": 12, "displayName": "Constant", "kind": "SCALAR", "shortRepresentation": {"description": "true"}}
  ]},
  "queryStats": {"elapsed_time": "4.12 msecs", "cpu_time": "3.5 msecs", "rows_returned": "1", "rows_scanned": "6",
                 "query_plan_creation_time": "0.9 msecs"}
 }
}"#;

    #[test]
    fn profile_tree() {
        let r: Value = serde_json::from_str(PROFILE).unwrap();
        let p = from_response("q", &r, true).unwrap();
        assert!(p.actual);
        let root = &p.root;
        assert_eq!(root.op, "Distributed Union");
        assert_eq!(root.object.as_deref(), Some("Singers"));
        assert!(root.props.iter().any(|(k, v)| k == "Split Range" && v == "true"));
        assert!(root.props.iter().any(|(k, v)| k == "rows_scanned" && v == "6"));
        assert_eq!((root.actual_rows, root.actual_ms, root.executions), (Some(1.0), Some(3.2), Some(1.0)));
        let apply = &root.children[0].children[0];
        assert_eq!(apply.op, "Cross Apply");
        let [scan, filter] = &apply.children[..] else { panic!("{apply:#?}") };
        assert_eq!((scan.detail.as_str(), scan.object.as_deref()), ("TableScan", Some("Singers")));
        assert!(scan.warnings.iter().any(|w| w.starts_with("Recorrido completo")));
        assert!(scan.props.iter().any(|(k, v)| k == "SingerId" && v == "SingerId"));
        assert!(scan.props.iter().any(|(k, v)| k == "Filas leídas" && v == "5"));
        assert_eq!(filter.props[0], ("Enlace".into(), "Map".into()));
        assert!(filter.props.iter().any(|(k, v)| k == "Seek Condition" && v == "($MarketingBudget > 1000)"));
        assert_eq!(filter.executions, Some(5.0));
        let idx = &filter.children[0];
        assert_eq!((idx.detail.as_str(), idx.object.as_deref()), ("IndexScan", Some("AlbumsByBudget")));
        assert!((idx.actual_ms.unwrap() - 0.0009).abs() < 1e-12);
    }

    #[test]
    fn emulator_has_no_plan() {
        let r: Value = serde_json::from_str(
            r#"{"stats":{"queryPlan":{"planNodes":[{"displayName":"No query plan"}]},"queryStats":{"elapsed_time":"672.001us","rows_returned":"0"}}}"#,
        )
        .unwrap();
        let p = from_response("q", &r, true).unwrap();
        assert_eq!(p.root.op, "No query plan");
        assert!(!p.root.warnings.is_empty());
        assert!((p.root.actual_ms.unwrap() - 0.672001).abs() < 1e-9);
        assert_eq!(p.root.actual_rows, Some(0.0));
        assert!(from_response("q", &serde_json::json!({"rows": []}), false).is_none());
    }
}

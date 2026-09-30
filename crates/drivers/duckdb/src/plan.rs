//! DuckDB's `EXPLAIN (FORMAT JSON)` and `EXPLAIN (ANALYZE, FORMAT JSON)`
//! as [`PlanNode`] trees. DuckDB reports no costs; estimates come as
//! `Estimated Cardinality`, measurements as `operator_cardinality` and
//! `operator_timing` (seconds).

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StmtKind {
    /// Only reads: running it again to measure it is harmless.
    Read,
    /// Writes, but EXPLAIN can plan it without running it.
    Write,
    /// No plan (DDL, SET, COPY…).
    Other,
}

pub(crate) fn classify(stmt: &str) -> StmtKind {
    let words = words(stmt);
    match words.first().map(String::as_str) {
        Some("select" | "with" | "from" | "values" | "table" | "pivot" | "unpivot") => {
            if words.iter().any(|w| matches!(w.as_str(), "insert" | "update" | "delete" | "merge")) {
                StmtKind::Write
            } else {
                StmtKind::Read
            }
        }
        Some("insert" | "update" | "delete" | "merge") => StmtKind::Write,
        _ => StmtKind::Other,
    }
}

fn words(stmt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in stmt.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
        }
        if c.is_alphanumeric() || c == '_' {
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The statement, cut to a line for messages.
pub(crate) fn short(stmt: &str) -> String {
    let one: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(60).collect::<String>())
    } else {
        one
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

fn estimate_warning(est: f64, act: f64) -> Option<String> {
    let (lo, hi) = if est < act { (est, act) } else { (act, est) };
    (hi - lo >= 100.0 && hi >= 10.0 * lo.max(1.0))
        .then(|| format!("Estimación de filas errada: {} estimadas, {} reales", fmt_num(est), fmt_num(act)))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
}

/// The JSON of either form into a plan. Estimated plans are an array of
/// operators; analyzed ones an object with query metrics whose child is
/// an `EXPLAIN_ANALYZE` wrapper around the real root.
pub(crate) fn duck_json(statement: &str, raw: &str, actual: bool) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let root = match &v {
        Value::Array(items) => match items.as_slice() {
            [one] => node(one),
            many => PlanNode { op: "PLAN".into(), children: many.iter().map(node).collect(), ..Default::default() },
        },
        Value::Object(m) => {
            let mut top = m.get("children").and_then(Value::as_array).and_then(|c| c.first()).ok_or("plan sin operadores")?;
            // Skip the EXPLAIN_ANALYZE operator itself.
            while top.get("operator_type").and_then(Value::as_str) == Some("EXPLAIN_ANALYZE") {
                match top.get("children").and_then(Value::as_array).and_then(|c| c.first()) {
                    Some(c) => top = c,
                    None => break,
                }
            }
            let mut root = node(top);
            for (k, val) in m {
                if !matches!(k.as_str(), "children" | "extra_info" | "query_name") && !val.is_object() {
                    root.props.push((format!("query {k}"), text(val)));
                }
            }
            if let Some(latency) = num(m.get("latency")) {
                root.props.push(("Tiempo total (ms)".into(), fmt_num(latency * 1000.0)));
            }
            root
        }
        _ => return Err("plan JSON inesperado".into()),
    };
    Ok(Plan { statement: statement.into(), root, actual, raw_format: "json".into(), raw: raw.trim().into() })
}

fn node(v: &Value) -> PlanNode {
    let extra = v.get("extra_info").cloned().unwrap_or(Value::Null);
    let e = |k: &str| extra.get(k).map(text).filter(|s| !s.is_empty());
    let op = v.get("name").or_else(|| v.get("operator_name")).map(text).unwrap_or_default().trim().to_string();
    let mut detail = Vec::new();
    if let Some(j) = e("Join Type") {
        detail.push(j);
    }
    if let Some(t) = e("Type").filter(|t| t != "Sequential Scan") {
        detail.push(t);
    }
    let est_rows = num(extra.get("Estimated Cardinality"));
    let actual_rows = num(v.get("operator_cardinality"));
    let mut n = PlanNode {
        op,
        detail: detail.join(" · "),
        object: e("Table").or_else(|| e("Function")),
        est_rows,
        actual_rows,
        actual_ms: num(v.get("operator_timing")).map(|s| s * 1000.0),
        ..Default::default()
    };
    if let (Some(e), Some(a)) = (est_rows, actual_rows) {
        n.warnings.extend(estimate_warning(e, a));
    }
    if let Value::Object(m) = &extra {
        n.props.extend(m.iter().map(|(k, v)| (k.clone(), text(v))));
    }
    if let Value::Object(m) = v {
        for (k, val) in m {
            if !matches!(k.as_str(), "children" | "extra_info" | "name" | "operator_name") && !val.is_object() {
                n.props.push((k.clone(), text(val)));
            }
        }
    }
    n.children = v.get("children").and_then(Value::as_array).map(|c| c.iter().map(node).collect()).unwrap_or_default();
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESTIMATED: &str = r##"[
    {
        "name": "HASH_GROUP_BY",
        "children": [
            {
                "name": "HASH_JOIN",
                "children": [
                    {"name": "SEQ_SCAN ", "children": [], "extra_info": {"Table": "memory.main.a", "Type": "Sequential Scan",
                      "Projections": ["id", "g"], "Filters": "g<5", "Estimated Cardinality": "40000"}},
                    {"name": "SEQ_SCAN ", "children": [], "extra_info": {"Table": "memory.main.b", "Type": "Sequential Scan",
                      "Projections": "a_id", "Estimated Cardinality": "5000"}}
                ],
                "extra_info": {"Join Type": "INNER", "Conditions": "id = a_id", "Estimated Cardinality": "1290"}
            }
        ],
        "extra_info": {"Groups": "#0", "Aggregates": "count_star()"}
    }
]"##;

    #[test]
    fn estimated_tree() {
        let p = duck_json("q", ESTIMATED, false).unwrap();
        assert_eq!(p.root.op, "HASH_GROUP_BY");
        let join = &p.root.children[0];
        assert_eq!((join.op.as_str(), join.detail.as_str()), ("HASH_JOIN", "INNER"));
        assert_eq!(join.est_rows, Some(1290.0));
        let a = &join.children[0];
        assert_eq!((a.op.as_str(), a.object.as_deref()), ("SEQ_SCAN", Some("memory.main.a")));
        assert!(a.props.iter().any(|(k, v)| k == "Projections" && v == "id, g"));
        assert_eq!(a.actual_rows, None);
    }

    const ANALYZED: &str = r#"{
    "latency": 0.00519925, "rows_returned": 5, "extra_info": {}, "query_name": "EXPLAIN ...",
    "children": [
        {
            "operator_type": "EXPLAIN_ANALYZE", "operator_name": "EXPLAIN_ANALYZE", "operator_timing": 3.33e-7,
            "extra_info": {}, "operator_cardinality": 0,
            "children": [
                {
                    "operator_type": "HASH_JOIN", "operator_name": "HASH_JOIN", "operator_timing": 0.000999377,
                    "extra_info": {"Join Type": "INNER", "Conditions": "id = a_id", "Estimated Cardinality": "12"},
                    "operator_cardinality": 2500, "operator_rows_scanned": 0,
                    "children": [
                        {"operator_type": "TABLE_SCAN", "operator_name": "SEQ_SCAN ", "operator_timing": 0.001368875,
                         "operator_rows_scanned": 96256, "operator_cardinality": 500,
                         "extra_info": {"Table": "memory.main.a", "Type": "Sequential Scan", "Estimated Cardinality": "40000"},
                         "children": []}
                    ]
                }
            ]
        }
    ]
}"#;

    #[test]
    fn analyzed_tree() {
        let p = duck_json("q", ANALYZED, true).unwrap();
        let root = &p.root;
        assert_eq!(root.op, "HASH_JOIN");
        assert_eq!(root.actual_rows, Some(2500.0));
        assert!((root.actual_ms.unwrap() - 0.999377).abs() < 1e-9);
        assert!(root.warnings.iter().any(|w| w.starts_with("Estimación")));
        assert!(root.props.iter().any(|(k, v)| k == "Tiempo total (ms)" && v == "5.20"));
        let scan = &root.children[0];
        assert_eq!(scan.actual_rows, Some(500.0));
        assert!(scan.props.iter().any(|(k, v)| k == "operator_rows_scanned" && v == "96256"));
        assert!(scan.warnings.iter().any(|w| w.starts_with("Estimación")));
    }

    #[test]
    fn statement_kinds() {
        assert_eq!(classify("FROM t SELECT a"), StmtKind::Read);
        assert_eq!(classify("delete from t"), StmtKind::Write);
        assert_eq!(classify("create table t as select 1"), StmtKind::Other);
    }
}

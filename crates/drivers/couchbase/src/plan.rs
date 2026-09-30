//! SQL++ plans: the JSON of `EXPLAIN` (`plan`) and, when the server
//! returns it (profiling is an Enterprise feature), the
//! `profile.executionTimings` of a run, the same operator tree with
//! `#stats` (items in / out, times).
//!
//! Operators are objects with `#operator`. A `Sequence` runs its
//! `~children` as a pipeline (each feeds the next), so it becomes a chain
//! whose last operator is the root; `Parallel` just wraps `~child`; joins
//! and nests take the pipeline so far plus their `~child` (the inner side);
//! unions list their branches in `~children`.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

/// Go duration text (`1.2ms`, `83.833µs`, `1m20.3s`, `2h1m`) in ms.
pub fn duration_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_digit() || n == '.' {
                break;
            }
            unit.push(n);
            chars.next();
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        total += v * match unit.as_str() {
            "h" => 3_600_000.0,
            "m" => 60_000.0,
            "s" => 1000.0,
            "ms" => 1.0,
            "µs" | "us" | "μs" => 0.001,
            "ns" => 0.000_001,
            _ => return None,
        };
        any = true;
    }
    if !num.is_empty() {
        return None;
    }
    any.then_some(total)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// One operator (without its inputs).
fn node_of(op: &Value) -> PlanNode {
    let name = op.get("#operator").and_then(Value::as_str).unwrap_or("?").to_string();
    let mut n = PlanNode { op: name, ..Default::default() };
    if let Some(ks) = op.get("keyspace").and_then(Value::as_str) {
        n.object = Some(match (op.get("bucket").and_then(Value::as_str), op.get("scope").and_then(Value::as_str)) {
            (Some(b), Some(s)) => format!("{b}.{s}.{ks}"),
            _ => ks.to_string(),
        });
    }
    n.detail = ["index", "condition", "on_clause", "on_keys", "as"]
        .iter()
        .find_map(|k| op.get(*k).map(text))
        .unwrap_or_default();
    if let Some(est) = op.get("optimizer_estimates") {
        n.total_cost = est.get("cost").and_then(Value::as_f64);
        n.est_rows = est.get("cardinality").and_then(Value::as_f64);
    }
    if let Some(st) = op.get("#stats") {
        n.actual_rows = st.get("#itemsOut").and_then(Value::as_f64);
        let t = |k: &str| st.get(k).and_then(Value::as_str).and_then(duration_ms);
        n.actual_ms = match (t("execTime"), t("servTime")) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        for (k, v) in st.as_object().into_iter().flatten() {
            n.props.push((k.clone(), text(v)));
        }
    }
    for (k, v) in op.as_object().into_iter().flatten() {
        if k.starts_with('#') || k.starts_with('~') || k == "optimizer_estimates" {
            continue;
        }
        n.props.push((k.clone(), text(v)));
    }
    n
}

/// The node for `op`, given what feeds it in a pipeline (`input`).
fn convert(op: &Value, input: Option<PlanNode>) -> Option<PlanNode> {
    let name = op.get("#operator").and_then(Value::as_str).unwrap_or("");
    match name {
        "Sequence" => {
            let mut acc = input;
            for c in op.get("~children").and_then(Value::as_array).into_iter().flatten() {
                acc = convert(c, acc);
            }
            acc
        }
        "Parallel" => match op.get("~child") {
            Some(c) => convert(c, input),
            None => input,
        },
        _ => {
            let mut n = node_of(op);
            n.children.extend(input);
            if let Some(c) = op.get("~child").and_then(|c| convert(c, None)) {
                n.children.push(c);
            }
            if name != "Sequence" {
                for c in op.get("~children").and_then(Value::as_array).into_iter().flatten() {
                    n.children.extend(convert(c, None));
                }
            }
            Some(n)
        }
    }
}

pub fn from_json(stmt: &str, plan: &Value, actual: bool) -> Plan {
    let root = convert(plan, None).unwrap_or_default();
    Plan {
        statement: stmt.to_string(),
        root,
        actual,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(plan).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations() {
        assert_eq!(duration_ms("1.5ms"), Some(1.5));
        assert_eq!(duration_ms("83.833µs"), Some(0.083833));
        assert_eq!(duration_ms("1m20.5s"), Some(80_500.0));
        assert_eq!(duration_ms("x"), None);
        assert_eq!(duration_ms("12"), None);
    }

    #[test]
    fn sequence_becomes_a_chain() {
        let plan = json!({"#operator":"Sequence","~children":[
            {"#operator":"PrimaryScan3","bucket":"b","scope":"_default","keyspace":"c1","index":"#sequentialscan",
             "#stats":{"#itemsOut":2,"execTime":"1ms","servTime":"0.5ms"}},
            {"#operator":"Fetch","keyspace":"c1"},
            {"#operator":"Parallel","~child":{"#operator":"Sequence","~children":[
                {"#operator":"Filter","condition":"((`c1`.`a`) = 1)","optimizer_estimates":{"cost":5.5,"cardinality":1.0}},
                {"#operator":"InitialProject"}]}},
            {"#operator":"NestedLoopJoin","on_clause":"x","~child":{"#operator":"Sequence","~children":[{"#operator":"IndexScan3","keyspace":"c2"}]}}
        ]});
        let p = from_json("q", &plan, true);
        let r = &p.root;
        assert_eq!(r.op, "NestedLoopJoin");
        assert_eq!(r.children.len(), 2);
        assert_eq!(r.children[1].op, "IndexScan3");
        let project = &r.children[0];
        assert_eq!(project.op, "InitialProject");
        let filter = &project.children[0];
        assert_eq!((filter.op.as_str(), filter.total_cost, filter.est_rows), ("Filter", Some(5.5), Some(1.0)));
        let scan = &filter.children[0].children[0];
        assert_eq!(scan.object.as_deref(), Some("b._default.c1"));
        assert_eq!((scan.actual_rows, scan.actual_ms), (Some(2.0), Some(1.5)));
    }
}

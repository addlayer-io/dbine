//! OrientDB execution plans: the `executionPlan` document of `EXPLAIN` /
//! `PROFILE` (`steps`, each with `name`, `description`, `cost`,
//! `subSteps`). Steps run top to bottom, each feeding the next, so the
//! tree's root is the last step and each step's child is the one before it;
//! `subSteps` (clusters of a class scan, UNION branches…) hang under their
//! step. `PROFILE` costs are nanoseconds.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn step(v: &Value, actual: bool) -> PlanNode {
    let desc = text(v.get("description"));
    let first = desc.lines().next().unwrap_or_default().trim().trim_start_matches('+').trim().to_string();
    let mut n = PlanNode {
        op: text(v.get("name")).trim_end_matches("ExecutionStep").trim_end_matches("Step").to_string(),
        detail: first,
        ..Default::default()
    };
    if n.op.is_empty() {
        n.op = text(v.get("type"));
    }
    let cost = v.get("cost").and_then(Value::as_f64).filter(|c| *c >= 0.0);
    if actual {
        n.self_cost = cost;
        n.actual_ms = cost.map(|ns| ns / 1e6);
    }
    if desc.lines().count() > 1 {
        n.props.push(("Descripción".into(), desc.trim().to_string()));
    }
    if let Some(t) = v.get("targetNode").map(|t| text(Some(t))).filter(|t| !t.is_empty() && *t != text(v.get("name"))) {
        n.props.push(("Destino".into(), t));
    }
    n.children = chain(v.get("subSteps").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), actual, true);
    n
}

/// Steps as a chain (each the child of the next), or as siblings for
/// sub-steps that don't feed each other (a class scan's clusters).
fn chain(steps: &[Value], actual: bool, siblings: bool) -> Vec<PlanNode> {
    if siblings {
        return steps.iter().map(|s| step(s, actual)).collect();
    }
    let mut below: Option<PlanNode> = None;
    for s in steps {
        let mut n = step(s, actual);
        if let Some(b) = below.take() {
            n.children.insert(0, b);
        }
        below = Some(n);
    }
    below.into_iter().collect()
}

fn totals(n: &mut PlanNode) -> f64 {
    let below: f64 = n.children.iter_mut().map(totals).sum();
    let t = n.self_cost.unwrap_or(0.0) + below;
    n.total_cost = Some(t);
    t
}

pub fn from_execution_plan(statement: &str, plan: &Value, actual: bool) -> Plan {
    let steps = plan.get("steps").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut root = chain(steps, actual, false).pop().unwrap_or_else(|| PlanNode { op: "PLAN".into(), ..Default::default() });
    if actual {
        totals(&mut root);
    }
    let raw = text(plan.get("prettyPrint"));
    Plan { statement: statement.to_string(), root, actual, raw_format: "text".into(), raw }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_become_a_chain() {
        let p = json!({
            "prettyPrint": "+ FETCH FROM CLASS P\n+ FILTER ITEMS WHERE\n  a = 1",
            "steps": [
                { "name": "FetchFromClassExecutionStep", "cost": 10, "description": "+ FETCH FROM CLASS P\n  + FETCH FROM CLUSTER 9 ASC",
                  "subSteps": [{ "name": "FetchFromClusterExecutionStep", "cost": 4, "description": "+ FETCH FROM CLUSTER 9 ASC", "subSteps": [] },
                               { "name": "FetchFromClusterExecutionStep", "cost": 5, "description": "+ FETCH FROM CLUSTER 10 ASC", "subSteps": [] }] },
                { "name": "FilterStep", "cost": 2000000, "description": "+ FILTER ITEMS WHERE \n  a = 1", "subSteps": [] }
            ]
        });
        let plan = from_execution_plan("SELECT FROM P WHERE a = 1", &p, true);
        assert_eq!(plan.root.op, "Filter");
        assert_eq!(plan.root.actual_ms, Some(2.0));
        let fetch = &plan.root.children[0];
        assert_eq!((fetch.op.as_str(), fetch.detail.as_str()), ("FetchFromClass", "FETCH FROM CLASS P"));
        assert_eq!(fetch.children.len(), 2);
        assert_eq!(plan.root.total_cost, Some(2_000_019.0));
        let est = from_execution_plan("q", &p, false);
        assert!(est.root.total_cost.is_none() && !est.actual);
    }
}

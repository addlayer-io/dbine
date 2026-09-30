//! Snowflake plans as [`PlanNode`] trees.
//!
//! - Estimated: `EXPLAIN USING JSON` (compiled, not run; no warehouse).
//!   `Operations` holds one list of operators per step; each operator names
//!   its parents (`parentOperators`), so children are found by reversing
//!   that. Snowflake gives no costs or row estimates, only the micro-
//!   partitions and bytes each scan is assigned after pruning.
//! - Actual: the script runs, then `GET_QUERY_OPERATOR_STATS('<query id>')`
//!   gives every operator's measured statistics (rows in / out, pruning,
//!   spilling, share of the execution time). The share of time goes to
//!   `self_cost`, so the UI's cost percentages are time percentages.

use dbine_driver::{Plan, PlanNode};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StmtKind {
    /// A query or DML: EXPLAIN can plan it.
    Plannable,
    /// DDL, USE, SET, SHOW…: no plan.
    Other,
}

pub(crate) fn classify(stmt: &str) -> StmtKind {
    let words: Vec<String> = stmt
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(64)
        .map(str::to_ascii_lowercase)
        .collect();
    match words.first().map(String::as_str) {
        Some("select" | "with" | "insert" | "update" | "delete" | "merge") => StmtKind::Plannable,
        Some("create") if words.iter().any(|w| w == "as") && words.iter().any(|w| w == "table") => StmtKind::Plannable,
        _ => StmtKind::Other,
    }
}

pub(crate) fn short(stmt: &str) -> String {
    let one: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(60).collect::<String>())
    } else {
        one
    }
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
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).filter(|n| n.is_finite())
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

/// VARIANT / ARRAY cells come back from the SQL API as JSON text.
fn parsed(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn ids(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| num(Some(x)).map(|n| n as i64)).collect(),
        Value::Null => Vec::new(),
        other => num(Some(other)).map(|n| vec![n as i64]).unwrap_or_default(),
    }
}

/// Hangs every operator from its (first) parent; operators without one
/// are the roots, in id order.
fn assemble(mut nodes: BTreeMap<i64, (Vec<i64>, PlanNode)>) -> Vec<PlanNode> {
    // Children before parents: deepest ids first isn't guaranteed, so
    // repeatedly move leaves (nodes nobody names as parent) up.
    loop {
        let parents: std::collections::BTreeSet<i64> = nodes.values().flat_map(|(p, _)| p.first().copied()).collect();
        let leaf = nodes
            .iter()
            .find(|(id, (p, _))| !parents.contains(id) && p.first().is_some_and(|pid| nodes.contains_key(pid)))
            .map(|(id, _)| *id);
        let Some(id) = leaf else { break };
        let (p, n) = nodes.remove(&id).expect("present");
        let parent = nodes.get_mut(&p[0]).expect("parent present");
        parent.1.children.insert(0, n);
    }
    // Children were inserted from the highest id down; restore id order.
    let mut roots: Vec<PlanNode> = nodes.into_values().map(|(_, n)| n).collect();
    for r in &mut roots {
        sort_children(r);
    }
    roots
}

fn sort_children(n: &mut PlanNode) {
    n.children.sort_by_key(|c| op_id(c));
    for c in &mut n.children {
        sort_children(c);
    }
}

fn op_id(n: &PlanNode) -> i64 {
    n.props.iter().find(|(k, _)| k == "Id").and_then(|(_, v)| v.parse().ok()).unwrap_or(i64::MAX)
}

/// Several roots (steps) under a synthetic "Query" node.
fn single_root(mut roots: Vec<PlanNode>, label: &str) -> PlanNode {
    if roots.len() == 1 {
        roots.pop().expect("one")
    } else {
        PlanNode { op: label.into(), children: roots, ..Default::default() }
    }
}

fn pruning_warning(assigned: Option<f64>, total: Option<f64>) -> Option<String> {
    match (assigned, total) {
        (Some(a), Some(t)) if t > 10.0 && a >= t => Some(format!("Sin poda de particiones: lee las {} micro-particiones", fmt_num(t))),
        _ => None,
    }
}

// ---- EXPLAIN USING JSON --------------------------------------------------

pub(crate) fn explain_json(statement: &str, raw: &str) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw.trim()).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let ops = v.get("Operations").and_then(Value::as_array).ok_or("el plan JSON no tiene \"Operations\"")?;
    let mut roots = Vec::new();
    for (step, group) in ops.iter().enumerate() {
        let list = group.as_array().cloned().unwrap_or_else(|| vec![group.clone()]);
        let nodes: BTreeMap<i64, (Vec<i64>, PlanNode)> = list
            .iter()
            .map(|o| (num(o.get("id")).unwrap_or(0.0) as i64, (ids(o.get("parentOperators").unwrap_or(&Value::Null)), explain_node(o))))
            .collect();
        for mut r in assemble(nodes) {
            if ops.len() > 1 {
                r.props.insert(0, ("Paso".into(), (step + 1).to_string()));
            }
            roots.push(r);
        }
    }
    let mut root = single_root(roots, "Query");
    if let Some(Value::Object(g)) = v.get("GlobalStats") {
        let mut top: Vec<(String, String)> = g.iter().map(|(k, v)| (k.clone(), text(v))).collect();
        top.sort();
        root.props.splice(0..0, top);
        root.warnings.extend(pruning_warning(num(g.get("partitionsAssigned")), num(g.get("partitionsTotal"))));
    }
    Ok(Plan { statement: statement.into(), root, actual: false, raw_format: "json".into(), raw: raw.trim().into() })
}

fn explain_node(o: &Value) -> PlanNode {
    let mut n = PlanNode { op: o.get("operation").map(text).unwrap_or_default(), ..Default::default() };
    n.props.push(("Id".into(), o.get("id").map(text).unwrap_or_default()));
    if let Some(Value::Array(objs)) = o.get("objects") {
        n.object = Some(objs.iter().map(text).collect::<Vec<_>>().join(", "));
    }
    if let Some(Value::Array(ex)) = o.get("expressions") {
        let e: Vec<String> = ex.iter().map(text).collect();
        if !e.is_empty() && !n.op.contains("Scan") {
            n.detail = e.join(", ");
        }
        n.props.push(("Expresiones".into(), e.join(", ")));
    }
    if let Value::Object(m) = o {
        for (k, v) in m {
            if !matches!(k.as_str(), "id" | "operation" | "objects" | "expressions" | "parentOperators") {
                n.props.push((k.clone(), text(v)));
            }
        }
    }
    n.warnings.extend(pruning_warning(num(o.get("partitionsAssigned")), num(o.get("partitionsTotal"))));
    n
}

// ---- GET_QUERY_OPERATOR_STATS ---------------------------------------------

/// A measured plan from GET_QUERY_OPERATOR_STATS rows (column name, in
/// lower case → cell). `None` when there are none (DDL…).
pub(crate) fn operator_stats(statement: &str, query_id: &str, rows: &[Map<String, Value>]) -> Option<Plan> {
    if rows.is_empty() {
        return None;
    }
    let mut steps: BTreeMap<i64, BTreeMap<i64, (Vec<i64>, PlanNode)>> = BTreeMap::new();
    for r in rows {
        let step = num(r.get("step_id")).unwrap_or(1.0) as i64;
        let id = num(r.get("operator_id")).unwrap_or(0.0) as i64;
        let parents = ids(&parsed(r.get("parent_operators")));
        steps.entry(step).or_default().insert(id, (parents, stats_node(r)));
    }
    let many = steps.len() > 1;
    let mut roots = Vec::new();
    for (step, nodes) in steps {
        for mut n in assemble(nodes) {
            if many {
                n.props.insert(0, ("Paso".into(), step.to_string()));
            }
            roots.push(n);
        }
    }
    let mut root = single_root(roots, "Query");
    cumulate(&mut root);
    root.props.insert(0, ("Id de consulta".into(), query_id.to_string()));
    let raw = Value::Array(rows.iter().cloned().map(Value::Object).collect());
    Some(Plan {
        statement: statement.into(),
        root,
        actual: true,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&raw).unwrap_or_default(),
    })
}

/// Sums `self_cost` (share of time) up the tree into `total_cost`.
fn cumulate(n: &mut PlanNode) -> Option<f64> {
    let kids: Vec<Option<f64>> = n.children.iter_mut().map(cumulate).collect();
    if n.self_cost.is_none() && kids.iter().all(Option::is_none) {
        return None;
    }
    let t = n.self_cost.unwrap_or(0.0) + kids.into_iter().flatten().sum::<f64>();
    n.total_cost = Some(t);
    Some(t)
}

fn flat(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                flat(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, x, out);
            }
        }
        Value::Null => {}
        other => out.push((prefix.to_string(), text(other))),
    }
}

fn stats_node(r: &Map<String, Value>) -> PlanNode {
    let op = r.get("operator_type").map(text).unwrap_or_default();
    let stats = parsed(r.get("operator_statistics"));
    let time = parsed(r.get("execution_time_breakdown"));
    let attrs = parsed(r.get("operator_attributes"));
    let s = |p: &str| num(stats.pointer(p));
    let mut n = PlanNode { op: op.clone(), ..Default::default() };
    n.props.push(("Id".into(), r.get("operator_id").map(text).unwrap_or_default()));
    n.actual_rows = s("/output_rows");
    n.self_cost = num(time.get("overall_percentage")).map(|p| if p <= 1.0 { p * 100.0 } else { p });
    let a = |k: &str| attrs.get(k).map(text).filter(|v| !v.is_empty());
    n.object = a("table_name").or_else(|| a("table_names"));
    n.detail = [a("join_type"), a("equality_join_condition"), a("filter_condition"), a("grouping_keys"), a("sort_keys")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

    let spilled_local = s("/spilling/bytes_spilled_local_storage").unwrap_or(0.0);
    let spilled_remote = s("/spilling/bytes_spilled_remote_storage").unwrap_or(0.0);
    if spilled_remote > 0.0 {
        n.warnings.push(format!("Derramó {} bytes a almacenamiento remoto", fmt_num(spilled_remote)));
    }
    if spilled_local > 0.0 {
        n.warnings.push(format!("Derramó {} bytes a disco local", fmt_num(spilled_local)));
    }
    if let (Some(i), Some(o)) = (s("/input_rows"), s("/output_rows")) {
        if op.contains("Join") && o > 10_000.0 && o > 10.0 * i.max(1.0) {
            n.warnings.push(format!("Join explosivo: {} filas de salida con {} de entrada", fmt_num(o), fmt_num(i)));
        }
    }
    n.warnings.extend(pruning_warning(s("/pruning/partitions_scanned"), s("/pruning/partitions_total")));

    let mut props = Vec::new();
    flat("", &attrs, &mut props);
    flat("", &stats, &mut props);
    if let Value::Object(t) = &time {
        for (k, v) in t {
            props.push((format!("tiempo.{k}"), text(v)));
        }
    }
    n.props.extend(props);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EXPLAIN USING JSON SELECT c.name, SUM(o.total) FROM orders o JOIN
    /// customers c ON o.cid = c.id WHERE o.day > '2024-01-01' GROUP BY 1`.
    const EXPLAIN: &str = r#"{"GlobalStats":{"partitionsTotal":250,"partitionsAssigned":250,"bytesAssigned":98304000},
"Operations":[[
 {"id":0,"operation":"Result","expressions":["C.NAME","SUM(O.TOTAL)"]},
 {"id":1,"parentOperators":[0],"operation":"Aggregate","expressions":["aggExprs: [SUM(O.TOTAL)]","groupKeys: [C.NAME]"]},
 {"id":2,"parentOperators":[1],"operation":"InnerJoin","expressions":["joinKey: (C.ID = O.CID)"]},
 {"id":3,"parentOperators":[2],"operation":"Filter","expressions":["O.DAY > '2024-01-01'"]},
 {"id":4,"parentOperators":[3],"operation":"TableScan","objects":["SALES.PUBLIC.ORDERS"],"expressions":["CID","TOTAL","DAY"],
  "partitionsAssigned":240,"partitionsTotal":240,"bytesAssigned":94371840},
 {"id":5,"parentOperators":[2],"operation":"JoinFilter","expressions":["joinKey: (C.ID = O.CID)"]},
 {"id":6,"parentOperators":[5],"operation":"TableScan","objects":["SALES.PUBLIC.CUSTOMERS"],"expressions":["ID","NAME"],
  "partitionsAssigned":10,"partitionsTotal":10,"bytesAssigned":3932160}
]]}"#;

    #[test]
    fn explain_tree() {
        let p = explain_json("q", EXPLAIN).unwrap();
        assert!(!p.actual);
        let root = &p.root;
        assert_eq!(root.op, "Result");
        assert!(root.props.iter().any(|(k, v)| k == "bytesAssigned" && v == "98304000"));
        assert!(root.warnings.iter().any(|w| w.starts_with("Sin poda")));
        let join = &root.children[0].children[0];
        assert_eq!((join.op.as_str(), join.detail.as_str()), ("InnerJoin", "joinKey: (C.ID = O.CID)"));
        let [filter, jf] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!(filter.op, "Filter");
        let scan = &filter.children[0];
        assert_eq!(scan.object.as_deref(), Some("SALES.PUBLIC.ORDERS"));
        assert!(scan.warnings.iter().any(|w| w.contains("240")));
        assert!(scan.props.iter().any(|(k, v)| k == "partitionsAssigned" && v == "240"));
        let cust = &jf.children[0];
        assert!(cust.warnings.is_empty(), "10 partitions is too few to warn");
    }

    #[test]
    fn classify_statements() {
        assert_eq!(classify("select 1"), StmtKind::Plannable);
        assert_eq!(classify("MERGE INTO t USING s ON 1=1 WHEN MATCHED THEN DELETE"), StmtKind::Plannable);
        assert_eq!(classify("create table t as select 1"), StmtKind::Plannable);
        assert_eq!(classify("use warehouse w"), StmtKind::Other);
    }

    /// GET_QUERY_OPERATOR_STATS rows as the SQL API returns them (VARIANT
    /// and ARRAY columns as JSON text).
    const STATS: &str = r#"[
 {"query_id":"01b2","step_id":"1","operator_id":"0","parent_operators":null,"operator_type":"Result",
  "operator_statistics":"{\"input_rows\": 12, \"output_rows\": 12}",
  "execution_time_breakdown":"{\"overall_percentage\": 0.02, \"processing\": 1.0}",
  "operator_attributes":"{\"expressions\": [\"C.NAME\", \"SUM(O.TOTAL)\"]}"},
 {"query_id":"01b2","step_id":"1","operator_id":"1","parent_operators":"[0]","operator_type":"Aggregate",
  "operator_statistics":"{\"input_rows\": 5000000, \"output_rows\": 12, \"spilling\": {\"bytes_spilled_local_storage\": 1048576}}",
  "execution_time_breakdown":"{\"overall_percentage\": 0.30}",
  "operator_attributes":"{\"grouping_keys\": [\"C.NAME\"]}"},
 {"query_id":"01b2","step_id":"1","operator_id":"2","parent_operators":"[1]","operator_type":"Join",
  "operator_statistics":"{\"input_rows\": 20000, \"output_rows\": 5000000}",
  "execution_time_breakdown":"{\"overall_percentage\": 0.40}",
  "operator_attributes":"{\"equality_join_condition\": \"(C.ID = O.CID)\", \"join_type\": \"INNER\"}"},
 {"query_id":"01b2","step_id":"1","operator_id":"3","parent_operators":"[2]","operator_type":"TableScan",
  "operator_statistics":"{\"output_rows\": 19000, \"io\": {\"bytes_scanned\": 94371840}, \"pruning\": {\"partitions_scanned\": 240, \"partitions_total\": 240}}",
  "execution_time_breakdown":"{\"overall_percentage\": 0.25}",
  "operator_attributes":"{\"table_name\": \"SALES.PUBLIC.ORDERS\", \"columns\": [\"CID\", \"TOTAL\"]}"},
 {"query_id":"01b2","step_id":"1","operator_id":"4","parent_operators":"[2]","operator_type":"TableScan",
  "operator_statistics":"{\"output_rows\": 1000, \"pruning\": {\"partitions_scanned\": 1, \"partitions_total\": 10}}",
  "execution_time_breakdown":"{\"overall_percentage\": 0.03}",
  "operator_attributes":"{\"table_name\": \"SALES.PUBLIC.CUSTOMERS\"}"}
]"#;

    #[test]
    fn operator_stats_tree() {
        let rows: Vec<Map<String, Value>> = serde_json::from_str(STATS).unwrap();
        let p = operator_stats("q", "01b2", &rows).unwrap();
        assert!(p.actual);
        let root = &p.root;
        assert_eq!(root.op, "Result");
        assert_eq!(root.props[0], ("Id de consulta".into(), "01b2".into()));
        assert!((root.total_cost.unwrap() - 100.0).abs() < 1e-9);
        let agg = &root.children[0];
        assert_eq!(agg.detail, "C.NAME");
        assert!(agg.warnings.iter().any(|w| w.contains("disco local")));
        let join = &agg.children[0];
        assert_eq!(join.detail, "INNER · (C.ID = O.CID)");
        assert!(join.warnings.iter().any(|w| w.starts_with("Join explosivo")));
        assert_eq!(join.self_cost, Some(40.0));
        let [orders, cust] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!((orders.object.as_deref(), orders.actual_rows), (Some("SALES.PUBLIC.ORDERS"), Some(19_000.0)));
        assert!(orders.warnings.iter().any(|w| w.starts_with("Sin poda")));
        assert!(orders.props.iter().any(|(k, v)| k == "io.bytes_scanned" && v == "94371840"));
        assert!(cust.warnings.is_empty());
        assert!(operator_stats("q", "x", &[]).is_none());
    }
}

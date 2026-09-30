//! Athena plans as [`PlanNode`] trees.
//!
//! - Estimated: `EXPLAIN (FORMAT JSON)` (Athena's engine is Trino: one
//!   tree per fragment, stitched together at each `RemoteSource`). The
//!   parser is the trino driver's, copied (this crate can't depend on it).
//! - Actual: the statement runs once and `GetQueryRuntimeStatistics` gives
//!   its stages (rows, bytes, time) with each stage's operator tree; no
//!   second run, so nothing is scanned or billed twice.
//!
//! Trino's cost estimates are per operator, not cumulative: they go to
//! `self_cost` and `total_cost` is summed up the tree.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StmtKind {
    /// Only reads: running it again to measure it is harmless.
    Read,
    /// Writes, but EXPLAIN can plan it without running it.
    Write,
    /// No plan (DDL, SET, USE…).
    Other,
}

pub(crate) fn classify(stmt: &str) -> StmtKind {
    let words = words(stmt);
    match words.first().map(String::as_str) {
        Some("select" | "with" | "values" | "table") => StmtKind::Read,
        Some("insert" | "update" | "delete" | "merge") => StmtKind::Write,
        Some("create") if words.iter().any(|w| w == "as") && words.iter().any(|w| w == "table") => StmtKind::Write,
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

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).filter(|n| n.is_finite())
}

/// Fragment ids in `[1, 2]`.
fn ids(s: &str) -> Vec<String> {
    s.trim_matches(['[', ']', ' ']).split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// Sums `self_cost` up the tree into `total_cost` (when any is known).
fn cumulate(n: &mut PlanNode) -> Option<f64> {
    let children: Vec<Option<f64>> = n.children.iter_mut().map(cumulate).collect();
    if n.self_cost.is_none() && children.iter().all(Option::is_none) {
        return None;
    }
    let total = n.self_cost.unwrap_or(0.0) + children.into_iter().flatten().sum::<f64>();
    n.total_cost = Some(total);
    Some(total)
}

/// Replaces every `RemoteSource` leaf's children with the fragments it
/// reads, recursively.
fn stitch(n: &mut PlanNode, fragments: &mut BTreeMap<String, PlanNode>, sources: &dyn Fn(&PlanNode) -> Vec<String>) {
    for id in sources(n) {
        if let Some(mut f) = fragments.remove(&id) {
            stitch(&mut f, fragments, sources);
            n.children.push(f);
        }
    }
    for c in &mut n.children {
        stitch(c, fragments, sources);
    }
}

fn remote_ids(n: &PlanNode) -> Vec<String> {
    n.props
        .iter()
        .filter(|(k, _)| k == "sourceFragmentIds" || k == "remoteSources")
        .flat_map(|(_, v)| ids(v))
        .collect()
}

// ---- EXPLAIN (FORMAT JSON) ----------------------------------------------

pub(crate) fn plan_json(statement: &str, raw: &str) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let mut root = if v.get("name").is_some() {
        json_node(&v)
    } else {
        let Value::Object(m) = &v else { return Err("plan JSON inesperado".into()) };
        let mut fragments: BTreeMap<String, PlanNode> =
            m.iter().map(|(id, n)| (id.clone(), with_fragment(json_node(n), id))).collect();
        let first = m.keys().next().cloned().ok_or("plan vacío")?;
        let mut root = fragments.remove(&first).expect("first fragment");
        stitch(&mut root, &mut fragments, &remote_ids);
        root
    };
    cumulate(&mut root);
    Ok(Plan { statement: statement.into(), root, actual: false, raw_format: "json".into(), raw: raw.trim().into() })
}

fn with_fragment(mut n: PlanNode, id: &str) -> PlanNode {
    n.props.insert(0, ("Fragmento".into(), id.to_string()));
    n
}

fn json_node(v: &Value) -> PlanNode {
    let s = |k: &str| v.get(k).map(text).unwrap_or_default();
    let mut n = PlanNode { op: s("name"), ..Default::default() };
    let mut detail = Vec::new();
    match v.get("descriptor") {
        Some(Value::Object(d)) => {
            for (k, val) in d {
                let val = text(val);
                if k == "table" {
                    n.object = Some(val.clone());
                } else if matches!(k.as_str(), "type" | "criteria" | "orderBy" | "partitioning" | "count" | "keys") {
                    detail.push(if k == "type" || k == "criteria" { val.clone() } else { format!("{k} = {val}") });
                }
                n.props.push((k.clone(), val));
            }
        }
        _ => {
            // Presto: a one-line `identifier`.
            let id = s("identifier");
            if !id.is_empty() {
                detail.push(id.trim_matches(['[', ']']).to_string());
            }
        }
    }
    n.detail = detail.join(" · ");
    if let Some(Value::Array(est)) = v.get("estimates") {
        if let Some(last) = est.last() {
            n.est_rows = num(last.get("outputRowCount"));
            n.self_cost = num(last.get("cpuCost"));
            for k in ["outputSizeInBytes", "cpuCost", "memoryCost", "networkCost"] {
                if let Some(x) = num(last.get(k)) {
                    n.props.push((k.into(), fmt_num(x)));
                }
            }
        }
    }
    if let Some(Value::Array(details)) = v.get("details") {
        n.props.extend(details.iter().map(|d| (String::new(), text(d))));
    }
    if let Some(Value::Array(outputs)) = v.get("outputs") {
        let layout: Vec<String> =
            outputs.iter().map(|o| format!("{}:{}", text(o.get("name").unwrap_or(&Value::Null)), text(o.get("type").unwrap_or(&Value::Null)))).collect();
        n.props.push(("Layout".into(), layout.join(", ")));
    }
    if let Some(Value::Array(r)) = v.get("remoteSources") {
        n.props.push(("remoteSources".into(), format!("[{}]", r.iter().map(text).collect::<Vec<_>>().join(", "))));
    }
    n.children = v.get("children").and_then(Value::as_array).map(|c| c.iter().map(json_node).collect()).unwrap_or_default();
    n
}

/// The first cell of every row of an EXPLAIN result, one per line,
/// without the header row Athena may repeat.
pub(crate) fn explain_text(rows: &[Vec<Option<String>>]) -> String {
    rows.iter()
        .filter_map(|r| r.first().cloned().flatten())
        .filter(|l| l != "Query Plan")
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- GetQueryRuntimeStatistics ------------------------------------------

/// A measured plan from `GetQueryRuntimeStatistics` (as the API's JSON:
/// `Timeline`, `Rows`, `OutputStage`) plus the execution's own statistics
/// (`DataScannedInBytes`…) as root props. `None` when Athena gave no
/// stages (DDL, or statistics not available).
pub(crate) fn runtime_plan(statement: &str, stats: &Value, props: Vec<(String, String)>) -> Option<Plan> {
    let stage = stats.get("OutputStage").filter(|s| s.get("QueryStagePlan").is_some_and(|p| !p.is_null()))?;
    let mut root = stage_tree(stage);
    let mut top = props;
    for (label, group, key) in [
        ("Filas leídas", "Rows", "InputRows"),
        ("Bytes leídos", "Rows", "InputBytes"),
        ("Filas devueltas", "Rows", "OutputRows"),
        ("Bytes devueltos", "Rows", "OutputBytes"),
        ("En cola (ms)", "Timeline", "QueryQueueTimeInMillis"),
        ("Planificación (ms)", "Timeline", "QueryPlanningTimeInMillis"),
        ("Ejecución del motor (ms)", "Timeline", "EngineExecutionTimeInMillis"),
        ("Tiempo total (ms)", "Timeline", "TotalExecutionTimeInMillis"),
    ] {
        if let Some(n) = num(stats.get(group).and_then(|g| g.get(key))) {
            top.push((label.into(), fmt_num(n)));
        }
    }
    root.props.splice(0..0, top);
    Some(Plan {
        statement: statement.into(),
        root,
        actual: true,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(stats).unwrap_or_default(),
    })
}

/// A stage: its operator tree, with the stage's figures on its top
/// operator and the sub-stages hung from the `RemoteSource`s that read
/// them (or from the top operator when no source names them).
fn stage_tree(stage: &Value) -> PlanNode {
    let mut subs: BTreeMap<String, PlanNode> = stage
        .get("SubStages")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|s| (num(s.get("StageId")).map(fmt_num).unwrap_or_default(), stage_tree(s))).collect())
        .unwrap_or_default();
    let mut n = stage.get("QueryStagePlan").filter(|p| !p.is_null()).map(stage_op).unwrap_or_else(|| PlanNode { op: "Stage".into(), ..Default::default() });
    stitch(&mut n, &mut subs, &remote_ids);
    n.children.extend(subs.into_values());
    n.actual_rows = num(stage.get("OutputRows"));
    n.actual_ms = num(stage.get("ExecutionTime"));
    let mut props = Vec::new();
    if let Some(id) = num(stage.get("StageId")) {
        props.push(("Etapa".to_string(), fmt_num(id)));
    }
    if let Some(s) = stage.get("State").and_then(Value::as_str) {
        props.push(("Estado".into(), s.to_string()));
    }
    for (label, key) in [("Filas de entrada", "InputRows"), ("Bytes de entrada", "InputBytes"), ("Bytes de salida", "OutputBytes")] {
        if let Some(v) = num(stage.get(key)) {
            props.push((label.into(), fmt_num(v)));
        }
    }
    n.props.splice(0..0, props);
    n
}

/// A `QueryStagePlan` node: `Name`, `Identifier`, `Children`,
/// `RemoteSources` (the stage ids it reads).
fn stage_op(v: &Value) -> PlanNode {
    let name = v.get("Name").map(text).unwrap_or_default();
    let id = v.get("Identifier").map(text).unwrap_or_default();
    let mut n = PlanNode { op: name.clone(), ..Default::default() };
    if !id.is_empty() {
        if name.contains("Scan") || name == "TableWriter" {
            n.object = Some(id.trim_matches(['[', ']']).to_string());
        } else {
            n.detail = id.trim_matches(['[', ']']).to_string();
        }
        n.props.push(("Identificador".into(), id));
    }
    if let Some(Value::Array(r)) = v.get("RemoteSources") {
        n.props.push(("remoteSources".into(), format!("[{}]", r.iter().map(text).collect::<Vec<_>>().join(", "))));
    }
    n.children = v.get("Children").and_then(Value::as_array).map(|c| c.iter().map(stage_op).collect()).unwrap_or_default();
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
  "0" : {
    "id" : "20", "name" : "Output", "descriptor" : { "columnNames" : "[name, _col1]" },
    "outputs" : [ { "type" : "varchar(25)", "name" : "name" } ], "details" : [ "_col1 := count" ],
    "estimates" : [ { "outputRowCount" : 5.0, "outputSizeInBytes" : 105.4, "cpuCost" : 0.0, "memoryCost" : 0.0, "networkCost" : 0.0 } ],
    "children" : [ {
      "id" : "363", "name" : "RemoteSource", "descriptor" : { "sourceFragmentIds" : "[1]" },
      "outputs" : [ ], "details" : [ ], "estimates" : [ ], "children" : [ ]
    } ]
  },
  "1" : {
    "id" : "1", "name" : "InnerJoin", "descriptor" : { "criteria" : "(nationkey_2 = nationkey)", "distribution" : "REPLICATED" },
    "outputs" : [ ], "details" : [ "Distribution: REPLICATED" ],
    "estimates" : [ { "outputRowCount" : 1364.13, "outputSizeInBytes" : 28000.0, "cpuCost" : 53837.2, "memoryCost" : 527.0, "networkCost" : 0.0 } ],
    "children" : [
      { "id" : "2", "name" : "ScanFilterProject", "descriptor" : { "table" : "tpch:tiny:customer", "filterPredicate" : "(0.0 < acctbal)" },
        "outputs" : [ ], "details" : [ ],
        "estimates" : [ { "outputRowCount" : 1500.0, "cpuCost" : 27000.0 }, { "outputRowCount" : 1364.13, "cpuCost" : 12277.2 } ],
        "children" : [ ] },
      { "id" : "3", "name" : "RemoteSource", "descriptor" : { "sourceFragmentIds" : "[2]" },
        "outputs" : [ ], "details" : [ ], "estimates" : [ ], "children" : [ ] }
    ]
  },
  "2" : {
    "id" : "9", "name" : "TableScan", "descriptor" : { "table" : "tpch:tiny:nation" },
    "outputs" : [ ], "details" : [ ], "estimates" : [ { "outputRowCount" : 25.0, "cpuCost" : 527.0 } ], "children" : [ ]
  }
}"#;

    #[test]
    fn json_fragments_are_stitched() {
        let p = plan_json("q", JSON).unwrap();
        let root = &p.root;
        assert_eq!(root.op, "Output");
        assert!(root.props.iter().any(|(k, v)| k.is_empty() && v == "_col1 := count"));
        let remote = &root.children[0];
        assert_eq!(remote.op, "RemoteSource");
        let join = &remote.children[0];
        assert_eq!((join.op.as_str(), join.detail.as_str()), ("InnerJoin", "(nationkey_2 = nationkey)"));
        assert_eq!(join.self_cost, Some(53837.2));
        let scan = &join.children[0];
        assert_eq!(scan.object.as_deref(), Some("tpch:tiny:customer"));
        assert_eq!((scan.est_rows, scan.self_cost), (Some(1364.13), Some(12277.2)));
        let nation = &join.children[1].children[0];
        assert_eq!(nation.op, "TableScan");
        assert!((root.total_cost.unwrap() - (53837.2 + 12277.2 + 527.0)).abs() < 1e-6);
        assert!(!p.actual);
    }

    /// GetQueryRuntimeStatistics' response, as the API returns it.
    const RUNTIME: &str = r#"{
  "Timeline": { "QueryQueueTimeInMillis": 104, "QueryPlanningTimeInMillis": 312, "EngineExecutionTimeInMillis": 1450,
                "ServiceProcessingTimeInMillis": 40, "TotalExecutionTimeInMillis": 1594 },
  "Rows": { "InputRows": 15000, "InputBytes": 2400000, "OutputBytes": 180, "OutputRows": 5 },
  "OutputStage": {
    "StageId": 0, "State": "FINISHED", "OutputBytes": 180, "OutputRows": 5, "InputBytes": 190, "InputRows": 5,
    "ExecutionTime": 12,
    "QueryStagePlan": { "Name": "Output", "Identifier": "[name, _col1]", "Children": [
      { "Name": "TopN", "Identifier": "[5 by (count DESC_NULLS_LAST)]", "Children": [
        { "Name": "RemoteSource", "Identifier": "", "Children": [], "RemoteSources": ["1"] } ], "RemoteSources": [] } ],
      "RemoteSources": [] },
    "SubStages": [ {
      "StageId": 1, "State": "FINISHED", "OutputBytes": 190, "OutputRows": 5, "InputBytes": 2400000, "InputRows": 15000,
      "ExecutionTime": 1320,
      "QueryStagePlan": { "Name": "Aggregate", "Identifier": "[name]", "Children": [
        { "Name": "ScanFilterProject", "Identifier": "[awsdatacatalog:sales:orders]", "Children": [], "RemoteSources": [] } ],
        "RemoteSources": [] },
      "SubStages": []
    } ]
  }
}"#;

    #[test]
    fn runtime_statistics_tree() {
        let v: Value = serde_json::from_str(RUNTIME).unwrap();
        let p = runtime_plan("q", &v, vec![("Datos escaneados (bytes)".into(), "2400000".into())]).unwrap();
        assert!(p.actual);
        let root = &p.root;
        assert_eq!(root.op, "Output");
        assert_eq!(root.props[0].0, "Datos escaneados (bytes)");
        assert!(root.props.iter().any(|(k, v)| k == "Tiempo total (ms)" && v == "1594"));
        assert_eq!((root.actual_rows, root.actual_ms), (Some(5.0), Some(12.0)));
        let remote = &root.children[0].children[0];
        assert_eq!(remote.op, "RemoteSource");
        let agg = &remote.children[0];
        assert_eq!((agg.op.as_str(), agg.detail.as_str()), ("Aggregate", "name"));
        assert_eq!(agg.actual_ms, Some(1320.0));
        assert!(agg.props.iter().any(|(k, v)| k == "Etapa" && v == "1"));
        assert_eq!(agg.children[0].object.as_deref(), Some("awsdatacatalog:sales:orders"));
        assert!(runtime_plan("q", &serde_json::json!({"Timeline": {}}), vec![]).is_none());
        assert!(runtime_plan("q", &serde_json::json!({"OutputStage": {"QueryStagePlan": null}}), vec![]).is_none());
    }

    #[test]
    fn explain_rows_drop_the_header() {
        let rows = vec![vec![Some("Query Plan".to_string())], vec![Some("{".into())], vec![Some("}".into())]];
        assert_eq!(explain_text(&rows), "{\n}");
    }

    #[test]
    fn statement_kinds() {
        assert_eq!(classify("SELECT 1"), StmtKind::Read);
        assert_eq!(classify("insert into t select 1"), StmtKind::Write);
        assert_eq!(classify("create table t as select 1"), StmtKind::Write);
        assert_eq!(classify("create table t (a int)"), StmtKind::Other);
    }
}

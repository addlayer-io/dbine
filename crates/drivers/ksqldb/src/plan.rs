//! ksqlDB `EXPLAIN`: the `queryDescription` entity carries the logical
//! execution plan as indented text (` > [ SINK ] | Schema: … | Logger: …`,
//! one tab level per step) and the Kafka Streams topology.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

/// `executionPlan` text → tree (each step's inputs are nested below it).
pub fn execution_tree(text: &str) -> Option<PlanNode> {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let fold = |stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, indent: usize| {
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            let (_, done) = stack.pop().expect("non-empty");
            match stack.last_mut() {
                Some(top) => top.1.children.push(done),
                None => roots.push(done),
            }
        }
    };
    for line in text.lines() {
        let Some(gt) = line.find('>') else { continue };
        if !line[..gt].chars().all(char::is_whitespace) {
            continue;
        }
        let indent = line[..gt].chars().map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
        let mut parts = line[gt + 1..].split(" | ");
        let head = parts.next().unwrap_or_default().trim();
        let op = head.trim_start_matches('[').trim_end_matches(']').trim().to_string();
        let mut n = PlanNode { op, ..Default::default() };
        for p in parts {
            let (k, v) = p.split_once(": ").unwrap_or(("Detalle", p));
            n.props.push((k.trim().to_string(), v.trim().to_string()));
        }
        if let Some((_, logger)) = n.props.iter().find(|(k, _)| k == "Logger") {
            n.detail = logger.rsplit('.').next().unwrap_or_default().to_string();
        }
        fold(&mut stack, &mut roots, indent);
        stack.push((indent, n));
    }
    fold(&mut stack, &mut roots, 0);
    match roots.len() {
        0 => None,
        1 => roots.pop(),
        _ => Some(PlanNode { op: "PLAN".into(), children: roots, ..Default::default() }),
    }
}

/// A `queryDescription` entity → plan (estimated: ksqlDB has no per
/// operator runtime figures over REST).
pub fn from_description(statement: &str, d: &Value) -> Plan {
    let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let exec = s("executionPlan");
    let topology = s("topology");
    let mut root = execution_tree(&exec)
        .or_else(|| (!topology.trim().is_empty()).then(|| dbine_driver::plan::tree_from_indented_text(&topology)))
        .unwrap_or_else(|| PlanNode { op: "Consulta".into(), ..Default::default() });
    let list = |k: &str| {
        d.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .filter(|v| !v.is_empty())
    };
    let mut extra: Vec<(String, String)> = Vec::new();
    for (label, v) in [
        ("Consulta", Some(s("id")).filter(|v| !v.is_empty())),
        ("Tipo", Some(s("queryType")).filter(|v| !v.is_empty())),
        ("Estado", Some(s("state")).filter(|v| !v.is_empty())),
        ("Orígenes", list("sources")),
        ("Destinos", list("sinks")),
        ("Ventana", Some(s("windowType")).filter(|v| !v.is_empty())),
    ] {
        if let Some(v) = v {
            extra.push((label.into(), v));
        }
    }
    extra.append(&mut root.props);
    root.props = extra;
    let mut raw = exec;
    if !topology.trim().is_empty() {
        raw = format!("{}\n\n{}", raw.trim_end(), topology.trim_end());
    }
    Plan { statement: statement.to_string(), root, actual: false, raw_format: "text".into(), raw }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_nest_by_tabs() {
        let t = " > [ SINK ] | Schema: ID INT KEY, N STRING | Logger: CSAS_S2_1.S2\n\t\t > [ PROJECT ] | Schema: ID INT KEY, N STRING | Logger: CSAS_S2_1.Project\n\t\t\t\t > [ FILTER ] | Schema: X | Logger: CSAS_S2_1.WhereFilter\n\t\t\t\t\t\t > [ SOURCE ] | Schema: ID INT KEY | Logger: CSAS_S2_1.KsqlTopic.Source\n";
        let root = execution_tree(t).unwrap();
        assert_eq!(root.op, "SINK");
        assert_eq!(root.detail, "S2");
        assert_eq!(root.props[0], ("Schema".to_string(), "ID INT KEY, N STRING".to_string()));
        assert_eq!(root.children[0].op, "PROJECT");
        assert_eq!(root.children[0].children[0].children[0].op, "SOURCE");
    }

    #[test]
    fn joins_have_two_inputs() {
        let t = " > [ SINK ] | Logger: Q.J\n\t\t > [ JOIN ] | Logger: Q.Join\n\t\t\t\t > [ SOURCE ] | Logger: Q.L\n\t\t\t\t > [ SOURCE ] | Logger: Q.R\n";
        let root = execution_tree(t).unwrap();
        assert_eq!(root.children[0].children.len(), 2);
    }

    #[test]
    fn description_props() {
        let d = json!({"id": "CSAS_S2_1", "queryType": "PERSISTENT", "sources": ["S"], "sinks": ["S2"],
            "executionPlan": " > [ SINK ] | Logger: CSAS_S2_1.S2\n", "topology": "Topologies:\n   Sub-topology: 0\n"});
        let p = from_description("CREATE STREAM S2 AS SELECT * FROM S", &d);
        assert_eq!(p.root.op, "SINK");
        assert_eq!(p.root.props[0], ("Consulta".to_string(), "CSAS_S2_1".to_string()));
        assert!(p.raw.contains("Topologies"));
    }
}

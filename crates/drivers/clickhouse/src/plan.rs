//! ClickHouse's `EXPLAIN json = 1, indexes = 1` (the query plan steps)
//! as a [`PlanNode`] tree. ClickHouse gives no costs nor row estimates
//! there; what it does say is how many parts and granules each index let
//! through, which is what the warnings look at.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

/// Rows per granule with the default `index_granularity`.
const GRANULE_ROWS: f64 = 8192.0;
/// Reading every granule of at least this many gets a warning (~100k rows).
const BIG_READ_GRANULES: f64 = 12.0;

/// SELECT-like statements: the only ones ClickHouse explains as JSON.
pub(crate) fn explainable(stmt: &str) -> bool {
    let kw: String = stmt.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    matches!(kw.to_ascii_lowercase().as_str(), "select" | "with")
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

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) if items.iter().all(|i| !i.is_object()) => {
            items.iter().map(text).collect::<Vec<_>>().join(", ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn plan_json(statement: &str, raw: &str) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let top = v.get(0).unwrap_or(&v);
    let plan = top.get("Plan").ok_or("el plan JSON no tiene \"Plan\"")?;
    Ok(Plan {
        statement: statement.into(),
        root: node(plan),
        actual: false,
        raw_format: "json".into(),
        raw: raw.trim().into(),
    })
}

fn node(p: &Value) -> PlanNode {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let op = s("Node Type");
    let description = s("Description");
    let reads = op.starts_with("ReadFrom");
    let mut n = PlanNode {
        object: (reads && !description.is_empty()).then(|| description.clone()),
        detail: if reads { String::new() } else { description },
        op,
        ..Default::default()
    };
    if let Value::Object(m) = p {
        for (k, v) in m {
            match k.as_str() {
                "Plans" | "Node Type" | "Description" => {}
                "Indexes" => {
                    for idx in v.as_array().into_iter().flatten() {
                        index_props(idx, &mut n);
                    }
                }
                _ => n.props.push((k.clone(), text(v))),
            }
        }
    }
    n.children = p.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(node).collect()).unwrap_or_default();
    n
}

/// `{"Type": "PrimaryKey", "Condition": …, "Initial Granules": 24, "Selected Granules": 1}`.
fn index_props(idx: &Value, n: &mut PlanNode) {
    let ty = idx.get("Type").map(text).unwrap_or_else(|| "Index".into());
    let name = idx.get("Name").map(text).map(|x| format!("{ty} {x}")).unwrap_or(ty.clone());
    let g = |k: &str| idx.get(k).and_then(Value::as_f64);
    let mut parts = Vec::new();
    if let Some(c) = idx.get("Condition").map(text) {
        parts.push(c);
    }
    if let (Some(sel), Some(init)) = (g("Selected Parts"), g("Initial Parts")) {
        parts.push(format!("partes {sel}/{init}"));
    }
    if let (Some(sel), Some(init)) = (g("Selected Granules"), g("Initial Granules")) {
        parts.push(format!("gránulos {sel}/{init}"));
        if ty == "PrimaryKey" {
            n.est_rows = Some(sel * GRANULE_ROWS);
            if sel >= init && init >= BIG_READ_GRANULES {
                n.warnings.push(format!("El índice primario no descarta gránulos: lee los {init} (~{} filas)", (init * GRANULE_ROWS) as u64));
            }
        }
    }
    if let Some(keys) = idx.get("Keys").map(text) {
        parts.push(format!("claves {keys}"));
    }
    n.props.push((format!("Índice {name}"), parts.join(" · ")));
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"[
  {
    "Plan": {
      "Node Type": "Expression", "Node Id": "Expression_11", "Description": "Project names",
      "Plans": [
        {
          "Node Type": "Join", "Node Id": "Join_24", "Description": "JOIN FillRightFirst",
          "Plans": [
            {"Node Type": "ReadFromMergeTree", "Node Id": "ReadFromMergeTree_2", "Description": "default.b",
             "Indexes": [{"Type": "PrimaryKey", "Condition": "true", "Initial Parts": 1, "Selected Parts": 1,
                          "Initial Granules": 30, "Selected Granules": 30}]},
            {"Node Type": "ReadFromMergeTree", "Node Id": "ReadFromMergeTree_0", "Description": "default.a",
             "Indexes": [{"Type": "PrimaryKey", "Keys": ["id"], "Condition": "(id in (-Inf, 999])",
                          "Initial Parts": 1, "Selected Parts": 1, "Initial Granules": 24, "Selected Granules": 1},
                         {"Type": "Skip", "Name": "idx_g", "Initial Granules": 1, "Selected Granules": 1}]}
          ]
        }
      ]
    }
  }
]"#;

    #[test]
    fn steps_become_a_tree() {
        let p = plan_json("q", PLAN).unwrap();
        assert!(!p.actual);
        assert_eq!((p.root.op.as_str(), p.root.detail.as_str()), ("Expression", "Project names"));
        let join = &p.root.children[0];
        assert_eq!(join.detail, "JOIN FillRightFirst");
        let [b, a] = &join.children[..] else { panic!() };
        assert_eq!(b.object.as_deref(), Some("default.b"));
        assert!(b.warnings.iter().any(|w| w.contains("no descarta")));
        assert_eq!(a.est_rows, Some(8192.0));
        assert!(a.warnings.is_empty());
        assert!(a.props.iter().any(|(k, v)| k == "Índice PrimaryKey" && v.contains("gránulos 1/24")));
        assert!(a.props.iter().any(|(k, _)| k == "Índice Skip idx_g"));
        assert!(explainable("WITH x AS (SELECT 1) SELECT * FROM x") && !explainable("INSERT INTO t SELECT 1"));
    }
}

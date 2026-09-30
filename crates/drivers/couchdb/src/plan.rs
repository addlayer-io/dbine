//! Mango `_explain` replies (and `_find` execution stats) → [`Plan`] trees.
//!
//! A Mango query reads an index (a JSON/text index, or `_all_docs` when
//! none fits), fetches the documents unless the index covers the fields,
//! and filters them with the selector in memory:
//! `Mango query → Filter → Fetch → Index Scan`.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

const RATIO_WARN: f64 = 100.0;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// The plan of one Mango query. `stats` is the `execution_stats` of the
/// `_find` reply when the query ran.
pub fn from_explain(statement: &str, explain: &Value, stats: Option<&Value>) -> Plan {
    let ix = explain.get("index").cloned().unwrap_or(Value::Null);
    let ix_name = ix.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
    let ix_type = ix.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let ddoc = ix.get("ddoc").and_then(Value::as_str).map(|d| d.trim_start_matches("_design/").to_string());

    // Index access.
    let mut scan = PlanNode::default();
    let full = ix_type == "special" || ix_name == "_all_docs";
    if full {
        scan.op = "Full Scan".into();
        scan.object = Some("_all_docs".into());
        scan.warnings.push("Sin índice: recorre todos los documentos (_all_docs)".into());
    } else {
        scan.op = match ix_type.as_str() {
            "text" => "Text Search",
            "nouveau" => "Nouveau Search",
            _ => "Index Scan",
        }
        .into();
        scan.object = Some(match &ddoc {
            Some(d) => format!("{d}/{ix_name}"),
            None => ix_name.clone(),
        });
    }
    if let Some(def) = ix.get("def") {
        scan.detail = def.get("fields").map_or_else(|| text(def), text);
        scan.props.push(("def".into(), text(def)));
    }
    scan.props.push(("type".into(), ix_type.clone()));
    if let Some(m) = explain.get("mrargs").and_then(Value::as_object) {
        for (k, v) in m {
            scan.props.push((k.clone(), text(v)));
        }
    }

    let covering = explain.get("covering").and_then(Value::as_bool) == Some(true);
    let selector = explain.get("selector").cloned().unwrap_or(Value::Null);
    let mut filter = PlanNode { op: "Filter".into(), detail: text(&selector), ..Default::default() };
    filter.props.push(("selector".into(), text(&selector)));

    let mut root = PlanNode { op: "Mango query".into(), ..Default::default() };
    root.object = explain.get("dbname").and_then(Value::as_str).map(str::to_string);
    for k in ["fields", "limit", "skip"] {
        if let Some(v) = explain.get(k) {
            root.props.push((k.into(), text(v)));
        }
    }
    if let Some(o) = explain.get("opts").and_then(Value::as_object) {
        for k in ["sort", "use_index", "r", "conflicts", "partition", "stable", "update"] {
            if let Some(v) = o.get(k).filter(|v| !v.is_null() && *v != &Value::String(String::new())) {
                root.props.push((k.into(), text(v)));
            }
        }
    }
    root.props.push(("covering".into(), covering.to_string()));
    if let Some(c) = explain.get("index_candidates").and_then(Value::as_array) {
        let list: Vec<String> = c
            .iter()
            .map(|cand| {
                let name = cand.pointer("/index/name").map(text).unwrap_or_default();
                let reasons: Vec<String> = cand
                    .pointer("/analysis/reasons")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|r| r.get("name").map(text)).collect())
                    .unwrap_or_default();
                if reasons.is_empty() { name } else { format!("{name} ({})", reasons.join(", ")) }
            })
            .collect();
        if !list.is_empty() {
            root.props.push(("index_candidates".into(), list.join("; ")));
        }
    }

    let mut fetch_rows = None;
    if let Some(s) = stats {
        let keys = num(s.get("total_keys_examined"));
        let docs = num(s.get("total_docs_examined")).or_else(|| num(s.get("total_quorum_docs_examined")));
        let out = num(s.get("results_returned"));
        scan.actual_rows = if full { docs.or(keys) } else { keys.or(docs) };
        fetch_rows = docs;
        filter.actual_rows = out;
        root.actual_rows = out;
        root.actual_ms = num(s.get("execution_time_ms"));
        if let Some(m) = s.as_object() {
            for (k, v) in m {
                root.props.push((k.clone(), text(v)));
            }
        }
        let read = docs.unwrap_or(0.0).max(keys.unwrap_or(0.0));
        if read > RATIO_WARN && read / out.unwrap_or(0.0).max(1.0) > RATIO_WARN {
            root.warnings.push(format!(
                "Examina {read} documentos/claves para devolver {}: falta un índice más selectivo",
                out.unwrap_or(0.0)
            ));
        }
    }
    // _all_docs reads the documents themselves; a covering index needs none.
    if covering || full {
        filter.children.push(scan);
    } else {
        let fetch = PlanNode { op: "Fetch".into(), detail: "documentos".into(), actual_rows: fetch_rows, ..Default::default() };
        filter.children.push(PlanNode { children: vec![scan], ..fetch });
    }
    root.children.push(filter);
    Plan {
        statement: statement.to_string(),
        root,
        actual: stats.is_some(),
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&match stats {
            Some(s) => serde_json::json!({ "explain": explain, "execution_stats": s }),
            None => explain.clone(),
        })
        .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn explain_all_docs() -> Value {
        json!({
            "dbname": "films",
            "index": { "ddoc": null, "name": "_all_docs", "type": "special", "def": { "fields": [{ "_id": "asc" }] } },
            "partitioned": false,
            "selector": { "year": { "$gt": 2000 } },
            "opts": { "use_index": [], "bookmark": "nil", "limit": 26, "skip": 0, "sort": {}, "fields": "all_fields", "r": 1, "conflicts": false },
            "limit": 26, "skip": 0, "fields": "all_fields",
            "mrargs": { "include_docs": true, "view_type": "map", "reduce": false, "start_key": null, "end_key": "<MAX>", "direction": "fwd" },
            "covering": false
        })
    }

    #[test]
    fn full_scan_with_stats() {
        let stats = json!({ "total_keys_examined": 0, "total_docs_examined": 1000, "total_quorum_docs_examined": 0,
                            "results_returned": 3, "execution_time_ms": 12.5 });
        let p = from_explain("{…}", &explain_all_docs(), Some(&stats));
        assert!(p.actual);
        assert_eq!(p.root.object.as_deref(), Some("films"));
        assert_eq!(p.root.actual_rows, Some(3.0));
        assert_eq!(p.root.actual_ms, Some(12.5));
        assert!(p.root.warnings[0].contains("1000"));
        let filter = &p.root.children[0];
        assert_eq!(filter.op, "Filter");
        let scan = &filter.children[0];
        assert_eq!(scan.op, "Full Scan");
        assert_eq!(scan.actual_rows, Some(1000.0));
        assert!(scan.warnings[0].contains("_all_docs"));
        assert!(p.raw.contains("execution_stats"));
    }

    #[test]
    fn json_index_estimated() {
        let e = json!({
            "dbname": "films",
            "index": { "ddoc": "_design/by-year", "name": "year-idx", "type": "json", "def": { "fields": [{ "year": "asc" }] } },
            "selector": { "year": { "$gt": 2000 } },
            "opts": { "sort": {}, "r": 1 },
            "mrargs": { "start_key": [2000], "end_key": ["<MAX>"], "direction": "fwd", "include_docs": true },
            "covering": false,
            "index_candidates": [{ "index": { "name": "_all_docs" }, "analysis": { "usable": true, "reasons": [{ "name": "unfavored_type" }] } }]
        });
        let p = from_explain("q", &e, None);
        assert!(!p.actual);
        let fetch = &p.root.children[0].children[0];
        assert_eq!(fetch.op, "Fetch");
        let scan = &fetch.children[0];
        assert_eq!(scan.op, "Index Scan");
        assert_eq!(scan.object.as_deref(), Some("by-year/year-idx"));
        assert_eq!(scan.detail, "[{\"year\":\"asc\"}]");
        assert!(scan.warnings.is_empty());
        assert!(p.root.props.iter().any(|(k, v)| k == "index_candidates" && v.contains("unfavored_type")));
    }
}

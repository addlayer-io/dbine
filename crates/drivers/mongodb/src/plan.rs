//! MongoDB `explain` replies → [`Plan`] trees.
//!
//! The winning plan (`queryPlanner.winningPlan`, or its `queryPlan` with the
//! slot-based engine) is a tree of stages linked by `inputStage` /
//! `inputStages` (and `shards` on a mongos). With `executionStats`, the
//! classic engine returns `executionStats.executionStages` with the same
//! shape plus the figures; both trees are walked side by side. Aggregations
//! that aren't fully pushed down to the query layer come as a `stages`
//! array (the first one `$cursor`, holding a query plan): each stage feeds
//! the next, so the last one is the root.

use dbine_driver::{Plan, PlanNode};
use mongodb::bson::{Bson, Document};

/// Commands `explain` accepts (lowercase).
pub fn explainable(name: &str) -> bool {
    matches!(name, "find" | "aggregate" | "count" | "distinct" | "update" | "delete" | "findandmodify" | "mapreduce")
}

/// Figures worth a warning: more than this many documents read per one returned.
const RATIO_WARN: f64 = 100.0;

/// Keys that link stages or are shown elsewhere.
const SKIP_KEYS: &[&str] = &[
    "stage",
    "inputStage",
    "inputStages",
    "shards",
    "queryPlan",
    "slotBasedPlan",
    "nReturned",
    "executionTimeMillisEstimate",
    "executionTimeMillis",
    "isEOF",
    "saveState",
    "restoreState",
    "needYield",
];

pub fn num(b: Option<&Bson>) -> Option<f64> {
    match b? {
        Bson::Int32(n) => Some(*n as f64),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        _ => None,
    }
}

fn text(b: &Bson) -> String {
    match b {
        Bson::String(s) => s.clone(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => n.to_string(),
        Bson::Boolean(v) => v.to_string(),
        Bson::Null => "null".into(),
        other => other.clone().into_relaxed_extjson().to_string(),
    }
}

fn clip(s: String, n: usize) -> String {
    if s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// The reply as pretty JSON, without the cluster gossip.
pub fn raw_json(reply: &Document) -> String {
    let mut r = reply.clone();
    for k in ["$clusterTime", "operationTime", "ok"] {
        r.remove(k);
    }
    serde_json::to_string_pretty(&Bson::Document(r).into_relaxed_extjson()).unwrap_or_default()
}

/// The plan of one statement from its `explain` reply.
pub fn from_explain(statement: &str, command: &str, reply: &Document, actual: bool) -> Plan {
    let mut root = PlanNode { op: command.to_string(), ..Default::default() };
    fill_root(&mut root, reply);
    Plan { statement: statement.to_string(), root, actual, raw_format: "json".into(), raw: raw_json(reply) }
}

/// Statement node: the command, with the query plan (or the pipeline, or
/// the shards) below it.
fn fill_root(root: &mut PlanNode, reply: &Document) {
    if let Ok(stages) = reply.get_array("stages") {
        let chain = pipeline(stages);
        if let Some(last) = chain.as_ref() {
            root.actual_rows = last.actual_rows;
            // Stage times are cumulative: the last one is the pipeline's.
            root.actual_ms = last.actual_ms;
            root.object = find_object(last);
        }
        root.children.extend(chain);
    } else if let Ok(shards) = reply.get_document("shards") {
        // Sharded aggregate: each shard runs its part, mongos merges.
        let mut merge = PlanNode {
            op: "MERGE".into(),
            detail: reply.get_str("mergeType").unwrap_or_default().to_string(),
            ..Default::default()
        };
        if let Some(sp) = reply.get("splitPipeline") {
            merge.props.push(("splitPipeline".into(), clip(text(sp), 2000)));
        }
        for (name, s) in shards {
            let Some(s) = s.as_document() else { continue };
            let mut shard = PlanNode { op: "SHARD".into(), object: Some(name.clone()), ..Default::default() };
            fill_root(&mut shard, s);
            merge.children.push(shard);
        }
        root.children.push(merge);
    } else if let Ok(qp) = reply.get_document("queryPlanner") {
        query(root, qp, reply.get_document("executionStats").ok());
    }
}

/// A query plan under `node` (the statement or a `$cursor` stage).
fn query(node: &mut PlanNode, qp: &Document, es: Option<&Document>) {
    let ns = qp.get_str("namespace").ok().map(str::to_string);
    if node.object.is_none() {
        node.object = ns.clone();
    }
    let winning = qp.get_document("winningPlan").ok().map(|w| w.get_document("queryPlan").unwrap_or(w));
    let exec = es.and_then(|e| e.get_document("executionStages").ok());
    for (k, label) in [
        ("plannerVersion", "plannerVersion"),
        ("indexFilterSet", "indexFilterSet"),
        ("planCacheKey", "planCacheKey"),
        ("queryHash", "queryHash"),
        ("parsedQuery", "parsedQuery"),
    ] {
        if let Some(v) = qp.get(k) {
            node.props.push((label.into(), clip(text(v), 2000)));
        }
    }
    if let Ok(r) = qp.get_array("rejectedPlans") {
        node.props.push(("rejectedPlans".into(), r.len().to_string()));
    }
    if let Some(es) = es {
        node.actual_rows = num(es.get("nReturned")).or(node.actual_rows);
        node.actual_ms = num(es.get("executionTimeMillis")).or(node.actual_ms);
        for k in ["totalKeysExamined", "totalDocsExamined", "executionSuccess"] {
            if let Some(v) = es.get(k) {
                node.props.push((k.into(), text(v)));
            }
        }
        let docs = num(es.get("totalDocsExamined")).unwrap_or(0.0);
        let keys = num(es.get("totalKeysExamined")).unwrap_or(0.0);
        let n = num(es.get("nReturned")).unwrap_or(0.0);
        let read = docs.max(keys);
        // Writes return nothing: the ratio only means something for reads.
        let write = matches!(node.op.as_str(), "update" | "delete" | "findandmodify");
        if !write && read > RATIO_WARN && read / n.max(1.0) > RATIO_WARN {
            node.warnings.push(format!(
                "Examina {read} documentos/claves para devolver {n}: falta un índice más selectivo"
            ));
        }
    }
    if let Some(w) = winning {
        node.children.push(stage(w, exec, ns.as_deref()));
    }
}

/// One stage of the winning plan, merged with its execution stats when
/// they describe the same stage.
fn stage(w: &Document, e: Option<&Document>, ns: Option<&str>) -> PlanNode {
    let op = w.get_str("stage").unwrap_or("?").to_string();
    let e = e.filter(|e| e.get_str("stage").ok() == Some(op.as_str()));
    let mut n = PlanNode { op: op.clone(), ..Default::default() };
    n.object = w.get_str("indexName").ok().map(str::to_string);
    if n.object.is_none() && matches!(op.as_str(), "COLLSCAN" | "CLUSTERED_IXSCAN" | "EOF") {
        n.object = ns.map(str::to_string);
    }
    if let Some(kp) = w.get("keyPattern") {
        n.detail = text(kp);
    } else if let Ok(d) = w.get_str("direction") {
        n.detail = d.to_string();
    }
    for (k, v) in w {
        if !SKIP_KEYS.contains(&k.as_str()) {
            n.props.push((k.clone(), clip(text(v), 2000)));
        }
    }
    if let Some(e) = e {
        n.actual_rows = num(e.get("nReturned"));
        n.actual_ms = num(e.get("executionTimeMillisEstimate")).or_else(|| num(e.get("executionTimeMillis")));
        for (k, v) in e {
            if !SKIP_KEYS.contains(&k.as_str()) && !w.contains_key(k) && v.as_document().is_none() {
                n.props.push((k.clone(), clip(text(v), 2000)));
            }
        }
        if e.get_bool("usedDisk") == Ok(true) {
            n.warnings.push("Usó disco (no alcanzó la memoria)".into());
        }
    }
    match op.as_str() {
        "COLLSCAN" => n.warnings.push("COLLSCAN: recorre toda la colección".into()),
        "SORT" => n.warnings.push("SORT: ordena en memoria (ningún índice da el orden)".into()),
        _ => {}
    }
    // Children: inputStage, inputStages, or the shards of a mongos plan.
    if let Ok(c) = w.get_document("inputStage") {
        n.children.push(stage(c, e.and_then(|e| e.get_document("inputStage").ok()), ns));
    }
    if let Ok(cs) = w.get_array("inputStages") {
        let es = e.and_then(|e| e.get_array("inputStages").ok());
        for (i, c) in cs.iter().enumerate() {
            let Some(c) = c.as_document() else { continue };
            let ec = es.and_then(|a| a.get(i)).and_then(Bson::as_document);
            n.children.push(stage(c, ec, ns));
        }
    }
    if let Ok(shards) = w.get_array("shards") {
        let es = e.and_then(|e| e.get_array("shards").ok());
        for (i, s) in shards.iter().enumerate() {
            let Some(s) = s.as_document() else { continue };
            let name = s.get_str("shardName").unwrap_or("?").to_string();
            let es = es
                .and_then(|a| a.iter().filter_map(Bson::as_document).find(|d| d.get_str("shardName") == Ok(name.as_str())).or_else(|| a.get(i)?.as_document()));
            let mut sn = PlanNode { op: "SHARD".into(), object: Some(name), ..Default::default() };
            if let Some(es) = es {
                sn.actual_rows = num(es.get("nReturned"));
                sn.actual_ms = num(es.get("executionTimeMillis")).or_else(|| num(es.get("executionTimeMillisEstimate")));
                for k in ["totalKeysExamined", "totalDocsExamined"] {
                    if let Some(v) = es.get(k) {
                        sn.props.push((k.into(), text(v)));
                    }
                }
            }
            if let Ok(wp) = s.get_document("winningPlan") {
                let wp = wp.get_document("queryPlan").unwrap_or(wp);
                let ex = es.and_then(|d| d.get_document("executionStages").ok());
                sn.children.push(stage(wp, ex, s.get_str("namespace").ok().or(ns)));
            }
            n.children.push(sn);
        }
    }
    n
}

/// Aggregation stages as a chain: the last one is the root, each one's
/// child is the stage before it.
fn pipeline(stages: &[Bson]) -> Option<PlanNode> {
    let mut below: Option<PlanNode> = None;
    for s in stages.iter().filter_map(Bson::as_document) {
        let Some((name, spec)) = s.iter().find(|(k, _)| k.starts_with('$')) else { continue };
        let mut n = PlanNode { op: name.clone(), ..Default::default() };
        n.actual_rows = num(s.get("nReturned"));
        n.actual_ms = num(s.get("executionTimeMillisEstimate"));
        if name == "$cursor" {
            if let Some(c) = spec.as_document() {
                if let Ok(qp) = c.get_document("queryPlanner") {
                    query(&mut n, qp, c.get_document("executionStats").ok());
                    // The $cursor's own figures are the stage's, not the query's.
                    n.actual_rows = num(s.get("nReturned")).or(n.actual_rows);
                }
            }
        } else {
            n.detail = clip(text(spec), 300);
            if name == "$lookup" {
                if let Some(from) = spec.as_document().and_then(|d| d.get_str("from").ok()) {
                    n.object = Some(from.to_string());
                }
                if num(s.get("collectionScans")).unwrap_or(0.0) > 0.0 {
                    n.warnings.push("El $lookup recorre la colección externa sin índice".into());
                }
            }
            if s.get_bool("usedDisk") == Ok(true) {
                n.warnings.push("Usó disco (no alcanzó la memoria)".into());
            }
            for (k, v) in s {
                if k != name && !SKIP_KEYS.contains(&k.as_str()) {
                    n.props.push((k.clone(), clip(text(v), 2000)));
                }
            }
        }
        if let Some(b) = below.take() {
            n.children.push(b);
        }
        below = Some(n);
    }
    below
}

/// The first object (namespace / index) found going down the tree.
fn find_object(n: &PlanNode) -> Option<String> {
    n.object.clone().or_else(|| n.children.iter().find_map(find_object))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Document {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        match Bson::try_from(v).unwrap() {
            Bson::Document(d) => d,
            _ => panic!("not a document"),
        }
    }

    const FIND_EXEC: &str = r#"{
      "explainVersion": "1",
      "queryPlanner": {
        "namespace": "t.people", "indexFilterSet": false,
        "parsedQuery": { "age": { "$gt": 30 } },
        "winningPlan": { "stage": "SORT", "sortPattern": { "name": 1 }, "memLimit": 104857600, "type": "simple",
          "inputStage": { "stage": "COLLSCAN", "filter": { "age": { "$gt": 30 } }, "direction": "forward" } },
        "rejectedPlans": []
      },
      "executionStats": {
        "executionSuccess": true, "nReturned": 2, "executionTimeMillis": 3,
        "totalKeysExamined": 0, "totalDocsExamined": 5000,
        "executionStages": { "stage": "SORT", "nReturned": 2, "executionTimeMillisEstimate": 1, "works": 5004,
          "usedDisk": false, "sortPattern": { "name": 1 },
          "inputStage": { "stage": "COLLSCAN", "nReturned": 2, "executionTimeMillisEstimate": 1, "works": 5002,
            "docsExamined": 5000, "direction": "forward", "filter": { "age": { "$gt": 30 } } } }
      },
      "ok": 1
    }"#;

    #[test]
    fn find_with_execution_stats() {
        let p = from_explain("db.people.find(…)", "find", &doc(FIND_EXEC), true);
        let r = &p.root;
        assert_eq!(r.op, "find");
        assert_eq!(r.object.as_deref(), Some("t.people"));
        assert_eq!(r.actual_rows, Some(2.0));
        assert_eq!(r.actual_ms, Some(3.0));
        assert!(r.warnings.iter().any(|w| w.contains("5000")), "{:?}", r.warnings);
        let sort = &r.children[0];
        assert_eq!(sort.op, "SORT");
        assert!(sort.warnings[0].starts_with("SORT"));
        let scan = &sort.children[0];
        assert_eq!(scan.op, "COLLSCAN");
        assert_eq!(scan.object.as_deref(), Some("t.people"));
        assert_eq!(scan.actual_rows, Some(2.0));
        assert!(scan.warnings.iter().any(|w| w.starts_with("COLLSCAN")));
        assert!(scan.props.iter().any(|(k, v)| k == "docsExamined" && v == "5000"));
        assert!(p.raw.contains("\"winningPlan\"") && !p.raw.contains("\"ok\""));
    }

    #[test]
    fn index_scan_estimated() {
        let d = doc(r#"{ "queryPlanner": { "namespace": "t.people",
          "winningPlan": { "stage": "FETCH", "inputStage": { "stage": "IXSCAN", "keyPattern": { "age": 1 },
            "indexName": "age_1", "isMultiKey": false, "direction": "forward",
            "indexBounds": { "age": ["(30, inf.0]"] } } },
          "rejectedPlans": [{ "stage": "COLLSCAN" }] } }"#);
        let p = from_explain("q", "find", &d, false);
        assert!(!p.actual);
        let ix = &p.root.children[0].children[0];
        assert_eq!(ix.op, "IXSCAN");
        assert_eq!(ix.object.as_deref(), Some("age_1"));
        assert_eq!(ix.detail, "{\"age\":1}");
        assert!(ix.warnings.is_empty());
        assert!(p.root.props.contains(&("rejectedPlans".into(), "1".into())));
    }

    #[test]
    fn slot_based_engine_uses_query_plan() {
        let d = doc(r#"{ "queryPlanner": { "namespace": "t.c", "winningPlan": {
            "queryPlan": { "stage": "GROUP", "inputStage": { "stage": "COLLSCAN", "direction": "forward" } },
            "slotBasedPlan": { "slots": "…", "stages": "…" } } },
          "executionStats": { "nReturned": 3, "executionTimeMillis": 1, "totalDocsExamined": 10,
            "executionStages": { "stage": "project", "nReturned": 3 } } }"#);
        let p = from_explain("q", "aggregate", &d, true);
        let g = &p.root.children[0];
        assert_eq!(g.op, "GROUP");
        assert_eq!(g.actual_rows, None); // SBE stages don't line up
        assert_eq!(g.children[0].op, "COLLSCAN");
        assert_eq!(p.root.actual_rows, Some(3.0));
    }

    #[test]
    fn aggregation_stages_chain() {
        let d = doc(r#"{ "stages": [
            { "$cursor": { "queryPlanner": { "namespace": "t.o", "winningPlan": { "stage": "COLLSCAN", "direction": "forward" } },
                           "executionStats": { "nReturned": 100, "executionTimeMillis": 2, "totalDocsExamined": 100,
                             "executionStages": { "stage": "COLLSCAN", "nReturned": 100, "docsExamined": 100 } } },
              "nReturned": 100, "executionTimeMillisEstimate": 1 },
            { "$lookup": { "from": "c", "as": "x", "localField": "a", "foreignField": "b" },
              "nReturned": 100, "executionTimeMillisEstimate": 5, "collectionScans": 100 },
            { "$sort": { "sortKey": { "n": -1 } }, "nReturned": 10, "executionTimeMillisEstimate": 6, "usedDisk": false }
          ] }"#);
        let p = from_explain("q", "aggregate", &d, true);
        let sort = &p.root.children[0];
        assert_eq!(sort.op, "$sort");
        assert_eq!(p.root.actual_rows, Some(10.0));
        let lookup = &sort.children[0];
        assert_eq!(lookup.op, "$lookup");
        assert_eq!(lookup.object.as_deref(), Some("c"));
        assert_eq!(lookup.warnings.len(), 1);
        let cursor = &lookup.children[0];
        assert_eq!(cursor.op, "$cursor");
        assert_eq!(cursor.actual_rows, Some(100.0));
        assert_eq!(cursor.children[0].op, "COLLSCAN");
        assert_eq!(p.root.object.as_deref(), Some("c"));
    }

    #[test]
    fn sharded_find() {
        let d = doc(r#"{ "queryPlanner": { "winningPlan": { "stage": "SHARD_MERGE", "shards": [
            { "shardName": "s0", "namespace": "t.c", "winningPlan": { "stage": "IXSCAN", "indexName": "a_1" } },
            { "shardName": "s1", "namespace": "t.c", "winningPlan": { "stage": "COLLSCAN" } } ] } },
          "executionStats": { "nReturned": 4, "executionTimeMillis": 2, "executionStages": { "stage": "SHARD_MERGE",
            "nReturned": 4, "shards": [
              { "shardName": "s1", "nReturned": 3, "executionTimeMillis": 1, "executionStages": { "stage": "COLLSCAN", "nReturned": 3 } },
              { "shardName": "s0", "nReturned": 1, "executionTimeMillis": 1, "executionStages": { "stage": "IXSCAN", "nReturned": 1 } } ] } } }"#);
        let p = from_explain("q", "find", &d, true);
        let merge = &p.root.children[0];
        assert_eq!(merge.op, "SHARD_MERGE");
        assert_eq!(merge.children[0].object.as_deref(), Some("s0"));
        assert_eq!(merge.children[0].actual_rows, Some(1.0));
        assert_eq!(merge.children[1].children[0].op, "COLLSCAN");
        assert_eq!(merge.children[1].children[0].actual_rows, Some(3.0));
        assert_eq!(merge.children[1].children[0].object.as_deref(), Some("t.c"));
    }
}

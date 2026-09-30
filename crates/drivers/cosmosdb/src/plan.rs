//! Cosmos DB plans.
//!
//! Cosmos has no EXPLAIN. The estimated plan is the gateway's query plan
//! (the one the SDKs ask for before a cross-partition query: which
//! operators run client-side, TOP / ORDER BY / GROUP BY / aggregates, and
//! the partition key ranges it touches). The actual plan comes from the
//! query metrics of a real run (`x-ms-documentdb-query-metrics`), drawn as
//! the backend's phases: index lookup → document load → runtime → output,
//! with the index metrics' suggestions as warnings.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use dbine_driver::{Plan, PlanNode};
use serde_json::{json, Value};

/// What the client side of DBine "supports", for the query plan request
/// (the gateway refuses to plan without it). DBine doesn't run these
/// operators itself; it only reads the plan.
pub const SUPPORTED_QUERY_FEATURES: &str = "Aggregate, CompositeAggregate, Distinct, MultipleOrderBy, OffsetAndLimit, \
OrderBy, Top, GroupBy, MultipleAggregates, NonValueAggregate, DCount, NonStreamingOrderBy, ListAndSetAggregate, CountIf";

const RATIO_WARN: f64 = 100.0;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn non_empty(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty() && s != "None",
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    })
}

/// Estimated plan from a query plan reply (`Null` when the server gave none).
pub fn from_query_plan(sql: &str, container: &str, qp: &Value) -> Plan {
    let mut root = PlanNode { op: "SELECT".into(), object: Some(container.to_string()), ..Default::default() };
    let info = qp.get("queryInfo").cloned().unwrap_or(Value::Null);
    if let Some(r) = non_empty(info.get("rewrittenQuery")) {
        root.props.push(("rewrittenQuery".into(), text(r)));
    }
    for k in ["hasSelectValue", "hasNonStreamingOrderBy"] {
        if let Some(v) = info.get(k) {
            root.props.push((k.into(), text(v)));
        }
    }
    // Client-side operators, outermost first.
    let mut ops: Vec<PlanNode> = Vec::new();
    let op = |name: &str, detail: String| PlanNode { op: name.into(), detail, ..Default::default() };
    if let Some(t) = non_empty(info.get("top")) {
        ops.push(op("Top", text(t)));
    }
    if non_empty(info.get("offset")).is_some() || non_empty(info.get("limit")).is_some() {
        let d = format!(
            "OFFSET {} LIMIT {}",
            info.get("offset").map(text).unwrap_or_default(),
            info.get("limit").map(text).unwrap_or_default()
        );
        ops.push(op("Offset/Limit", d));
    }
    if let Some(d) = non_empty(info.get("distinctType")) {
        ops.push(op("Distinct", text(d)));
    }
    if let Some(g) = non_empty(info.get("groupByExpressions")) {
        let mut n = op("Group By", text(g));
        if let Some(a) = non_empty(info.get("groupByAliasToAggregateType")) {
            n.props.push(("aggregates".into(), text(a)));
        }
        ops.push(n);
    }
    if let Some(a) = non_empty(info.get("aggregates")) {
        ops.push(op("Aggregate", text(a)));
    }
    if let Some(o) = non_empty(info.get("orderByExpressions")) {
        let mut n = op("Order By", text(o));
        if let Some(d) = info.get("orderBy") {
            n.props.push(("orderBy".into(), text(d)));
        }
        if info.get("hasNonStreamingOrderBy").and_then(Value::as_bool) == Some(true) {
            n.warnings.push("ORDER BY sin índice que lo resuelva en streaming".into());
        }
        ops.push(n);
    }
    if let Some(dc) = non_empty(info.get("dCountInfo")) {
        ops.push(op("DCount", text(dc)));
    }
    let ranges = qp.get("queryRanges").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut leaf = PlanNode { op: "Query Ranges".into(), object: Some(container.to_string()), ..Default::default() };
    if qp.is_null() {
        leaf.op = "Query".into();
        leaf.detail = "sin plan del servidor".into();
    } else {
        leaf.detail = match ranges.len() {
            1 if ranges[0].get("min").map(text).unwrap_or_default().is_empty()
                && ranges[0].get("max").map(text).as_deref() == Some("FF") =>
            {
                "todas las particiones".into()
            }
            1 => "una partición".into(),
            n => format!("{n} rangos de partición"),
        };
        for (i, r) in ranges.iter().take(50).enumerate() {
            let incl = |k: &str| r.get(k).and_then(Value::as_bool).unwrap_or(false);
            let v = format!(
                "{}{}, {}{}",
                if incl("isMinInclusive") { "[" } else { "(" },
                r.get("min").map(text).unwrap_or_default(),
                r.get("max").map(text).unwrap_or_default(),
                if incl("isMaxInclusive") { "]" } else { ")" },
            );
            leaf.props.push((format!("rango {}", i + 1), v));
        }
    }
    // Chain: root → outermost operator → … → ranges.
    let mut below = leaf;
    while let Some(mut n) = ops.pop() {
        n.children.push(below);
        below = n;
    }
    root.children.push(below);
    Plan {
        statement: sql.to_string(),
        root,
        actual: false,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(qp).unwrap_or_default(),
    }
}

/// `key=value;key=value` metrics, summed over pages (ratios averaged).
pub fn parse_metrics(pages: &[String]) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64, usize)> = Vec::new();
    for page in pages {
        // Several partitions may come comma-separated in one header.
        for part in page.split([';', ',']) {
            let Some((k, v)) = part.split_once('=') else { continue };
            let Ok(v) = v.trim().parse::<f64>() else { continue };
            let k = k.trim();
            match out.iter_mut().find(|(n, _, _)| n == k) {
                Some(e) => {
                    e.1 += v;
                    e.2 += 1;
                }
                None => out.push((k.to_string(), v, 1)),
            }
        }
    }
    out.into_iter().map(|(k, v, n)| if k.to_ascii_lowercase().contains("ratio") { (k, v / n as f64) } else { (k, v) }).collect()
}

/// The index metrics header: Base64 JSON (older servers: plain JSON).
pub fn parse_index_metrics(raw: &str) -> Option<Value> {
    let decoded = B64.decode(raw.trim()).ok().and_then(|b| String::from_utf8(b).ok());
    let s = decoded.as_deref().unwrap_or(raw);
    serde_json::from_str(s).ok()
}

/// Index specs of a group of the index metrics (`Utilized…` / `Potential…`),
/// in both the old flat shape and the newer nested one.
fn index_specs(m: &Value, utilized: bool) -> Vec<String> {
    let (flat, nested) = if utilized { ("Utilized", "UtilizedIndexes") } else { ("Potential", "PotentialIndexes") };
    let mut out = Vec::new();
    let mut take = |list: Option<&Value>| {
        for i in list.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(s) = i.get("IndexSpec").and_then(Value::as_str) {
                out.push(s.to_string());
            } else if let Some(a) = i.get("IndexSpecs").and_then(Value::as_array) {
                out.push(format!("({})", a.iter().map(text).collect::<Vec<_>>().join(", ")));
            }
        }
    };
    take(m.get(format!("{flat}SingleIndexes")));
    take(m.get(format!("{flat}CompositeIndexes")));
    if let Some(n) = m.get(nested) {
        take(n.get("SingleIndexes"));
        take(n.get("CompositeIndexes"));
    }
    out
}

/// Actual plan from the metrics of a run.
pub fn from_metrics(sql: &str, container: &str, metrics: &[String], index: &[String], charge: f64, pages: usize, by_range: usize) -> Plan {
    let m = parse_metrics(metrics);
    let get = |k: &str| m.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| *v);
    let node = |op: &str, ms: &str| PlanNode { op: op.into(), actual_ms: get(ms), ..Default::default() };

    let mut lookup = node("Index Lookup", "indexLookupTimeInMs");
    lookup.object = Some(container.to_string());
    let mut used = Vec::new();
    let mut suggested = Vec::new();
    let mut index_json = Vec::new();
    for raw in index {
        if let Some(v) = parse_index_metrics(raw) {
            used.extend(index_specs(&v, true));
            suggested.extend(index_specs(&v, false));
            index_json.push(v);
        }
    }
    used.dedup();
    suggested.dedup();
    if !used.is_empty() {
        lookup.detail = used.join(", ");
    }
    for s in &suggested {
        lookup.warnings.push(format!("Índice sugerido: {s}"));
    }
    if let Some(r) = get("indexUtilizationRatio") {
        lookup.props.push(("indexUtilizationRatio".into(), format!("{r:.2}")));
    }
    for k in ["indexHitDocumentCount", "indexHitRatio"] {
        if let Some(v) = get(k) {
            lookup.props.push((k.into(), v.to_string()));
        }
    }
    lookup.actual_rows = get("indexHitDocumentCount");

    let mut load = node("Document Load", "documentLoadTimeInMs");
    load.actual_rows = get("retrievedDocumentCount");
    if let Some(v) = get("retrievedDocumentSize") {
        load.props.push(("retrievedDocumentSize (bytes)".into(), v.to_string()));
    }
    load.children.push(lookup);

    let mut vm = node("Runtime Execution", "VMExecutionTimeInMs");
    vm.actual_rows = get("outputDocumentCount");
    for k in ["instructionCount", "systemFunctionExecuteTimeInMs", "userFunctionExecuteTimeInMs"] {
        if let Some(v) = get(k) {
            vm.props.push((k.into(), v.to_string()));
        }
    }
    vm.children.push(load);

    let mut output = node("Write Output", "writeOutputTimeInMs");
    output.actual_rows = get("outputDocumentCount");
    if let Some(v) = get("outputDocumentSize") {
        output.props.push(("outputDocumentSize (bytes)".into(), v.to_string()));
    }
    output.children.push(vm);

    let mut root = PlanNode { op: "SELECT".into(), object: Some(container.to_string()), ..Default::default() };
    root.actual_ms = get("totalExecutionTimeInMs");
    root.actual_rows = get("outputDocumentCount");
    root.props.push(("Costo (RU)".into(), format!("{charge:.2}")));
    root.props.push(("Páginas".into(), pages.to_string()));
    if by_range > 0 {
        root.props.push(("Rangos de partición (consulta por rango)".into(), by_range.to_string()));
    }
    for k in [
        "queryCompileTimeInMs",
        "queryLogicalPlanBuildTimeInMs",
        "queryPhysicalPlanBuildTimeInMs",
        "queryOptimizationTimeInMs",
    ] {
        if let Some(v) = get(k) {
            root.props.push((k.into(), v.to_string()));
        }
    }
    let read = get("retrievedDocumentCount").unwrap_or(0.0);
    let out = get("outputDocumentCount").unwrap_or(0.0);
    if read > RATIO_WARN && read / out.max(1.0) > RATIO_WARN {
        root.warnings.push(format!("Lee {read} documentos para devolver {out}: falta un índice o un filtro más selectivo"));
    }
    if !suggested.is_empty() {
        root.warnings.push(format!("El servidor sugiere índices: {}", suggested.join(", ")));
    }
    root.children.push(output);
    let raw = json!({ "queryMetrics": metrics, "indexMetrics": index_json, "requestCharge": charge });
    Plan {
        statement: sql.to_string(),
        root,
        actual: true,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&raw).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_plan_becomes_a_chain() {
        let qp = json!({
            "partitionedQueryExecutionInfoVersion": 2,
            "queryInfo": { "distinctType": "None", "top": 10, "offset": null, "limit": null,
                "orderBy": ["Descending"], "orderByExpressions": ["c.price"], "groupByExpressions": [],
                "groupByAliases": [], "aggregates": [], "groupByAliasToAggregateType": {},
                "rewrittenQuery": "SELECT TOP 10 c._rid, [{\"item\": c.price}] AS orderByItems, c AS payload FROM c ORDER BY c.price DESC",
                "hasSelectValue": false, "dCountInfo": null, "hasNonStreamingOrderBy": false },
            "queryRanges": [{ "min": "", "max": "FF", "isMinInclusive": true, "isMaxInclusive": false }]
        });
        let p = from_query_plan("SELECT TOP 10 * FROM c ORDER BY c.price DESC", "products", &qp);
        assert!(!p.actual);
        let top = &p.root.children[0];
        assert_eq!(top.op, "Top");
        assert_eq!(top.detail, "10");
        let order = &top.children[0];
        assert_eq!(order.op, "Order By");
        assert_eq!(order.detail, "[\"c.price\"]");
        let leaf = &order.children[0];
        assert_eq!(leaf.op, "Query Ranges");
        assert_eq!(leaf.detail, "todas las particiones");
        assert_eq!(leaf.props[0].1, "[, FF)");
    }

    #[test]
    fn no_plan_from_server() {
        let p = from_query_plan("SELECT * FROM c", "c", &Value::Null);
        assert_eq!(p.root.children[0].op, "Query");
    }

    #[test]
    fn metrics_become_phases() {
        let m = vec![
            "totalExecutionTimeInMs=2.50;queryCompileTimeInMs=0.10;queryLogicalPlanBuildTimeInMs=0.02;\
queryPhysicalPlanBuildTimeInMs=0.05;queryOptimizationTimeInMs=0.00;VMExecutionTimeInMs=1.20;indexLookupTimeInMs=0.30;\
instructionCount=120;documentLoadTimeInMs=0.80;systemFunctionExecuteTimeInMs=0.00;userFunctionExecuteTimeInMs=0.00;\
retrievedDocumentCount=1000;retrievedDocumentSize=51200;outputDocumentCount=3;outputDocumentSize=300;writeOutputTimeInMs=0.01;\
indexUtilizationRatio=0.00"
                .to_string(),
            "totalExecutionTimeInMs=0.50;retrievedDocumentCount=500;outputDocumentCount=1;indexUtilizationRatio=1.00".to_string(),
        ];
        let idx = B64.encode(
            r#"{"UtilizedSingleIndexes":[],"PotentialSingleIndexes":[{"FilterExpression":"","IndexSpec":"/age/?","FilterPreciseSet":true,"IndexPreciseSet":true,"IndexImpactScore":"High"}],"UtilizedCompositeIndexes":[],"PotentialCompositeIndexes":[]}"#,
        );
        let p = from_metrics("SELECT * FROM c WHERE c.age = 3", "people", &m, &[idx], 12.34, 2, 0);
        assert!(p.actual);
        let r = &p.root;
        assert_eq!(r.actual_ms, Some(3.0));
        assert_eq!(r.actual_rows, Some(4.0));
        assert!(r.warnings.iter().any(|w| w.contains("1500")));
        assert!(r.warnings.iter().any(|w| w.contains("/age/?")));
        let out = &r.children[0];
        assert_eq!(out.op, "Write Output");
        let vm = &out.children[0];
        assert_eq!(vm.op, "Runtime Execution");
        assert_eq!(vm.actual_ms, Some(1.2));
        let load = &vm.children[0];
        assert_eq!(load.actual_rows, Some(1500.0));
        let lookup = &load.children[0];
        assert_eq!(lookup.op, "Index Lookup");
        assert_eq!(lookup.warnings, ["Índice sugerido: /age/?"]);
        assert!(lookup.props.iter().any(|(k, v)| k == "indexUtilizationRatio" && v == "0.50"));
    }

    #[test]
    fn nested_index_metrics() {
        let v = parse_index_metrics(
            r#"{"UtilizedIndexes":{"SingleIndexes":[{"IndexSpec":"/name/?"}],"CompositeIndexes":[{"IndexSpecs":["/a ASC","/b DESC"]}]},"PotentialIndexes":{"SingleIndexes":[],"CompositeIndexes":[]}}"#,
        )
        .unwrap();
        assert_eq!(index_specs(&v, true), ["/name/?", "(/a ASC, /b DESC)"]);
        assert!(index_specs(&v, false).is_empty());
    }
}

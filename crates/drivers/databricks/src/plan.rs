//! Databricks (Spark SQL) plans as [`PlanNode`] trees.
//!
//! - Estimated: `EXPLAIN FORMATTED`, whose `== Physical Plan ==` section is
//!   an operator tree drawn with `+-` / `:-` (each operator numbered, like
//!   `HashAggregate (7)`), followed by one details block per number
//!   (`(7) HashAggregate [codegen id : 2]` then `Key: value` lines). The
//!   tree gives the shape, the blocks the props.
//! - Actual: the Statement Execution API returns no execution metrics and
//!   the Spark UI's per-operator metrics aren't reachable through it; the
//!   query history API (`/api/2.0/sql/history/queries`, `include_metrics`)
//!   gives the query's totals (time, rows, bytes read, spill), which go on
//!   the root of the plan.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StmtKind {
    /// Queries and DML: EXPLAIN plans them.
    Plannable,
    /// DDL, USE, SET, SHOW…
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
        Some("select" | "with" | "values" | "table" | "from" | "insert" | "update" | "delete" | "merge") => StmtKind::Plannable,
        Some("create" | "replace") if words.iter().any(|w| w == "as") && words.iter().any(|w| w == "table") => StmtKind::Plannable,
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

/// `HashAggregate (7)` → ("HashAggregate", Some(7)).
fn numbered(body: &str) -> (String, Option<u32>) {
    let b = body.trim();
    if let Some(open) = b.rfind(" (") {
        if let Some(n) = b[open + 2..].strip_suffix(')').and_then(|n| n.trim().parse().ok()) {
            return (b[..open].trim().to_string(), Some(n));
        }
    }
    (b.to_string(), None)
}

/// `Scan parquet spark_catalog.default.t` → op "Scan parquet", object.
fn split_scan(name: &str) -> (String, Option<String>) {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 3 && (words[0].ends_with("Scan") || words[0] == "Scan") {
        return (format!("{} {}", words[0], words[1]), Some(words[2..].join(" ")));
    }
    (name.to_string(), None)
}

/// A plan from `EXPLAIN FORMATTED` output. Without a `== Physical Plan ==`
/// section (older runtimes, EXTENDED output) the text is drawn as an
/// indented tree.
pub(crate) fn formatted(statement: &str, raw: &str) -> Plan {
    let lines: Vec<&str> = raw.lines().collect();
    let Some(start) = lines.iter().position(|l| l.trim() == "== Physical Plan ==") else {
        return dbine_driver::plan::plan_from_text(statement, raw, false);
    };
    // The tree: up to the first blank line.
    let mut items: Vec<(usize, PlanNode, Option<u32>)> = Vec::new();
    let mut i = start + 1;
    while i < lines.len() && !lines[i].trim().is_empty() {
        let line = lines[i];
        let body_at = line.find(|c: char| !matches!(c, ' ' | ':' | '+' | '-' | '|')).unwrap_or(line.len());
        let (name, id) = numbered(&line[body_at..]);
        let (op, object) = split_scan(&name);
        items.push((body_at, PlanNode { op, object, ..Default::default() }, id));
        i += 1;
    }
    // Details: "(N) Name [extra]" blocks of "Key: value" lines.
    let mut details: HashMap<u32, (String, Vec<(String, String)>)> = HashMap::new();
    let mut current: Option<u32> = None;
    let mut trailer = Vec::new();
    for line in &lines[i..] {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("=====") || t.starts_with("Subquery:") {
            current = None;
            trailer.push(t.to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix('(') {
            if let Some((n, head)) = rest.split_once(") ") {
                if let Ok(n) = n.parse::<u32>() {
                    current = Some(n);
                    let extra = head.find(" [").map(|p| head[p + 1..].trim_matches(['[', ']']).to_string()).unwrap_or_default();
                    details.insert(n, (extra, Vec::new()));
                    continue;
                }
            }
        }
        match current.and_then(|n| details.get_mut(&n)) {
            Some((_, props)) => {
                let (k, v) = match t.split_once(": ") {
                    Some((k, v)) => (k.trim().trim_end_matches(':').trim().to_string(), v.trim().to_string()),
                    None => (String::new(), t.to_string()),
                };
                props.push((k, v));
            }
            None => trailer.push(t.to_string()),
        }
    }
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    for (depth, mut node, id) in items {
        if let Some((extra, props)) = id.and_then(|n| details.remove(&n)) {
            enrich(&mut node, &extra, props);
        }
        if let Some(n) = id {
            node.props.insert(0, ("Id".into(), n.to_string()));
        }
        while stack.len() > 1 && stack.last().is_some_and(|(d, _)| *d >= depth) {
            let (_, child) = stack.pop().expect("non-empty");
            stack.last_mut().expect("parent").1.children.push(child);
        }
        stack.push((depth, node));
    }
    while stack.len() > 1 {
        let (_, child) = stack.pop().expect("non-empty");
        stack.last_mut().expect("parent").1.children.push(child);
    }
    let mut root = stack.pop().map(|(_, n)| n).unwrap_or_else(|| PlanNode { op: "Plan".into(), ..Default::default() });
    if !trailer.is_empty() {
        root.props.push(("Subconsultas".into(), trailer.join("\n")));
    }
    Plan { statement: statement.into(), root, actual: false, raw_format: "text".into(), raw: raw.trim_end().into() }
}

fn enrich(n: &mut PlanNode, extra: &str, props: Vec<(String, String)>) {
    if !extra.is_empty() {
        n.props.push(("Etapa".into(), extra.to_string()));
    }
    // Keys may carry a count: `Keys [1]`.
    let get = |k: &str| {
        props.iter().find(|(pk, _)| pk == k || pk.strip_prefix(k).is_some_and(|r| r.starts_with(" ["))).map(|(_, v)| v.clone())
    };
    let detail: Vec<String> = ["Join type", "Join condition", "Condition", "Keys", "Arguments", "Functions"]
        .iter()
        .filter_map(|k| get(k))
        .filter(|v| !v.is_empty() && v != "None")
        .collect();
    if !detail.is_empty() {
        n.detail = detail.join(" · ");
    }
    if n.op.contains("CartesianProduct") || (n.op.contains("NestedLoopJoin") && get("Join condition").is_none_or(|c| c == "None")) {
        n.warnings.push("Producto cartesiano: join sin condición de igualdad".into());
    }
    if n.op.contains("Scan") && get("PartitionFilters").is_some_and(|f| f == "[]") && get("PushedFilters").is_some_and(|f| f == "[]") {
        n.warnings.push("Lee la tabla completa: sin filtros empujados ni de partición".into());
    }
    n.props.extend(props);
}

/// The query's totals from the query history API (`metrics`), on the
/// plan's root. Marks the plan as actual.
pub(crate) fn add_history(p: &mut Plan, q: &Value) {
    let m = q.get("metrics").cloned().unwrap_or(Value::Null);
    let f = |k: &str| m.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let root = &mut p.root;
    root.actual_ms = f("total_time_ms").or_else(|| q.get("duration").and_then(Value::as_f64));
    root.actual_rows = f("rows_produced_count").or_else(|| q.get("rows_produced").and_then(Value::as_f64));
    let mut props = vec![(
        "Cifras reales".to_string(),
        "Solo totales de la consulta: la API de Databricks no expone métricas por operador".to_string(),
    )];
    if let Some(id) = q.get("query_id").and_then(Value::as_str) {
        props.push(("Id de consulta".into(), id.to_string()));
    }
    if let Value::Object(map) = &m {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            if let Some(v) = map.get(k).filter(|v| !v.is_null() && !v.is_object() && !v.is_array()) {
                props.push((k.clone(), v.to_string().trim_matches('"').to_string()));
            }
        }
    }
    root.props.splice(0..0, props);
    if f("spill_to_disk_bytes").is_some_and(|b| b > 0.0) {
        root.warnings.push(format!("Derramó {} bytes a disco", f("spill_to_disk_bytes").unwrap_or(0.0) as u64));
    }
    if let (Some(pruned), Some(read)) = (f("pruned_files_count"), f("read_files_count")) {
        if pruned == 0.0 && read > 100.0 {
            p.root.warnings.push(format!("Sin poda de archivos: leyó {} archivos", read as u64));
        }
    }
    p.actual = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATTED: &str = "== Physical Plan ==
AdaptiveSparkPlan (10)
+- HashAggregate (9)
   +- Exchange (8)
      +- HashAggregate (7)
         +- Project (6)
            +- BroadcastHashJoin Inner BuildRight (5)
               :- Filter (2)
               :  +- Scan parquet spark_catalog.default.orders (1)
               +- BroadcastExchange (4)
                  +- Scan parquet spark_catalog.default.customers (3)


(1) Scan parquet spark_catalog.default.orders
Output [3]: [cid#1L, total#2, day#3]
Batched: true
Location: PreparedDeltaFileIndex [dbfs:/user/hive/warehouse/orders]
PartitionFilters: []
PushedFilters: [IsNotNull(day), GreaterThan(day,2024-01-01)]
ReadSchema: struct<cid:bigint,total:double,day:date>

(2) Filter [codegen id : 2]
Input [3]: [cid#1L, total#2, day#3]
Condition : ((isnotnull(day#3) AND (day#3 > 2024-01-01)) AND isnotnull(cid#1L))

(3) Scan parquet spark_catalog.default.customers
Output [2]: [id#4L, name#5]
Batched: true
PartitionFilters: []
PushedFilters: []
ReadSchema: struct<id:bigint,name:string>

(4) BroadcastExchange
Input [2]: [id#4L, name#5]
Arguments: HashedRelationBroadcastMode(List(input[0, bigint, false]),false), [plan_id=42]

(5) BroadcastHashJoin [codegen id : 2]
Left keys [1]: [cid#1L]
Right keys [1]: [id#4L]
Join type: Inner
Join condition: None

(6) Project [codegen id : 2]
Output [2]: [total#2, name#5]
Input [5]: [cid#1L, total#2, day#3, id#4L, name#5]

(7) HashAggregate [codegen id : 2]
Input [2]: [total#2, name#5]
Keys [1]: [name#5]
Functions [1]: [partial_sum(total#2)]

(8) Exchange
Input [2]: [name#5, sum#9]
Arguments: hashpartitioning(name#5, 200), ENSURE_REQUIREMENTS, [plan_id=47]

(9) HashAggregate [codegen id : 3]
Input [2]: [name#5, sum#9]
Keys [1]: [name#5]
Functions [1]: [sum(total#2)]

(10) AdaptiveSparkPlan
Output [2]: [name#5, sum(total)#8]
Arguments: isFinalPlan=false
";

    #[test]
    fn formatted_tree_and_details() {
        let p = formatted("q", FORMATTED);
        assert!(!p.actual);
        let root = &p.root;
        assert_eq!(root.op, "AdaptiveSparkPlan");
        assert_eq!(root.detail, "isFinalPlan=false");
        let join = &root.children[0].children[0].children[0].children[0].children[0];
        assert_eq!(join.op, "BroadcastHashJoin Inner BuildRight");
        assert_eq!(join.detail, "Inner");
        assert!(join.props.iter().any(|(k, v)| k == "Etapa" && v == "codegen id : 2"));
        let [filter, bx] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!(filter.op, "Filter");
        assert!(filter.detail.starts_with("((isnotnull(day#3)"));
        let orders = &filter.children[0];
        assert_eq!((orders.op.as_str(), orders.object.as_deref()), ("Scan parquet", Some("spark_catalog.default.orders")));
        assert!(orders.warnings.is_empty());
        assert!(orders.props.iter().any(|(k, v)| k == "PushedFilters" && v.starts_with("[IsNotNull")));
        let cust = &bx.children[0];
        assert_eq!(cust.object.as_deref(), Some("spark_catalog.default.customers"));
        assert!(cust.warnings.iter().any(|w| w.starts_with("Lee la tabla completa")));
        let agg = &root.children[0];
        assert_eq!(agg.detail, "[name#5] · [sum(total#2)]");
    }

    /// Recorded from Spark 3.5.3 (`spark-sql`), the engine under Databricks.
    #[test]
    fn recorded_spark_output() {
        let p = formatted("q", include_str!("../fixtures/spark35_explain_formatted.txt"));
        let root = &p.root;
        assert_eq!(root.op, "AdaptiveSparkPlan");
        let join = &root.children[0].children[0].children[0].children[0].children[0];
        assert_eq!(join.op, "BroadcastHashJoin Inner BuildRight");
        let [left, right] = &join.children[..] else { panic!("{join:#?}") };
        let scan_o = &left.children[0].children[0];
        assert_eq!((scan_o.op.as_str(), scan_o.object.as_deref()), ("Scan parquet", Some("spark_catalog.default.o")));
        assert!(scan_o.props.iter().any(|(k, _)| k == "PushedFilters"));
        assert_eq!(right.op, "BroadcastExchange");
        assert_eq!(right.children[0].children[0].object.as_deref(), Some("spark_catalog.default.c"));
        assert!(left.children[0].detail.contains("2024-01-01"), "{:?}", left.children[0]);
    }

    #[test]
    fn history_metrics_on_the_root() {
        let mut p = formatted("q", FORMATTED);
        let q: Value = serde_json::from_str(
            r#"{"query_id":"01ef","status":"FINISHED","duration":2100,"rows_produced":12,
                "metrics":{"total_time_ms":2100,"execution_time_ms":1800,"compilation_time_ms":250,"read_bytes":98304000,
                           "rows_produced_count":12,"rows_read_count":5000000,"spill_to_disk_bytes":1048576,
                           "read_files_count":240,"pruned_files_count":0,"photon_total_time_ms":1500}}"#,
        )
        .unwrap();
        add_history(&mut p, &q);
        assert!(p.actual);
        assert_eq!((p.root.actual_ms, p.root.actual_rows), (Some(2100.0), Some(12.0)));
        assert!(p.root.props.iter().any(|(k, v)| k == "read_bytes" && v == "98304000"));
        assert!(p.root.warnings.iter().any(|w| w.contains("1048576")));
        assert!(p.root.warnings.iter().any(|w| w.starts_with("Sin poda")));
    }

    #[test]
    fn classify_statements() {
        assert_eq!(classify("SELECT 1"), StmtKind::Plannable);
        assert_eq!(classify("merge into t using s on 1=1 when matched then delete"), StmtKind::Plannable);
        assert_eq!(classify("CREATE OR REPLACE TABLE t AS SELECT 1"), StmtKind::Plannable);
        assert_eq!(classify("USE CATALOG main"), StmtKind::Other);
        assert_eq!(numbered("Scan parquet t (1)"), ("Scan parquet t".into(), Some(1)));
    }

    #[test]
    fn without_physical_section_falls_back_to_text() {
        let p = formatted("q", "Project [a]\n+- Relation t[a]");
        assert_eq!(p.root.op, "Project [a]");
        assert_eq!(p.root.children[0].op, "Relation t[a]");
    }
}

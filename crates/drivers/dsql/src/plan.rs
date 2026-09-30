//! Execution plans. DSQL speaks PostgreSQL's `EXPLAIN`: `(FORMAT JSON)`
//! with `ANALYZE` when measuring, and the text format as the fallback when
//! an option is refused. Adapted from the postgres driver's parser (this
//! crate can't depend on it), plus DSQL's own scan nodes (`Custom Scan
//! (btree-table)`, `Storage Scan`, `B-Tree Scan`…), whose full scans get a
//! warning.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

/// A sequential scan reading at least this many rows gets a warning.
const BIG_SCAN: f64 = 100_000.0;

/// What a statement does, as far as plans care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StmtKind {
    /// Only reads: running it again to measure it is harmless.
    Read,
    /// Writes, but EXPLAIN can plan it without running it.
    Write,
    /// No plan (DDL, SET, transaction control…).
    Other,
}

pub(crate) fn classify(stmt: &str) -> StmtKind {
    let words = words(stmt);
    match words.first().map(String::as_str) {
        Some("select" | "with" | "values" | "table") => {
            // A data-modifying CTE or SELECT INTO writes.
            if words.iter().any(|w| matches!(w.as_str(), "insert" | "update" | "delete" | "merge" | "into" | "upsert")) {
                StmtKind::Write
            } else {
                StmtKind::Read
            }
        }
        Some("insert" | "update" | "delete" | "merge" | "upsert" | "execute") => StmtKind::Write,
        _ => StmtKind::Other,
    }
}

/// Lower-case words outside string literals.
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

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `1234567.0` as `1234567`, `0.5` as `0.5`.
fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

/// A warning when estimated and actual rows are 10x or more apart.
pub(crate) fn estimate_warning(est: f64, act: f64) -> Option<String> {
    let (lo, hi) = if est < act { (est, act) } else { (act, est) };
    (hi - lo >= 100.0 && hi >= 10.0 * lo.max(1.0))
        .then(|| format!("Estimación de filas errada: {} estimadas, {} reales", fmt_num(est), fmt_num(act)))
}

/// Keys that already are node fields, or tree structure.
const PG_MAPPED: &[&str] = &["Plans", "Node Type"];

/// A plan from `EXPLAIN (FORMAT JSON)` output (a one-element array with
/// `Plan`, and with ANALYZE `Planning Time` / `Execution Time`…).
pub(crate) fn pg_json(statement: &str, raw: &str, actual: bool) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let top = v.get(0).unwrap_or(&v);
    let plan = top.get("Plan").ok_or("el plan JSON no tiene \"Plan\"")?;
    let mut root = pg_node(plan);
    if let Value::Object(m) = top {
        for (k, v) in m {
            if k != "Plan" && !matches!(v, Value::Array(a) if a.is_empty()) {
                root.props.push((k.clone(), text_or_json(v)));
            }
        }
    }
    Ok(Plan { statement: statement.into(), root, actual, raw_format: "json".into(), raw: raw.trim().into() })
}

fn text_or_json(v: &Value) -> String {
    match v {
        Value::Object(_) => v.to_string(),
        Value::Array(items) if items.iter().any(Value::is_object) => v.to_string(),
        _ => text(v),
    }
}

fn pg_node(p: &Value) -> PlanNode {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
    let node_type = s("Node Type");
    let mut op = match node_type {
        "ModifyTable" if !s("Operation").is_empty() => s("Operation").to_string(),
        "Aggregate" => match s("Strategy") {
            "Hashed" => "HashAggregate".into(),
            "Sorted" => "GroupAggregate".into(),
            "Mixed" => "MixedAggregate".into(),
            _ => "Aggregate".into(),
        },
        "SetOp" if s("Strategy") == "Hashed" => "HashSetOp".into(),
        // DSQL's storage access: `Custom Scan (btree-table)` and friends.
        "Custom Scan" if !s("Custom Plan Provider").is_empty() => format!("Custom Scan ({})", s("Custom Plan Provider")),
        t => t.to_string(),
    };
    if matches!(s("Partial Mode"), "Partial" | "Finalize") {
        op = format!("{} {op}", s("Partial Mode"));
    }
    if p.get("Parallel Aware").and_then(Value::as_bool) == Some(true) {
        op = format!("Parallel {op}");
    }

    let mut detail = Vec::new();
    if !s("Join Type").is_empty() {
        detail.push(format!("{} Join", s("Join Type")));
    }
    for k in ["Index Name", "Command", "Subplan Name"] {
        if !s(k).is_empty() {
            detail.push(s(k).to_string());
        }
    }
    if s("Scan Direction") == "Backward" {
        detail.push("Backward".into());
    }

    let object = ["Relation Name", "CTE Name", "Function Name", "Table Function Name"]
        .iter()
        .find(|k| !s(k).is_empty())
        .map(|k| {
            let mut o = s(k).to_string();
            if !s("Schema").is_empty() {
                o = format!("{}.{o}", s("Schema"));
            }
            if !s("Alias").is_empty() && s("Alias") != s(k) {
                o = format!("{o} {}", s("Alias"));
            }
            o
        })
        .or_else(|| (!s("Index Name").is_empty() && node_type.contains("Bitmap")).then(|| s("Index Name").to_string()));

    let est_rows = num(p.get("Plan Rows"));
    let loops = num(p.get("Actual Loops"));
    let per_loop_rows = num(p.get("Actual Rows"));
    let actual_rows = per_loop_rows.map(|r| r * loops.unwrap_or(1.0));
    let actual_ms = num(p.get("Actual Total Time")).map(|t| t * loops.unwrap_or(1.0));

    let mut warnings = Vec::new();
    if node_type == "Seq Scan" {
        let read = match (per_loop_rows, loops) {
            (Some(r), Some(l)) => (r + num(p.get("Rows Removed by Filter")).unwrap_or(0.0)) * l,
            _ => est_rows.unwrap_or(0.0),
        };
        if read >= BIG_SCAN {
            warnings.push(format!("Seq Scan en tabla grande (~{} filas leídas)", fmt_num(read)));
        }
    }
    if full_scan(&op) {
        warnings.push("Recorrido completo de la tabla".into());
    }
    if s("Sort Space Type") == "Disk" || s("Sort Method").starts_with("external") {
        warnings.push(format!("Sort derramó a disco ({})", s("Sort Method")));
    }
    if num(p.get("Hash Batches")).is_some_and(|b| b > 1.0) {
        warnings.push(format!("Hash en {} lotes: derramó a disco", fmt_num(num(p.get("Hash Batches")).unwrap_or(0.0))));
    }
    if num(p.get("HashAgg Batches")).is_some_and(|b| b > 1.0) || num(p.get("Disk Usage")).is_some_and(|d| d > 0.0) {
        warnings.push("Agregación derramó a disco".into());
    }
    if let (Some(e), Some(a), Some(l)) = (est_rows, per_loop_rows, loops) {
        if l > 0.0 {
            warnings.extend(estimate_warning(e, a));
        }
    }
    if loops == Some(0.0) {
        warnings.push("Nunca se ejecutó".into());
    }

    let mut props = Vec::new();
    if let Value::Object(m) = p {
        for (k, v) in m {
            if PG_MAPPED.contains(&k.as_str()) {
                continue;
            }
            // BUFFERS reports a dozen counters per node, most of them zero.
            if k.ends_with(" Blocks") && v.as_f64() == Some(0.0) {
                continue;
            }
            props.push((k.clone(), text_or_json(v)));
        }
    }
    let children = p.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(pg_node).collect()).unwrap_or_default();
    PlanNode {
        op,
        detail: detail.join(" · "),
        object,
        total_cost: num(p.get("Total Cost")),
        self_cost: None,
        est_rows,
        actual_rows,
        executions: loops,
        actual_ms,
        warnings,
        props,
        children,
    }
}

/// Nodes with their depth (indentation column), in output order, into a
/// tree: each node hangs from the closest shallower one before it.
pub(crate) fn build_tree(items: Vec<(usize, PlanNode)>) -> Option<PlanNode> {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    for (depth, node) in items {
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
    stack.pop().map(|(_, n)| n)
}

/// PostgreSQL's text format (Redshift's EXPLAIN, and the fallback when
/// `FORMAT JSON` is refused): `->` opens a node, other lines are
/// `Key: value` properties of the node above them.
pub(crate) fn pg_text(statement: &str, raw: &str, actual: bool) -> Plan {
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    let mut trailer = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("->") {
            items.push((indent + 1, pg_text_node(rest.trim())));
        } else if items.is_empty() {
            items.push((0, pg_text_node(t)));
        } else if indent == 0 && (t.starts_with("Planning Time") || t.starts_with("Execution Time") || t.starts_with("Planning:")) {
            trailer.push(split_prop(t));
        } else if let Some((_, n)) = items.last_mut() {
            let (k, v) = split_prop(t);
            if k == "Sort Method" && v.contains("external") {
                n.warnings.push(format!("Sort derramó a disco ({v})"));
            }
            if k == "Rows Removed by Filter" && n.op.ends_with("Seq Scan") {
                if let (Some(r), Ok(removed)) = (n.actual_rows, v.parse::<f64>()) {
                    let read = r + removed * n.executions.unwrap_or(1.0);
                    if read >= BIG_SCAN && n.warnings.iter().all(|w| !w.starts_with("Seq Scan")) {
                        n.warnings.push(format!("Seq Scan en tabla grande (~{} filas leídas)", fmt_num(read)));
                    }
                }
            }
            n.props.push((k, v));
        }
    }
    let mut root = build_tree(items).unwrap_or_else(|| PlanNode { op: "Plan".into(), ..Default::default() });
    root.props.extend(trailer);
    Plan { statement: statement.into(), root, actual, raw_format: "text".into(), raw: raw.trim_end().into() }
}

fn split_prop(t: &str) -> (String, String) {
    match t.split_once(": ") {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (String::new(), t.to_string()),
    }
}

/// `Index Scan using a_pkey on a  (cost=0.42..8.44 rows=1 width=8) (actual time=… rows=… loops=…)`.
fn pg_text_node(header: &str) -> PlanNode {
    let (name, figures) = match header.find("  (") {
        Some(i) => (&header[..i], &header[i..]),
        None => match header.find(" (cost=") {
            Some(i) => (&header[..i], &header[i..]),
            None => (header, ""),
        },
    };
    let mut node = PlanNode::default();
    let (head, object) = match name.split_once(" on ") {
        Some((h, o)) => (h, Some(o.trim().to_string())),
        None => (name, None),
    };
    let (op, index) = match head.split_once(" using ") {
        Some((o, i)) => (o, Some(i.trim().to_string())),
        None => (head, None),
    };
    node.op = op.trim().to_string();
    node.object = object;
    node.detail = index.unwrap_or_default();

    let field = |group: &str, key: &str| -> Option<String> {
        let i = group.find(key)?;
        Some(group[i + key.len()..].split([' ', ')']).next()?.to_string())
    };
    let (est, act) = match figures.find("(actual") {
        Some(i) => (&figures[..i], &figures[i..]),
        None => (figures, ""),
    };
    if let Some(cost) = field(est, "cost=") {
        if let Some((a, b)) = cost.split_once("..") {
            node.total_cost = b.parse().ok();
            node.props.push(("Startup Cost".into(), a.into()));
        }
    }
    node.est_rows = field(est, "rows=").and_then(|r| r.parse().ok());
    if let Some(w) = field(est, "width=") {
        node.props.push(("Plan Width".into(), w));
    }
    if figures.contains("never executed") {
        node.executions = Some(0.0);
        node.warnings.push("Nunca se ejecutó".into());
    } else if !act.is_empty() {
        let loops: f64 = field(act, "loops=").and_then(|l| l.parse().ok()).unwrap_or(1.0);
        let rows: Option<f64> = field(act, "rows=").and_then(|r| r.parse().ok());
        node.executions = Some(loops);
        node.actual_rows = rows.map(|r| r * loops);
        node.actual_ms = field(act, "time=")
            .and_then(|t| t.split_once("..").and_then(|(_, b)| b.parse::<f64>().ok()))
            .map(|t| t * loops);
        if let (Some(e), Some(r)) = (node.est_rows, rows) {
            node.warnings.extend(estimate_warning(e, r));
        }
    }
    if node.op.ends_with("Seq Scan") && node.actual_rows.or(node.est_rows).is_some_and(|r| r >= BIG_SCAN) {
        let r = node.actual_rows.or(node.est_rows).unwrap_or(0.0);
        node.warnings.push(format!("Seq Scan en tabla grande (~{} filas leídas)", fmt_num(r)));
    }
    if full_scan(&node.op) {
        node.warnings.push("Recorrido completo de la tabla".into());
    }
    node
}

/// DSQL's `Full Scan (btree-table)`: every row of the table is read.
fn full_scan(op: &str) -> bool {
    op.starts_with("Full Scan")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG_ANALYZE: &str = r#"[
  {
    "Plan": {
      "Node Type": "Sort", "Parallel Aware": false, "Startup Cost": 531.84, "Total Cost": 531.86,
      "Plan Rows": 10, "Plan Width": 12, "Actual Startup Time": 1.100, "Actual Total Time": 1.100,
      "Actual Rows": 10, "Actual Loops": 1, "Sort Key": ["(count(*)) DESC"], "Sort Method": "quicksort",
      "Sort Space Used": 25, "Sort Space Type": "Memory", "Shared Hit Blocks": 41, "Shared Read Blocks": 0,
      "Plans": [
        {
          "Node Type": "Aggregate", "Strategy": "Hashed", "Partial Mode": "Simple", "Parent Relationship": "Outer",
          "Parallel Aware": false, "Startup Cost": 531.57, "Total Cost": 531.67, "Plan Rows": 10,
          "Actual Total Time": 1.093, "Actual Rows": 10, "Actual Loops": 1, "Group Key": ["a.g"],
          "Plans": [
            {
              "Node Type": "Nested Loop", "Join Type": "Inner", "Parallel Aware": false, "Total Cost": 530.05,
              "Plan Rows": 3, "Actual Total Time": 1.073, "Actual Rows": 310, "Actual Loops": 1,
              "Plans": [
                {
                  "Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "b", "Alias": "b",
                  "Total Cost": 73.00, "Plan Rows": 50000, "Actual Total Time": 0.182, "Actual Rows": 25000,
                  "Actual Loops": 5, "Filter": "(a_id > 0)", "Rows Removed by Filter": 10
                },
                {
                  "Node Type": "Index Scan", "Scan Direction": "Forward", "Index Name": "a_pkey",
                  "Relation Name": "a", "Schema": "public", "Alias": "x", "Total Cost": 0.5, "Plan Rows": 1,
                  "Actual Total Time": 0.002, "Actual Rows": 1, "Actual Loops": 310, "Index Cond": "(id = b.a_id)"
                },
                {
                  "Node Type": "Sort", "Total Cost": 462.69, "Plan Rows": 5000, "Actual Total Time": 0.633,
                  "Actual Rows": 5000, "Actual Loops": 1, "Sort Method": "external merge", "Sort Space Type": "Disk"
                }
              ]
            }
          ]
        }
      ]
    },
    "Planning Time": 0.196,
    "Triggers": [],
    "Execution Time": 1.158
  }
]"#;

    #[test]
    fn pg_json_tree_and_figures() {
        let p = pg_json("select 1", PG_ANALYZE, true).unwrap();
        assert!(p.actual);
        assert_eq!(p.raw_format, "json");
        let root = &p.root;
        assert_eq!(root.op, "Sort");
        assert_eq!(root.total_cost, Some(531.86));
        assert!(root.props.iter().any(|(k, v)| k == "Execution Time" && v == "1.158"));
        assert!(root.props.iter().any(|(k, v)| k == "Sort Key" && v == "(count(*)) DESC"));
        assert!(!root.props.iter().any(|(k, _)| k == "Shared Read Blocks" || k == "Triggers"));
        let agg = &root.children[0];
        assert_eq!(agg.op, "HashAggregate");
        let join = &agg.children[0];
        assert_eq!(join.op, "Nested Loop");
        assert_eq!(join.detail, "Inner Join");
        assert!(join.warnings.iter().any(|w| w.starts_with("Estimación")), "{:?}", join.warnings);
        let [scan, idx, sort] = &join.children[..] else { panic!() };
        assert_eq!(scan.op, "Parallel Seq Scan");
        assert_eq!(scan.object.as_deref(), Some("b"));
        assert_eq!(scan.actual_rows, Some(125_000.0));
        assert_eq!(scan.executions, Some(5.0));
        assert!((scan.actual_ms.unwrap() - 0.91).abs() < 1e-9);
        assert!(scan.warnings.iter().any(|w| w.starts_with("Seq Scan en tabla grande")));
        assert_eq!(idx.detail, "a_pkey");
        assert_eq!(idx.object.as_deref(), Some("public.a x"));
        assert_eq!(idx.actual_rows, Some(310.0));
        assert!(idx.warnings.is_empty());
        assert!(sort.warnings.iter().any(|w| w.contains("external merge")));
    }

    #[test]
    fn pg_json_estimated_delete() {
        let raw = r#"[{"Plan": {"Node Type": "ModifyTable", "Operation": "Delete", "Relation Name": "b", "Alias": "b",
            "Total Cost": 8.44, "Plan Rows": 0, "Plans": [{"Node Type": "Index Scan", "Index Name": "b_pkey",
            "Relation Name": "b", "Alias": "b", "Total Cost": 8.44, "Plan Rows": 9, "Index Cond": "(id < 10)"}]}}]"#;
        let p = pg_json("delete from b where id < 10", raw, false).unwrap();
        assert_eq!(p.root.op, "Delete");
        assert_eq!(p.root.actual_rows, None);
        assert_eq!(p.root.children[0].est_rows, Some(9.0));
        assert!(p.root.children[0].props.iter().any(|(k, v)| k == "Index Cond" && v == "(id < 10)"));
    }

    #[test]
    fn pg_text_format() {
        let raw = "\
Sort  (cost=531.84..531.86 rows=10 width=12) (actual time=1.100..1.100 rows=10 loops=1)
  Sort Key: (count(*)) DESC
  Sort Method: quicksort  Memory: 25kB
  ->  HashAggregate  (cost=531.57..531.67 rows=10 width=12) (actual time=1.093..1.093 rows=10 loops=1)
        Group Key: a.g
        ->  Hash Join  (cost=10.00..530.05 rows=303 width=4) (actual time=0.520..1.073 rows=310 loops=1)
              Hash Cond: (b.a_id = a.id)
              ->  Seq Scan on b  (cost=0.00..73.00 rows=5000 width=4) (actual time=0.003..0.182 rows=5000 loops=1)
              ->  Hash  (cost=5.00..5.00 rows=100 width=8) (never executed)
                    ->  Index Scan using a_pkey on a  (cost=0.42..7.42 rows=100 width=8) (never executed)
Planning Time: 0.196 ms
Execution Time: 1.158 ms";
        let p = pg_text("q", raw, true);
        let root = &p.root;
        assert_eq!(root.op, "Sort");
        assert_eq!(root.total_cost, Some(531.86));
        assert!(root.props.iter().any(|(k, _)| k == "Execution Time"));
        let join = &root.children[0].children[0];
        assert_eq!(join.op, "Hash Join");
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].object.as_deref(), Some("b"));
        assert_eq!(join.children[0].actual_rows, Some(5000.0));
        let idx = &join.children[1].children[0];
        assert_eq!((idx.op.as_str(), idx.detail.as_str(), idx.object.as_deref()), ("Index Scan", "a_pkey", Some("a")));
        assert_eq!(idx.executions, Some(0.0));
    }

    #[test]
    fn dsql_text_full_scan() {
        let raw = "\
Full Scan (btree-table) on orders  (cost=100.76..104.68 rows=7 width=45) (actual time=1.12..1.20 rows=7 loops=1)
  -> Storage Scan on orders  (cost=100.76..104.68 rows=7 width=45) (actual rows=7 loops=1)
      Projections: id, amount
      -> B-Tree Scan on orders  (cost=100.76..104.68 rows=7 width=45) (actual rows=7 loops=1)
Planning Time: 0.1 ms
Execution Time: 1.3 ms";
        let p = pg_text("select * from orders", raw, true);
        assert_eq!((p.root.op.as_str(), p.root.object.as_deref()), ("Full Scan (btree-table)", Some("orders")));
        assert!(p.root.warnings.iter().any(|w| w == "Recorrido completo de la tabla"));
        let storage = &p.root.children[0];
        assert_eq!(storage.op, "Storage Scan");
        assert!(storage.props.iter().any(|(k, _)| k == "Projections"));
        assert_eq!(storage.children[0].op, "B-Tree Scan");
    }

    #[test]
    fn dsql_json_custom_scan() {
        let raw = r#"[{"Plan": {"Node Type": "Custom Scan", "Custom Plan Provider": "btree-table", "Relation Name": "t",
            "Total Cost": 104.5, "Plan Rows": 3}}]"#;
        let p = pg_json("select * from t", raw, false).unwrap();
        assert_eq!(p.root.op, "Custom Scan (btree-table)");
        assert_eq!(p.root.object.as_deref(), Some("t"));
    }

    #[test]
    fn statement_kinds() {
        assert_eq!(classify("SELECT * FROM t"), StmtKind::Read);
        assert_eq!(classify("with x as (select 1) select * from x"), StmtKind::Read);
        assert_eq!(classify("with x as (delete from t returning *) select * from x"), StmtKind::Write);
        assert_eq!(classify("select * into t2 from t"), StmtKind::Write);
        assert_eq!(classify("select 'delete' from t"), StmtKind::Read);
        assert_eq!(classify("UPDATE t SET a = 1"), StmtKind::Write);
        assert_eq!(classify("create table t (a int)"), StmtKind::Other);
    }

    #[test]
    fn estimate_warning_needs_a_real_gap() {
        assert!(estimate_warning(1.0, 5.0).is_none());
        assert!(estimate_warning(10.0, 50.0).is_none());
        assert!(estimate_warning(10.0, 500.0).is_some());
        assert!(estimate_warning(10_000.0, 12.0).is_some());
    }
}

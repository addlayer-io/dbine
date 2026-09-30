//! Trino / Presto plans as [`PlanNode`] trees: the distributed
//! `EXPLAIN (FORMAT JSON)` (one tree per fragment, stitched together at
//! each `RemoteSource`) and the text of `EXPLAIN ANALYZE` (the same
//! fragments drawn with `└─`, with measured CPU time and output rows).
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

fn estimate_warning(est: f64, act: f64) -> Option<String> {
    let (lo, hi) = if est < act { (est, act) } else { (act, est) };
    (hi - lo >= 100.0 && hi >= 10.0 * lo.max(1.0))
        .then(|| format!("Estimación de filas errada: {} estimadas, {} reales", fmt_num(est), fmt_num(act)))
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

// ---- EXPLAIN ANALYZE text -----------------------------------------------

/// `Fragment 1 [SINGLE]` blocks, each a header then an operator tree drawn
/// with `└─` / `├─` and `│`, properties indented under each operator.
pub(crate) fn analyze_text(statement: &str, raw: &str) -> Plan {
    let mut preamble = Vec::new();
    // Fragment id → (header props, items).
    let mut fragments: Vec<(String, Vec<(String, String)>, Vec<(usize, PlanNode)>)> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Fragment ") {
            let id = rest.split_whitespace().next().unwrap_or("").to_string();
            fragments.push((id, vec![("Fragmento".into(), rest.trim().to_string())], Vec::new()));
            continue;
        }
        let Some((_, header, items)) = fragments.last_mut() else {
            preamble.push(split_prop(line.trim()));
            continue;
        };
        let lead: String = line.chars().take_while(|c| matches!(c, ' ' | '│' | '└' | '├' | '─')).collect();
        let t = line[lead.len()..].trim();
        let branch = lead.chars().position(|c| c == '└' || c == '├');
        if let Some(depth) = branch {
            items.push((depth + 1, text_node(t)));
        } else if items.is_empty() && is_operator(t) {
            items.push((0, text_node(t)));
        } else if let Some((_, n)) = items.last_mut() {
            text_prop(n, t);
        } else {
            header.push(split_prop(t));
        }
    }
    let mut trees: BTreeMap<String, PlanNode> = BTreeMap::new();
    let mut order = Vec::new();
    for (id, header, items) in fragments {
        if let Some(mut root) = build_tree(items) {
            root.props.splice(0..0, header);
            order.push(id.clone());
            trees.insert(id, root);
        }
    }
    let mut root = match order.first().and_then(|id| trees.remove(id)) {
        Some(mut r) => {
            stitch(&mut r, &mut trees, &remote_ids);
            r
        }
        None => PlanNode { op: "Query".into(), ..Default::default() },
    };
    root.props.splice(0..0, preamble);
    cumulate(&mut root);
    Plan { statement: statement.into(), root, actual: true, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// `TopN[count = 5]` or `Values`: an identifier, then `[` or nothing.
fn is_operator(t: &str) -> bool {
    let name: String = t.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    !name.is_empty() && (t.len() == name.len() || t[name.len()..].starts_with('['))
}

fn split_prop(t: &str) -> (String, String) {
    match t.split_once(": ") {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (String::new(), t.to_string()),
    }
}

/// `InnerJoin[criteria = (a = b), distribution = REPLICATED]`.
fn text_node(t: &str) -> PlanNode {
    let (op, args) = match t.find('[') {
        Some(i) => (&t[..i], t[i + 1..].strip_suffix(']').unwrap_or(&t[i + 1..])),
        None => (t, ""),
    };
    let mut n = PlanNode { op: op.trim().to_string(), ..Default::default() };
    let mut detail = Vec::new();
    for (k, v) in top_level_args(args) {
        if k == "table" {
            n.object = Some(v.clone());
        } else if matches!(k.as_str(), "type" | "criteria") {
            detail.push(v.clone());
        } else if matches!(k.as_str(), "orderBy" | "partitioning" | "count" | "keys") {
            detail.push(format!("{k} = {v}"));
        }
        n.props.push((k, v));
    }
    n.detail = detail.join(" · ");
    n
}

/// `a = 1, b = [x, y]` → [(a, 1), (b, [x, y])], splitting on commas
/// outside brackets and parentheses.
fn top_level_args(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| match p.split_once(" = ") {
            Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
            None => (String::new(), p.trim().to_string()),
        })
        .collect()
}

/// `1364`, `1.5K`, `2M` → a number.
fn count(s: &str) -> Option<f64> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1e3),
        'M' => (&s[..s.len() - 1], 1e6),
        'B' => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    digits.parse::<f64>().ok().map(|n| n * mult)
}

/// `8.00ms`, `1.2s`, `0.00ns`, `1.11m` → milliseconds.
fn duration_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let n: f64 = s[..split].parse().ok()?;
    Some(
        n * match &s[split..] {
            "ns" => 1e-6,
            "us" | "µs" => 1e-3,
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            _ => return None,
        },
    )
}

fn text_prop(n: &mut PlanNode, t: &str) {
    if let Some(est) = t.strip_prefix("Estimates: ") {
        // Several `{…}/{…}` for fused operators: the last is the output.
        let last = est.rsplit('/').next().unwrap_or(est).trim_matches(['{', '}']);
        for (k, v) in last.split(", ").filter_map(|p| p.split_once(": ")) {
            match k {
                "rows" => n.est_rows = v.split_whitespace().next().and_then(count),
                "cpu" => n.self_cost = count(v),
                _ => {}
            }
        }
        n.props.push(("Estimates".into(), est.to_string()));
        return;
    }
    if t.starts_with("CPU: ") {
        for (k, v) in t.split(", ").filter_map(|p| p.split_once(": ")) {
            match k {
                "CPU" => n.actual_ms = v.split_whitespace().next().and_then(duration_ms),
                "Output" => n.actual_rows = v.split_whitespace().next().and_then(count),
                _ => {}
            }
            n.props.push((k.to_string(), v.to_string()));
        }
        if let (Some(e), Some(a)) = (n.est_rows, n.actual_rows) {
            n.warnings.extend(estimate_warning(e, a));
        }
        return;
    }
    let (k, v) = match t.split_once(" := ") {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => split_prop(t),
    };
    n.props.push((k, v));
}

fn build_tree(items: Vec<(usize, PlanNode)>) -> Option<PlanNode> {
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

    const ANALYZE: &str = "Trino version: 483
Queued: 536.08us, Analysis: 3.57ms, Planning: 39.25ms, Execution: 977.85ms, Finishing: 0.00ns
Fragment 1 [SINGLE]
    CPU: 4.77ms, Scheduled: 25.08ms, Blocked 24.60s (Input: 23.87s, Output: 0.00ns), Input: 5 rows (107B)
    Peak Memory: 328B, Tasks count: 1; per task: max: 328B
    Output layout: [name, count]
    Output partitioning: SINGLE []
    TopN[count = 5, orderBy = [count DESC NULLS LAST]]
    │   Layout: [name:varchar(25), count:bigint]
    │   Estimates: {rows: 5 (105B), cpu: ?, memory: ?, network: ?}
    │   CPU: 0.00ns (0.00%), Scheduled: 0.00ns (0.00%), Blocked: 0.00ns (0.00%), Output: 5 rows (107B)
    └─ RemoteSource[sourceFragmentIds = [3]]
           Layout: [name:varchar(25), count:bigint]
           CPU: 0.00ns (0.00%), Scheduled: 0.00ns (0.00%), Blocked: 23.87s (21.88%), Output: 5 rows (107B)

Fragment 3 [SOURCE]
    CPU: 57.58ms, Scheduled: 2.04s, Blocked 18.59s (Input: 17.48s, Output: 0.00ns), Input: 1525 rows (26.88kB)
    Output partitioning: HASH [name]
    InnerJoin[criteria = (nationkey_2 = nationkey), distribution = REPLICATED]
    │   Estimates: {rows: 1364 (28.08kB), cpu: 52.58k, memory: 527B, network: 0B}
    │   CPU: 12.00ms (1.80%), Scheduled: 19.00ms (0.68%), Blocked: 68.00ms (0.06%), Output: 100 rows (2.06kB)
    │   Distribution: REPLICATED
    ├─ ScanFilterProject[table = tpch:tiny:customer, filterPredicate = (double '0.0' < acctbal)]
    │         Estimates: {rows: 1500 (13.18kB), cpu: 26.37k, memory: 0B, network: 0B}/{rows: 1364 (11.99kB), cpu: 11.99k, memory: 0B, network: 0B}
    │         CPU: 12.00ms (32.43%), Scheduled: 14.00ms (11.02%), Blocked: 0.00ns (0.00%), Output: 1361 rows (11.96kB)
    │         acctbal := tpch:acctbal
    │         Dynamic filters:
    │             - df_451, [ SortedRangeSet[type=bigint, ranges=25, {[0], ..., [24]}] ], collection time=54.02ms
    └─ TableScan[table = tpch:tiny:nation]
           Estimates: {rows: 25 (527B), cpu: 527, memory: 0B, network: 0B}
           CPU: 1.00ms (2.70%), Scheduled: 2.00ms (1.57%), Blocked: 0.00ns (0.00%), Output: 25 rows (527B)
";

    #[test]
    fn analyze_text_tree() {
        let p = analyze_text("q", ANALYZE);
        assert!(p.actual);
        let root = &p.root;
        assert_eq!((root.op.as_str(), root.detail.as_str()), ("TopN", "count = 5 · orderBy = [count DESC NULLS LAST]"));
        assert_eq!(root.props[0], ("Trino version".into(), "483".into()));
        assert!(root.props.iter().any(|(k, v)| k == "Fragmento" && v == "1 [SINGLE]"));
        assert_eq!(root.actual_rows, Some(5.0));
        let remote = &root.children[0];
        assert_eq!(remote.op, "RemoteSource");
        let join = &remote.children[0];
        assert_eq!(join.op, "InnerJoin");
        assert_eq!(join.est_rows, Some(1364.0));
        assert_eq!(join.self_cost, Some(52_580.0));
        assert!(join.warnings.iter().any(|w| w.starts_with("Estimación")));
        assert_eq!(join.actual_ms, Some(12.0));
        let [scan, nation] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!(scan.object.as_deref(), Some("tpch:tiny:customer"));
        assert_eq!((scan.est_rows, scan.actual_rows), (Some(1364.0), Some(1361.0)));
        assert!(scan.props.iter().any(|(k, v)| k == "acctbal" && v == "tpch:acctbal"));
        assert_eq!(nation.actual_rows, Some(25.0));
        assert!(root.total_cost.is_some());
    }

    #[test]
    fn statement_kinds() {
        assert_eq!(classify("SELECT 1"), StmtKind::Read);
        assert_eq!(classify("insert into t select 1"), StmtKind::Write);
        assert_eq!(classify("create table t as select 1"), StmtKind::Write);
        assert_eq!(classify("create table t (a int)"), StmtKind::Other);
        assert_eq!(duration_ms("1.11m"), Some(66_600.0));
        assert_eq!(count("52.58k"), Some(52_580.0));
    }
}

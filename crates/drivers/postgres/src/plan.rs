//! Execution plans: PostgreSQL's `EXPLAIN (FORMAT JSON)`, its text format
//! (Redshift, and the fallback for variants without JSON) and
//! CockroachDB's `•` tree, all turned into [`PlanNode`] trees.

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
    node
}

/// H2's EXPLAIN: the statement as it will run, with the access path of
/// each table in `/* … */` comments (`/* public.t.tableScan */`,
/// `/* public.PK_IX: ID = 1 */`). One node per table access under the
/// statement.
pub(crate) fn h2_text(statement: &str, raw: &str) -> Plan {
    let op = raw.split_whitespace().next().unwrap_or("Plan").to_uppercase();
    let mut root = PlanNode { op, ..Default::default() };
    let mut rest = raw;
    while let Some(i) = rest.find("/*") {
        let Some(j) = rest[i..].find("*/") else { break };
        let c = rest[i + 2..i + j].trim();
        rest = &rest[i + j + 2..];
        let mut n = PlanNode::default();
        if let Some(obj) = c.strip_suffix(".tableScan") {
            n.op = "Table scan".into();
            n.object = Some(obj.to_string());
        } else if let Some((ix, cond)) = c.split_once(": ") {
            n.op = "Index".into();
            n.object = Some(ix.to_string());
            n.detail = cond.to_string();
        } else {
            n.op = c.to_string();
        }
        root.children.push(n);
    }
    Plan { statement: statement.into(), root, actual: false, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// Operator trees drawn as text, one operator per line: RisingWave
/// (`└─BatchScan { table: t, columns: [a] }`), CrateDB
/// (`└ Collect[doc.t | [a] | true] (rows=12)`) and Materialize
/// (`→Read db.public.t`, whose details go on deeper lines without `→`;
/// lines outside the tree describe the whole plan).
pub(crate) fn tree_text(statement: &str, raw: &str) -> Plan {
    let arrows = raw.contains('→');
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    let mut plan_props = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let body = line.trim_start_matches([' ', '│', '├', '└', '─', '→']).trim_end();
        let depth = line.chars().count() - line.trim_start_matches([' ', '│', '├', '└', '─', '→']).chars().count();
        if !arrows {
            items.push((depth, tree_node(body)));
        } else if line.trim_start().starts_with('→') {
            items.push((depth, tree_node(body)));
        } else if let Some((_, n)) = items.last_mut().filter(|(d, _)| depth > *d) {
            n.props.push(split_prop(body));
        } else if body != "Explained Query:" {
            plan_props.push(split_prop(body));
        }
    }
    let mut root = build_tree(items).unwrap_or_else(|| PlanNode { op: "Plan".into(), ..Default::default() });
    root.props.splice(0..0, plan_props);
    Plan { statement: statement.into(), root, actual: false, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// `BatchScan { table: t, … }`, `Collect[doc.t | [a] | true] (rows=12)`,
/// `Read db.public.t`: the operator, its detail, the object it reads and
/// the estimated rows when given.
fn tree_node(text: &str) -> PlanNode {
    let cut = [text.find(" {"), text.find('['), text.find(" (")].into_iter().flatten().min();
    let (op, detail) = match cut {
        Some(i) => (text[..i].trim().to_string(), text[i..].trim().to_string()),
        None => (text.to_string(), String::new()),
    };
    let mut n = PlanNode { op, detail, ..Default::default() };
    if let Some(rest) = n.op.strip_prefix("Read ") {
        n.object = Some(rest.trim().to_string());
        n.op = "Read".into();
    } else if let Some(i) = n.detail.find("table: ") {
        let t: String = n.detail[i + 7..].chars().take_while(|c| !matches!(c, ',' | ' ' | '}')).collect();
        n.object = Some(t);
    } else if let Some(inner) = n.detail.strip_prefix('[') {
        let first = inner.split([' ', '|', ']']).next().unwrap_or("");
        if first.contains('.') {
            n.object = Some(first.to_string());
        }
    }
    if let Some(i) = n.detail.rfind("(rows=") {
        let v: String = n.detail[i + 6..].chars().take_while(|c| c.is_ascii_digit()).collect();
        n.est_rows = v.parse().ok();
    }
    n
}

/// CockroachDB's `EXPLAIN (VERBOSE)` / `EXPLAIN ANALYZE` tree: `• op`
/// lines drawn with `│ ├── └──`, each followed by its `key: value` lines.
/// Lines before the tree describe the whole plan; the ones after it
/// (index recommendations) too.
pub(crate) fn cockroach_text(statement: &str, raw: &str, actual: bool) -> Plan {
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    let mut plan_props = Vec::new();
    let mut after_tree = false;
    for line in raw.lines() {
        if line.trim().is_empty() {
            if !items.is_empty() {
                after_tree = true;
            }
            continue;
        }
        if !after_tree {
            if let Some(i) = line.chars().position(|c| c == '•') {
                let op_text: String = line.chars().skip(i + 1).collect();
                items.push((i, cockroach_node(op_text.trim())));
                continue;
            }
        }
        let t = line.trim_start_matches([' ', '│', '├', '└', '─']).trim();
        if t.is_empty() {
            continue;
        }
        let (k, v) = split_prop(t);
        match items.last_mut() {
            Some((_, n)) if !after_tree => cockroach_prop(n, k, v),
            _ => plan_props.push((k, v)),
        }
    }
    let mut root = build_tree(items).unwrap_or_else(|| PlanNode { op: "plan".into(), ..Default::default() });
    if let Some((_, n)) = plan_props.iter().find(|(k, _)| k == "index recommendations") {
        root.warnings.push(format!("CockroachDB recomienda {n} índice(s)"));
    }
    root.props.splice(0..0, plan_props);
    Plan { statement: statement.into(), root, actual, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// `hash join (inner)` → op `hash join`, detail `inner`.
fn cockroach_node(text: &str) -> PlanNode {
    let (op, detail) = match text.split_once(" (") {
        Some((o, d)) => (o.to_string(), d.trim_end_matches(')').to_string()),
        None => (text.to_string(), String::new()),
    };
    PlanNode { op, detail, ..Default::default() }
}

/// `1,234 (100% of the table; stats collected…)` → 1234.
fn leading_count(v: &str) -> Option<f64> {
    v.split_whitespace().next()?.replace(',', "").parse().ok()
}

/// `318µs`, `5ms`, `1.2s`, `1m2s` → milliseconds.
pub(crate) fn duration_ms(v: &str) -> Option<f64> {
    let v = v.trim();
    let mut total = 0.0;
    let mut number = String::new();
    let mut chars = v.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_digit() || n == '.' || n == ' ' {
                break;
            }
            unit.push(n);
            chars.next();
        }
        let n: f64 = number.parse().ok()?;
        number.clear();
        total += n * match unit.as_str() {
            "ns" => 1e-6,
            "µs" | "us" => 1e-3,
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        };
        any = true;
        if chars.peek() == Some(&' ') {
            break;
        }
    }
    any.then_some(total)
}

fn cockroach_prop(n: &mut PlanNode, k: String, v: String) {
    match k.as_str() {
        "estimated row count" => n.est_rows = leading_count(&v),
        "actual row count" => n.actual_rows = leading_count(&v),
        "execution time" => n.actual_ms = duration_ms(&v),
        "table" => n.object = Some(v.clone()),
        "from" | "into" if n.object.is_none() => n.object = Some(v.clone()),
        "spans" if v == "FULL SCAN" => {
            let rows = n.actual_rows.or(n.est_rows).unwrap_or(0.0);
            if rows >= BIG_SCAN {
                n.warnings.push(format!("Recorrido completo de tabla grande (~{} filas)", fmt_num(rows)));
            }
        }
        _ => {}
    }
    if let (Some(e), Some(a)) = (n.est_rows, n.actual_rows) {
        if matches!(k.as_str(), "estimated row count" | "actual row count") {
            n.warnings.extend(estimate_warning(e, a));
        }
    }
    n.props.push((k, v));
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
    fn redshift_text_without_actuals() {
        let raw = "XN Hash Join DS_DIST_NONE  (cost=0.00..1000.00 rows=100 width=8)\n  Hash Cond: (\"outer\".a = \"inner\".a)\n  ->  XN Seq Scan on sales  (cost=0.00..17.24 rows=1724 width=4)\n  ->  XN Hash  (cost=0.00..1.00 rows=10 width=4)\n        ->  XN Seq Scan on users  (cost=0.00..1.00 rows=10 width=4)";
        let p = pg_text("q", raw, false);
        assert_eq!(p.root.op, "XN Hash Join DS_DIST_NONE");
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[1].children[0].object.as_deref(), Some("users"));
        assert_eq!(p.root.props[0].0, "Startup Cost");
    }

    const CRDB_ANALYZE: &str = "planning time: 248µs
execution time: 5ms
distribution: local
rows decoded from KV: 25,000 (1.4 MiB, 2 gRPC calls)

• sort
│ sql nodes: n1
│ execution time: 6µs
│ actual row count: 10
│ order: -count_rows
│
└── • group (hash)
    │ actual row count: 10
    │ group by: g
    │
    └── • hash join
        │ execution time: 263µs
        │ actual row count: 310
        │ equality: (a_id) = (id)
        │
        ├── • scan
        │     actual row count: 5,000
        │     missing stats
        │     table: b@b_pkey
        │     spans: FULL SCAN
        │
        └── • filter
            │ actual row count: 1,284
            │ estimated row count: 111 (missing stats)
            │ filter: s LIKE 'a%'
            │
            └── • scan
                  actual row count: 200,000
                  table: a@a_pkey
                  spans: FULL SCAN

index recommendations: 1
1. type: index creation
   SQL command: CREATE INDEX ON defaultdb.public.b (a_id);";

    #[test]
    fn cockroach_tree() {
        let p = cockroach_text("q", CRDB_ANALYZE, true);
        let root = &p.root;
        assert_eq!(root.op, "sort");
        assert_eq!(root.actual_ms, Some(0.006));
        assert_eq!(root.props[0], ("planning time".into(), "248µs".into()));
        assert!(root.props.iter().any(|(_, v)| v.starts_with("CREATE INDEX")));
        assert!(root.warnings.iter().any(|w| w.contains("recomienda 1")));
        let group = &root.children[0];
        assert_eq!((group.op.as_str(), group.detail.as_str()), ("group", "hash"));
        let join = &group.children[0];
        assert_eq!(join.children.len(), 2);
        let scan_b = &join.children[0];
        assert_eq!(scan_b.object.as_deref(), Some("b@b_pkey"));
        assert_eq!(scan_b.actual_rows, Some(5000.0));
        assert!(scan_b.props.iter().any(|(k, v)| k.is_empty() && v == "missing stats"));
        let filter = &join.children[1];
        assert!(filter.warnings.iter().any(|w| w.starts_with("Estimación")));
        let scan_a = &filter.children[0];
        assert_eq!(scan_a.actual_rows, Some(200_000.0));
        assert!(scan_a.warnings.iter().any(|w| w.contains("Recorrido completo")));
    }

    #[test]
    #[allow(clippy::approx_constant)] // 0.318 ms, not 1/π
    fn durations() {
        assert_eq!(duration_ms("5ms"), Some(5.0));
        assert_eq!(duration_ms("318µs"), Some(0.318));
        assert_eq!(duration_ms("1.5s"), Some(1500.0));
        assert_eq!(duration_ms("1m2s"), Some(62_000.0));
        assert_eq!(duration_ms("x"), None);
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

    #[test]
    fn operator_trees_as_text() {
        let rw = "BatchExchange { order: [], dist: Single }\n└─BatchFilter { predicate: (t1.id = 1:Int32) }\n  └─BatchScan { table: t1, columns: [id, v] }";
        let p = tree_text("q", rw);
        assert_eq!(p.root.op, "BatchExchange");
        let scan = &p.root.children[0].children[0];
        assert_eq!(scan.op, "BatchScan");
        assert_eq!(scan.object.as_deref(), Some("t1"));

        let crate_db = "Eval[id, name] (rows=12)\n  └ Collect[doc.t1 | [id, name] | true] (rows=12)";
        let p = tree_text("q", crate_db);
        assert_eq!(p.root.op, "Eval");
        assert_eq!(p.root.est_rows, Some(12.0));
        assert_eq!(p.root.children[0].object.as_deref(), Some("doc.t1"));

        let mz = "Explained Query:\n  →Accumulable GroupAggregate\n    Simple aggregates: count(*)\n    →Read materialize.public.t1\n\nSource materialize.public.t1\n  project=(#1)\n\nTarget cluster: quickstart\n";
        let p = tree_text("q", mz);
        assert_eq!(p.root.op, "Accumulable GroupAggregate");
        assert_eq!(p.root.children[0].op, "Read");
        assert_eq!(p.root.children[0].object.as_deref(), Some("materialize.public.t1"));
        assert!(p.root.props.iter().any(|(k, v)| k == "Target cluster" && v == "quickstart"));
        assert!(p.root.props.iter().any(|(k, v)| k == "Simple aggregates" && v == "count(*)"));
    }

    #[test]
    fn h2_access_paths() {
        let raw = "SELECT\n    \"public\".\"t1\".\"id\"\nFROM \"public\".\"t1\"\n    /* public.t1.tableScan */\nINNER JOIN \"public\".\"t2\"\n    /* public.PRIMARY_KEY_A: ID = 1 */";
        let p = h2_text("q", raw);
        assert_eq!(p.root.op, "SELECT");
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[0].object.as_deref(), Some("public.t1"));
        assert_eq!(p.root.children[1].detail, "ID = 1");
    }
}

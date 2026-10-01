//! Execution plans of the MySQL family, as [`PlanNode`] trees:
//! MySQL 8's `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE`, MariaDB's
//! `EXPLAIN|ANALYZE FORMAT=JSON`, TiDB's `EXPLAIN` rows drawn with `└─`,
//! and, for everything else, the classic tabular `EXPLAIN` or a
//! one-column text plan read by indentation.

use crate::Variant;
use dbine_driver::{Plan, PlanNode};
use serde_json::Value;

/// A full scan reading at least this many rows gets a warning.
const BIG_SCAN: f64 = 100_000.0;

/// How the server explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// MySQL 8.0.16+: `FORMAT=TREE`; `EXPLAIN ANALYZE` since 8.0.18.
    MySqlTree { analyze: bool },
    MariaDb,
    TiDb,
    /// Plain `EXPLAIN`: MySQL 5.7's table, or whatever the engine returns.
    Plain,
}

impl Flavor {
    pub(crate) fn detect(variant: Variant, version: &str) -> Flavor {
        if variant == Variant::MariaDb || version.contains("MariaDB") {
            return Flavor::MariaDb;
        }
        if variant == Variant::TiDb || version.contains("TiDB") {
            return Flavor::TiDb;
        }
        if variant == Variant::MySql {
            let v: Vec<u32> = version
                .split(|c: char| !c.is_ascii_digit())
                .take(3)
                .map(|p| p.parse().unwrap_or(0))
                .collect();
            let v = (v.first().copied().unwrap_or(0), v.get(1).copied().unwrap_or(0), v.get(2).copied().unwrap_or(0));
            if v >= (8, 0, 16) {
                return Flavor::MySqlTree { analyze: v >= (8, 0, 18) };
            }
        }
        Flavor::Plain
    }

    pub(crate) fn can_analyze(self) -> bool {
        matches!(self, Flavor::MySqlTree { analyze: true } | Flavor::MariaDb | Flavor::TiDb)
    }
}

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
            if words.iter().any(|w| matches!(w.as_str(), "insert" | "update" | "delete" | "into")) {
                StmtKind::Write
            } else {
                StmtKind::Read
            }
        }
        Some("insert" | "update" | "delete" | "replace") => StmtKind::Write,
        _ => StmtKind::Other,
    }
}

/// Lower-case words outside string literals and comments.
fn words(stmt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = stmt.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            let mut prev = ' ';
            for n in chars.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
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

/// The statements of a script as the mysql CLI reads them; optimizer hints
/// (`/*+ … */`) and MySQL's versioned comments (`/*! … */`) stay: they
/// change the plan.
pub(crate) fn split_keeping_hints(variant: Variant, sql: &str) -> Vec<String> {
    let d = crate::script_dialect(variant);
    dbine_driver::sql::split_script(sql, &d)
        .into_iter()
        .filter(|u| u.kind != dbine_driver::StatementKind::ClientCommand)
        .map(|u| dbine_driver::sql::strip_comments(&u.text, &d, true).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

/// A warning when estimated and actual rows are 10x or more apart.
fn estimate_warning(est: f64, act: f64) -> Option<String> {
    let (lo, hi) = if est < act { (est, act) } else { (act, est) };
    (hi - lo >= 100.0 && hi >= 10.0 * lo.max(1.0))
        .then(|| format!("Estimación de filas errada: {} estimadas, {} reales", fmt_num(est), fmt_num(act)))
}

fn full_scan_warning(rows: f64) -> Option<String> {
    (rows >= BIG_SCAN).then(|| format!("Recorrido completo de tabla grande (~{} filas)", fmt_num(rows)))
}

/// Nodes with their depth, in output order, into a tree: each node hangs
/// from the closest shallower one before it.
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

fn plan(statement: &str, root: PlanNode, actual: bool, raw_format: &str, raw: &str) -> Plan {
    Plan { statement: statement.into(), root, actual, raw_format: raw_format.into(), raw: raw.trim_end().into() }
}

// ---- MySQL 8 tree -------------------------------------------------------

/// `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE`: one `-> operator` per line,
/// four spaces deeper per level.
pub(crate) fn mysql_tree(statement: &str, raw: &str, actual: bool) -> Plan {
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        match t.strip_prefix("->") {
            Some(rest) => items.push((indent, tree_node(rest.trim()))),
            None => {
                if let Some((_, n)) = items.last_mut() {
                    n.props.push((String::new(), t.to_string()));
                }
            }
        }
    }
    let root = build_tree(items).unwrap_or_else(|| PlanNode { op: raw.trim().into(), ..Default::default() });
    plan(statement, root, actual, "text", raw)
}

/// `Filter: (a.s like 'a%')  (cost=0.25 rows=0.111) (actual time=0.004..0.004 rows=0.09 loops=500)`.
fn tree_node(text: &str) -> PlanNode {
    let (desc, figures) = match text.find("  (") {
        Some(i) => (&text[..i], &text[i..]),
        None => (text, ""),
    };
    let mut n = PlanNode::default();
    n.props.push(("Operación".into(), desc.to_string()));
    let colon = desc.find(": ").filter(|&i| !desc[..i].contains(" on "));
    if let Some(i) = colon {
        n.op = desc[..i].to_string();
        n.detail = desc[i + 2..].to_string();
    } else if let Some((op, rest)) = desc.split_once(" on ") {
        n.op = op.to_string();
        let (object, more) = rest.split_once(' ').unwrap_or((rest, ""));
        n.object = Some(object.to_string());
        n.detail = more.strip_prefix("using ").unwrap_or(more).to_string();
    } else {
        n.op = desc.to_string();
        // `Inner hash join (a.id = b.a_id)`: the condition is the detail.
        if n.op.contains("join") && n.op.ends_with(')') {
            if let Some(i) = n.op.find(" (") {
                n.detail = n.op[i + 2..n.op.len() - 1].to_string();
                n.op.truncate(i);
            }
        }
    }

    let field = |group: &str, key: &str| -> Option<String> {
        let i = group.find(key)?;
        Some(group[i + key.len()..].split([' ', ')']).next()?.to_string())
    };
    let (est, act) = match figures.find("(actual") {
        Some(i) => (&figures[..i], &figures[i..]),
        None => (figures, ""),
    };
    if let Some(cost) = field(est, "cost=") {
        n.total_cost = cost.rsplit("..").next().and_then(|c| c.parse().ok());
        n.props.push(("cost".into(), cost));
    }
    n.est_rows = field(est, "rows=").and_then(|r| r.parse().ok());
    let mut per_loop = None;
    if figures.contains("never executed") {
        n.executions = Some(0.0);
        n.warnings.push("Nunca se ejecutó".into());
    } else if !act.is_empty() {
        let loops: f64 = field(act, "loops=").and_then(|l| l.parse().ok()).unwrap_or(1.0);
        per_loop = field(act, "rows=").and_then(|r| r.parse::<f64>().ok());
        n.executions = Some(loops);
        n.actual_rows = per_loop.map(|r| r * loops);
        if let Some(time) = field(act, "time=") {
            n.actual_ms = time.rsplit("..").next().and_then(|t| t.parse::<f64>().ok()).map(|t| t * loops);
            n.props.push(("actual time".into(), time));
        }
    }
    if let (Some(e), Some(a)) = (n.est_rows, per_loop) {
        n.warnings.extend(estimate_warning(e, a));
    }
    if n.op == "Table scan" && !n.object.as_deref().unwrap_or("").starts_with('<') {
        n.warnings.extend(full_scan_warning(n.actual_rows.or(n.est_rows).unwrap_or(0.0)));
    }
    n
}

// ---- MariaDB JSON -------------------------------------------------------

/// Keys whose object value is an operator in MariaDB's JSON plan.
const MARIA_NODES: &[&str] = &[
    "query_block",
    "table",
    "filesort",
    "temporary_table",
    "union_result",
    "materialized",
    "duplicates_removal",
    "read_sorted_file",
    "window_functions_computation",
    "block-nl-join",
    "expression_cache",
    "range-checked-for-each-record",
];

/// `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON`: nested operators
/// (`query_block` → `filesort` → `nested_loop` of `table`s…).
pub(crate) fn maria_json(statement: &str, raw: &str, actual: bool) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("plan JSON ilegible: {e}"))?;
    let Value::Object(top) = &v else { return Err("plan JSON inesperado".into()) };
    let block = top.get("query_block").ok_or("el plan JSON no tiene query_block")?;
    let mut root = maria_node("query_block", block);
    for (k, v) in top {
        if k != "query_block" {
            root.props.push((k.clone(), if v.is_object() { v.to_string() } else { text(v) }));
        }
    }
    Ok(plan(statement, root, actual, "json", raw))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        Value::Object(_) => v.to_string(),
        other => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

/// Children of an element of an array of operators (`{"table": {…}}`).
fn maria_children_of(elem: &Value, out: &mut Vec<PlanNode>) {
    if let Value::Object(m) = elem {
        for (k, v) in m {
            if v.is_object() {
                out.push(maria_node(k, v));
            }
        }
    }
}

fn maria_node(key: &str, v: &Value) -> PlanNode {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let mut n = PlanNode { op: key.to_string(), ..Default::default() };
    match key {
        "table" => {
            n.op = access_op(s("access_type")).to_string();
            n.object = (!s("table_name").is_empty()).then(|| s("table_name").to_string());
            n.detail = s("key").to_string();
            if v.get("delete").is_some() {
                n.op = format!("Delete · {}", n.op);
            } else if v.get("update").is_some() {
                n.op = format!("Update · {}", n.op);
            }
        }
        "query_block" => {
            n.detail = [
                v.get("select_id").map(|i| format!("select #{i}")).unwrap_or_default(),
                s("operation").to_string(),
            ]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        }
        "filesort" => n.detail = s("sort_key").to_string(),
        "union_result" => n.object = (!s("table_name").is_empty()).then(|| s("table_name").to_string()),
        _ => {}
    }
    n.total_cost = num(v.get("cost"));
    n.est_rows = num(v.get("rows"));
    let loops = num(v.get("r_loops"));
    let per_loop = num(v.get("r_rows")).or_else(|| num(v.get("r_output_rows")));
    n.executions = loops;
    n.actual_rows = per_loop.map(|r| if v.get("r_rows").is_some() { r * loops.unwrap_or(1.0) } else { r });
    n.actual_ms = num(v.get("r_total_time_ms")).or_else(|| {
        let t = num(v.get("r_table_time_ms"));
        let o = num(v.get("r_other_time_ms"));
        (t.is_some() || o.is_some()).then(|| t.unwrap_or(0.0) + o.unwrap_or(0.0))
    });
    if let (Some(e), Some(a)) = (n.est_rows, num(v.get("r_rows"))) {
        n.warnings.extend(estimate_warning(e, a));
    }
    if key == "table" && s("access_type") == "ALL" {
        let read = num(v.get("r_rows")).or(n.est_rows).unwrap_or(0.0) * loops.or(num(v.get("loops"))).unwrap_or(1.0);
        n.warnings.extend(full_scan_warning(read));
    }

    let Value::Object(m) = v else { return n };
    for (k, child) in m {
        match child {
            Value::Array(items) if k == "nested_loop" => {
                let mut tables = Vec::new();
                for item in items {
                    maria_children_of(item, &mut tables);
                }
                // A left-deep join: ((t1 ⋈ t2) ⋈ t3).
                let mut it = tables.into_iter();
                if let Some(first) = it.next() {
                    let joined = it.fold(first, |acc, t| PlanNode {
                        op: "nested loop".into(),
                        children: vec![acc, t],
                        ..Default::default()
                    });
                    n.children.push(joined);
                }
            }
            Value::Array(items) if items.iter().any(Value::is_object) => {
                for item in items {
                    maria_children_of(item, &mut n.children);
                }
            }
            Value::Object(_) if MARIA_NODES.contains(&k.as_str()) => n.children.push(maria_node(k, child)),
            _ => n.props.push((k.clone(), text(child))),
        }
    }
    n
}

/// MySQL's `type` / `access_type` in words.
fn access_op(t: &str) -> &str {
    match t {
        "ALL" => "Table scan",
        "index" => "Full index scan",
        "range" => "Index range scan",
        "ref" => "Index lookup",
        "eq_ref" => "Unique index lookup",
        "ref_or_null" => "Index lookup or null",
        "const" | "system" => "Constant row",
        "index_merge" => "Index merge",
        "fulltext" => "Fulltext index",
        "unique_subquery" | "index_subquery" => "Subquery index lookup",
        "" => "table",
        other => other,
    }
}

// ---- Tabular EXPLAIN (MySQL 5.7, TiDB) ----------------------------------

fn col<'a>(header: &[String], row: &'a [String], names: &[&str]) -> Option<&'a str> {
    let i = header.iter().position(|h| names.iter().any(|n| h.eq_ignore_ascii_case(n)))?;
    row.get(i).map(String::as_str).filter(|v| !v.is_empty() && *v != "NULL")
}

/// The rows as tab-separated text, header first, for `Plan::raw`.
pub(crate) fn tsv(header: &[String], rows: &[Vec<String>]) -> String {
    std::iter::once(header.join("\t")).chain(rows.iter().map(|r| r.join("\t"))).collect::<Vec<_>>().join("\n")
}

/// Classic `EXPLAIN`: one row per table access, each a node under a
/// synthetic root (the rows don't say how they nest).
pub(crate) fn tabular(statement: &str, header: &[String], rows: &[Vec<String>]) -> Plan {
    let children = rows
        .iter()
        .map(|r| {
            let ty = col(header, r, &["type", "access_type"]).unwrap_or("");
            let est_rows = col(header, r, &["rows"]).and_then(|v| v.parse::<f64>().ok());
            let mut n = PlanNode {
                op: access_op(ty).to_string(),
                detail: col(header, r, &["key"]).unwrap_or("").to_string(),
                object: col(header, r, &["table"]).map(str::to_string),
                est_rows,
                props: header.iter().cloned().zip(r.iter().cloned()).filter(|(_, v)| v != "NULL").collect(),
                ..Default::default()
            };
            if ty == "ALL" {
                n.warnings.extend(full_scan_warning(est_rows.unwrap_or(0.0)));
            }
            n
        })
        .collect();
    let root = PlanNode { op: "Query".into(), children, ..Default::default() };
    plan(statement, root, false, "text", &tsv(header, rows))
}

/// TiDB: the `id` column draws the tree (`└─`, `├─`, `│`); `HashJoin_27`
/// loses its numeric suffix and `(Build)` / `(Probe)` becomes the detail.
pub(crate) fn tidb(statement: &str, header: &[String], rows: &[Vec<String>], actual: bool) -> Plan {
    let mut items = Vec::new();
    for r in rows {
        let Some(id) = col(header, r, &["id"]) else { continue };
        let depth = id.chars().take_while(|c| !c.is_alphanumeric()).count();
        let name: String = id.chars().skip(depth).collect();
        let (name, tag) = match name.split_once('(') {
            Some((n, t)) => (n.to_string(), t.trim_end_matches(')').to_string()),
            None => (name, String::new()),
        };
        let op = match name.rsplit_once('_') {
            Some((o, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => o.to_string(),
            _ => name.clone(),
        };
        let info = col(header, r, &["operator info"]).unwrap_or("");
        let mut detail = vec![tag];
        if !info.is_empty() && info.chars().count() <= 60 {
            detail.push(info.to_string());
        }
        let object = col(header, r, &["access object"]).map(|o| {
            o.split(", ").map(|p| p.split_once(':').map_or(p, |(_, v)| v)).collect::<Vec<_>>().join(", ")
        });
        let est_rows = col(header, r, &["estRows", "count"]).and_then(|v| v.parse::<f64>().ok());
        let actual_rows = col(header, r, &["actRows"]).and_then(|v| v.parse::<f64>().ok());
        let exec = col(header, r, &["execution info"]).unwrap_or("");
        let actual_ms = exec
            .split(", ")
            .find_map(|p| p.strip_prefix("time:"))
            .and_then(duration_ms);
        let executions = exec.split(", ").find_map(|p| p.strip_prefix("loops:")).and_then(|l| l.parse().ok());
        let mut n = PlanNode {
            op,
            detail: detail.into_iter().filter(|d| !d.is_empty()).collect::<Vec<_>>().join(" · "),
            object,
            est_rows,
            actual_rows,
            executions,
            actual_ms,
            props: header
                .iter()
                .cloned()
                .zip(r.iter().cloned())
                .filter(|(h, v)| h != "id" && !v.is_empty() && v != "N/A")
                .collect(),
            ..Default::default()
        };
        if let (Some(e), Some(a)) = (est_rows, actual_rows) {
            n.warnings.extend(estimate_warning(e, a));
        }
        if n.op.starts_with("TableFullScan") {
            n.warnings.extend(full_scan_warning(actual_rows.or(est_rows).unwrap_or(0.0)));
        }
        if info.contains("stats:pseudo") {
            n.warnings.push("Sin estadísticas de la tabla (pseudo)".into());
        }
        items.push((depth, n));
    }
    let root = build_tree(items).unwrap_or_else(|| PlanNode { op: "Query".into(), ..Default::default() });
    plan(statement, root, actual, "text", &tsv(header, rows))
}

/// `743.9µs`, `1.2ms`, `2s` → milliseconds.
fn duration_ms(v: &str) -> Option<f64> {
    let v = v.trim();
    let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let n: f64 = v[..split].parse().ok()?;
    Some(
        n * match &v[split..] {
            "ns" => 1e-6,
            "µs" | "us" => 1e-3,
            "ms" => 1.0,
            "s" => 1000.0,
            "m" | "min" => 60_000.0,
            _ => return None,
        },
    )
}

/// A one-column text plan (StarRocks, Doris, OceanBase…): each line a
/// node, nested by indentation, under a synthetic root.
pub(crate) fn text_lines(statement: &str, raw: &str) -> Plan {
    let mut items = vec![(0, PlanNode { op: "EXPLAIN".into(), ..Default::default() })];
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|c| c.is_whitespace() || matches!(c, '|' | '│' | '└' | '├' | '─' | '-' | '>')).count();
        items.push((indent + 1, PlanNode { op: t.trim_start_matches(['|', '│', '└', '├', '─', '-', '>', ' ']).to_string(), ..Default::default() }));
    }
    let root = build_tree(items).unwrap_or_default();
    plan(statement, root, false, "text", raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE_ANALYZE: &str = "\
-> Sort: `count(*)` DESC  (actual time=2.3..2.3 rows=5 loops=1)
    -> Table scan on <temporary>  (actual time=2.28..2.28 rows=5 loops=1)
        -> Aggregate using temporary table  (actual time=2.28..2.28 rows=5 loops=1)
            -> Nested loop inner join  (cost=225 rows=55.6) (actual time=1.96..2.23 rows=45 loops=1)
                -> Filter: (b.a_id is not null)  (cost=50.2 rows=500) (actual time=0.0326..0.0682 rows=500 loops=1)
                    -> Table scan on b  (cost=50.2 rows=500000) (actual time=0.0323..0.053 rows=500 loops=1)
                -> Filter: (a.s like 'a%')  (cost=0.25 rows=0.111) (actual time=0.00425..0.00426 rows=0.09 loops=500)
                    -> Single-row index lookup on a using PRIMARY (id=b.a_id)  (cost=0.25 rows=1) (actual time=358e-6..370e-6 rows=1 loops=500)
";

    #[test]
    fn mysql_tree_with_actuals() {
        let p = mysql_tree("q", TREE_ANALYZE, true);
        let root = &p.root;
        assert_eq!((root.op.as_str(), root.detail.as_str()), ("Sort", "`count(*)` DESC"));
        assert_eq!(root.actual_rows, Some(5.0));
        let join = &root.children[0].children[0].children[0];
        assert_eq!(join.op, "Nested loop inner join");
        assert_eq!(join.total_cost, Some(225.0));
        assert_eq!(join.est_rows, Some(55.6));
        let [left, right] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!(left.op, "Filter");
        let scan = &left.children[0];
        assert_eq!((scan.op.as_str(), scan.object.as_deref()), ("Table scan", Some("b")));
        assert!(scan.warnings.iter().any(|w| w.starts_with("Estimación")));
        let lookup = &right.children[0];
        assert_eq!(lookup.op, "Single-row index lookup");
        assert_eq!(lookup.object.as_deref(), Some("a"));
        assert_eq!(lookup.detail, "PRIMARY (id=b.a_id)");
        assert_eq!(lookup.executions, Some(500.0));
        assert_eq!(lookup.actual_rows, Some(500.0));
        assert!((lookup.actual_ms.unwrap() - 0.185).abs() < 1e-9);
        assert!((right.actual_rows.unwrap() - 45.0).abs() < 1e-9);
    }

    #[test]
    fn mysql_tree_estimated_and_never_executed() {
        let raw = "-> Insert into b\n    -> Filter: (a.id < 5)  (cost=1.26 rows=5)\n        -> Index range scan on a using PRIMARY over (id < 5)  (cost=0..1.26 rows=5) (never executed)";
        let p = mysql_tree("q", raw, false);
        assert_eq!(p.root.op, "Insert into b");
        let join = tree_node("Inner hash join (a.id = b.a_id)  (cost=1011 rows=100)");
        assert_eq!((join.op.as_str(), join.detail.as_str()), ("Inner hash join", "a.id = b.a_id"));
        let range = &p.root.children[0].children[0];
        assert_eq!(range.total_cost, Some(1.26));
        assert_eq!(range.detail, "PRIMARY over (id < 5)");
        assert_eq!(range.executions, Some(0.0));
    }

    const MARIA_ANALYZE: &str = r#"{
  "query_optimization": {"r_total_time_ms": 0.017768417},
  "query_block": {
    "select_id": 1, "cost": 0.5436708, "r_loops": 1, "r_total_time_ms": 0.595825907,
    "filesort": {
      "sort_key": "count(0) desc", "r_loops": 1, "r_total_time_ms": 0.280373941, "r_output_rows": 5,
      "temporary_table": {
        "nested_loop": [
          {"table": {"table_name": "b", "access_type": "ALL", "loops": 1, "r_loops": 1, "rows": 500000,
                     "r_rows": 500, "cost": 0.0923548, "r_table_time_ms": 0.026, "r_other_time_ms": 0.012,
                     "r_engine_stats": {"pages_accessed": 1}, "filtered": 100, "attached_condition": "b.a_id is not null"}},
          {"table": {"table_name": "a", "access_type": "eq_ref", "key": "PRIMARY", "ref": ["t.b.a_id"],
                     "loops": 500, "r_loops": 500, "rows": 1, "r_rows": 1, "cost": 0.451316}},
          {"block-nl-join": {"table": {"table_name": "c", "access_type": "ALL", "rows": 10},
                             "buffer_type": "flat", "join_type": "BNL"}}
        ]
      }
    },
    "subqueries": [
      {"query_block": {"select_id": 2, "nested_loop": [{"table": {"table_name": "d", "access_type": "const", "rows": 1}}]}}
    ]
  }
}"#;

    #[test]
    fn mariadb_json_tree() {
        let p = maria_json("q", MARIA_ANALYZE, true).unwrap();
        let root = &p.root;
        assert_eq!((root.op.as_str(), root.detail.as_str()), ("query_block", "select #1"));
        assert!(root.props.iter().any(|(k, _)| k == "query_optimization"));
        assert_eq!(root.children.len(), 2, "{root:#?}");
        let sort = &root.children[0];
        assert_eq!((sort.op.as_str(), sort.detail.as_str()), ("filesort", "count(0) desc"));
        assert_eq!(sort.actual_rows, Some(5.0));
        let tmp = &sort.children[0];
        assert_eq!(tmp.op, "temporary_table");
        // ((b ⋈ a) ⋈ c)
        let outer = &tmp.children[0];
        assert_eq!(outer.op, "nested loop");
        let inner = &outer.children[0];
        let b = &inner.children[0];
        assert_eq!((b.op.as_str(), b.object.as_deref()), ("Table scan", Some("b")));
        assert!(b.warnings.iter().any(|w| w.starts_with("Estimación")));
        assert!(b.warnings.iter().any(|w| w.starts_with("Recorrido completo")) == false);
        assert!((b.actual_ms.unwrap() - 0.038).abs() < 1e-9);
        assert!(b.props.iter().any(|(k, v)| k == "r_engine_stats" && v.contains("pages_accessed")));
        let a = &inner.children[1];
        assert_eq!((a.op.as_str(), a.detail.as_str()), ("Unique index lookup", "PRIMARY"));
        assert_eq!(a.actual_rows, Some(500.0));
        let bnl = &outer.children[1];
        assert_eq!(bnl.op, "block-nl-join");
        assert_eq!(bnl.children[0].object.as_deref(), Some("c"));
        let sub = &root.children[1];
        assert_eq!(sub.detail, "select #2");
        assert_eq!(sub.children[0].op, "Constant row");
    }

    #[test]
    fn mariadb_delete() {
        let raw = r#"{"query_block": {"select_id": 1, "table": {"delete": 1, "table_name": "b", "access_type": "range", "key": "PRIMARY", "rows": 10}}}"#;
        let p = maria_json("q", raw, false).unwrap();
        assert_eq!(p.root.children[0].op, "Delete · Index range scan");
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tidb_rows() {
        let header = strings(&["id", "estRows", "actRows", "task", "access object", "execution info", "operator info", "memory", "disk"]);
        let rows = vec![
            strings(&["Sort_10", "1.00", "5", "root", "", "time:743.9µs, loops:2, RU:2.02", "Column#6:desc", "2.27 KB", "0 Bytes"]),
            strings(&["└─HashJoin_27", "310.00", "45", "root", "", "time:287.9µs, loops:2", "inner join, equal:[eq(t.a.id, t.b.a_id)]", "33.9 KB", "0 Bytes"]),
            strings(&["  ├─TableReader_33(Build)", "62.00", "62", "root", "", "time:118µs, loops:2", "data:Selection_32", "3.65 KB", "N/A"]),
            strings(&["  │ └─TableFullScan_31", "1000.00", "1000", "cop[tikv]", "table:a", "tikv_task:{time:266µs, loops:0}", "keep order:false, stats:pseudo", "N/A", "N/A"]),
            strings(&["  └─TableReader_30(Probe)", "500.00", "500", "root", "", "time:126.3µs, loops:2", "data:Selection_29", "4.51 KB", "N/A"]),
        ];
        let p = tidb("q", &header, &rows, true);
        assert_eq!(p.root.op, "Sort");
        assert!((p.root.actual_ms.unwrap() - 0.7439).abs() < 1e-9);
        let join = &p.root.children[0];
        assert_eq!(join.op, "HashJoin");
        assert!(join.detail.starts_with("inner join"));
        assert_eq!(join.children.len(), 2);
        let build = &join.children[0];
        assert_eq!((build.op.as_str(), build.detail.as_str()), ("TableReader", "Build · data:Selection_32"));
        let scan = &build.children[0];
        assert_eq!(scan.object.as_deref(), Some("a"));
        assert_eq!(scan.actual_rows, Some(1000.0));
        assert!(scan.warnings.iter().any(|w| w.contains("pseudo")));
        assert!(!scan.props.iter().any(|(k, _)| k == "memory"));
        assert_eq!(join.children[1].detail, "Probe · data:Selection_29");
    }

    #[test]
    fn mysql57_tabular() {
        let header = strings(&["id", "select_type", "table", "partitions", "type", "possible_keys", "key", "key_len", "ref", "rows", "filtered", "Extra"]);
        let rows = vec![
            strings(&["1", "SIMPLE", "b", "NULL", "ALL", "NULL", "NULL", "NULL", "NULL", "200000", "100.00", "Using where"]),
            strings(&["1", "SIMPLE", "a", "NULL", "eq_ref", "PRIMARY", "PRIMARY", "4", "t.b.a_id", "1", "100.00", "NULL"]),
        ];
        let p = tabular("q", &header, &rows);
        assert_eq!(p.root.op, "Query");
        assert_eq!(p.root.children.len(), 2);
        assert!(p.root.children[0].warnings.iter().any(|w| w.starts_with("Recorrido completo")));
        assert_eq!(p.root.children[1].detail, "PRIMARY");
        assert!(p.raw.starts_with("id\tselect_type"));
    }

    #[test]
    fn text_plans_by_indentation() {
        let raw = "PLAN FRAGMENT 0\n OUTPUT EXPRS:1: id\n  RESULT SINK\n  1:EXCHANGE\nPLAN FRAGMENT 1\n  0:OlapScanNode\n     TABLE: t";
        let p = text_lines("q", raw);
        assert_eq!(p.root.op, "EXPLAIN");
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[1].children[0].children[0].op, "TABLE: t");
    }

    #[test]
    fn flavors_by_version() {
        assert_eq!(Flavor::detect(Variant::MySql, "8.4.11"), Flavor::MySqlTree { analyze: true });
        assert_eq!(Flavor::detect(Variant::MySql, "8.0.17-log"), Flavor::MySqlTree { analyze: false });
        assert_eq!(Flavor::detect(Variant::MySql, "5.7.44"), Flavor::Plain);
        assert_eq!(Flavor::detect(Variant::MySql, "11.8.9-MariaDB-ubu2404"), Flavor::MariaDb);
        assert_eq!(Flavor::detect(Variant::MySql, "8.0.11-TiDB-v7.5.1"), Flavor::TiDb);
        assert_eq!(Flavor::detect(Variant::StarRocks, "8.0.33"), Flavor::Plain);
    }

    #[test]
    fn hints_survive_the_split() {
        let s = split_keeping_hints(Variant::MySql, "SELECT /*+ NO_INDEX(a) */ * FROM a; -- x;\n/* c */ select 2 # y;\n;");
        assert_eq!(s, vec!["SELECT /*+ NO_INDEX(a) */ * FROM a", "select 2"]);
        // DELIMITER, backslash escapes and routine bodies, as the mysql CLI reads them.
        let s = split_keeping_hints(Variant::MySql, "select 'a\\';b';\nDELIMITER //\nselect /*!40001 SQL_NO_CACHE */ 1; select 2//\nDELIMITER ;\nselect 3");
        assert_eq!(s, vec!["select 'a\\';b'", "select /*!40001 SQL_NO_CACHE */ 1; select 2", "select 3"]);
        assert_eq!(classify("SELECT /*+ x */ 1"), StmtKind::Read);
        assert_eq!(classify("replace into t values (1)"), StmtKind::Write);
        assert_eq!(classify("select * from t into outfile '/tmp/x'"), StmtKind::Write);
    }
}

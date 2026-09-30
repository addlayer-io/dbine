//! Oracle plans from `PLAN_TABLE` (after `EXPLAIN PLAN`) or from
//! `V$SQL_PLAN_STATISTICS_ALL` (the cursor that just ran) as
//! [`PlanNode`] trees: rows linked by `ID` / `PARENT_ID`, root `ID = 0`.

use dbine_driver::{Plan, PlanNode};

/// A full table scan reading at least this many rows gets a warning.
const BIG_SCAN: f64 = 100_000.0;

/// Plan columns read from both sources (numbers through `TO_CHAR`).
pub(crate) const PLAN_COLUMNS: &[(&str, bool)] = &[
    ("id", true),
    ("parent_id", true),
    ("operation", false),
    ("options", false),
    ("object_owner", false),
    ("object_name", false),
    ("object_alias", false),
    ("object_type", false),
    ("optimizer", false),
    ("cost", true),
    ("cardinality", true),
    ("bytes", true),
    ("cpu_cost", true),
    ("io_cost", true),
    ("time", true),
    ("partition_start", false),
    ("partition_stop", false),
    ("access_predicates", false),
    ("filter_predicates", false),
    ("projection", false),
    ("qblock_name", false),
];

/// Extra columns of `V$SQL_PLAN_STATISTICS_ALL` (last execution).
pub(crate) const STAT_COLUMNS: &[&str] = &[
    "last_starts",
    "last_output_rows",
    "last_elapsed_time",
    "last_cr_buffer_gets",
    "last_disk_reads",
    "last_memory_used",
    "last_tempseg_size",
];

/// The SELECT list for [`PLAN_COLUMNS`] (plus `extra` numeric columns).
pub(crate) fn select_list(extra: &[&str]) -> String {
    PLAN_COLUMNS
        .iter()
        .map(|(c, numeric)| if *numeric { format!("TO_CHAR({c}) AS {c}") } else { c.to_string() })
        .chain(extra.iter().map(|c| format!("TO_CHAR({c}) AS {c}")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Statements EXPLAIN PLAN takes.
pub(crate) fn explainable(first_word: &str) -> bool {
    matches!(first_word, "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE")
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

/// One plan row: column name (lower case) → value, NULLs left out.
pub(crate) type PlanRow = Vec<(String, String)>;

fn get<'a>(r: &'a PlanRow, k: &str) -> Option<&'a str> {
    r.iter().find(|(c, _)| c == k).map(|(_, v)| v.as_str())
}

fn num(r: &PlanRow, k: &str) -> Option<f64> {
    get(r, k).and_then(|v| v.trim().parse().ok())
}

fn node(r: &PlanRow, actual: bool) -> PlanNode {
    let s = |k: &str| get(r, k).unwrap_or("").to_string();
    let object = get(r, "object_name").map(|name| match get(r, "object_owner") {
        Some(owner) => format!("{owner}.{name}"),
        None => name.to_string(),
    });
    let mut n = PlanNode {
        op: s("operation"),
        detail: s("options"),
        object,
        total_cost: num(r, "cost"),
        est_rows: num(r, "cardinality"),
        ..Default::default()
    };
    if actual {
        n.executions = num(r, "last_starts");
        n.actual_rows = num(r, "last_output_rows");
        // Microseconds, including the children (like A-Time).
        n.actual_ms = num(r, "last_elapsed_time").map(|us| us / 1000.0);
    }
    // E-Rows is per start, A-Rows the total over all starts.
    if let (Some(e), Some(a)) = (n.est_rows, n.actual_rows) {
        let starts = n.executions.unwrap_or(1.0).max(1.0);
        n.warnings.extend(estimate_warning(e * starts, a));
    }
    if n.op == "TABLE ACCESS" && n.detail.starts_with("FULL") {
        let rows = n.actual_rows.or(n.est_rows).unwrap_or(0.0);
        if rows >= BIG_SCAN {
            n.warnings.push(format!("TABLE ACCESS FULL de tabla grande (~{} filas)", fmt_num(rows)));
        }
    }
    if n.op == "MERGE JOIN" && n.detail == "CARTESIAN" {
        n.warnings.push("Producto cartesiano: falta un predicado de join".into());
    }
    if num(r, "last_tempseg_size").is_some_and(|t| t > 0.0) {
        n.warnings.push("Usó espacio temporal en disco".into());
    }
    n.props = r
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "operation" | "options" | "parent_id"))
        .cloned()
        .collect();
    n
}

/// Rows (in `ID` order) into a tree rooted at `ID = 0`.
pub(crate) fn tree(statement: &str, rows: &[PlanRow], actual: bool, raw: &str) -> Plan {
    fn children(parent: &str, rows: &[PlanRow], actual: bool) -> Vec<PlanNode> {
        rows.iter()
            .filter(|r| get(r, "parent_id") == Some(parent))
            .map(|r| {
                let mut n = node(r, actual);
                n.children = children(get(r, "id").unwrap_or(""), rows, actual);
                n
            })
            .collect()
    }
    let root = rows
        .iter()
        .find(|r| get(r, "parent_id").is_none())
        .map(|r| {
            let mut n = node(r, actual);
            n.children = children(get(r, "id").unwrap_or(""), rows, actual);
            n
        })
        .unwrap_or_else(|| PlanNode { op: "Plan".into(), ..Default::default() });
    Plan { statement: statement.into(), root, actual, raw_format: "text".into(), raw: raw.trim_end().into() }
}

/// Whether the cursor's plan carries row-source statistics (they're only
/// collected with `STATISTICS_LEVEL = ALL` or the gather hint).
pub(crate) fn has_stats(rows: &[PlanRow]) -> bool {
    rows.iter().any(|r| get(r, "last_starts").is_some() || get(r, "last_output_rows").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> PlanRow {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn rows() -> Vec<PlanRow> {
        vec![
            row(&[("id", "0"), ("operation", "SELECT STATEMENT"), ("optimizer", "ALL_ROWS"), ("cost", "700"), ("cardinality", "10"),
                  ("last_starts", "1"), ("last_output_rows", "5000"), ("last_elapsed_time", "12000")]),
            row(&[("id", "1"), ("parent_id", "0"), ("operation", "HASH JOIN"), ("cost", "700"), ("cardinality", "10"),
                  ("access_predicates", "\"B\".\"A_ID\"=\"A\".\"ID\""), ("last_starts", "1"), ("last_output_rows", "5000"),
                  ("last_elapsed_time", "11000"), ("last_tempseg_size", "1048576")]),
            row(&[("id", "2"), ("parent_id", "1"), ("operation", "TABLE ACCESS"), ("options", "FULL"),
                  ("object_owner", "DBINE"), ("object_name", "A"), ("cost", "400"), ("cardinality", "200000"),
                  ("last_starts", "1"), ("last_output_rows", "200000")]),
            row(&[("id", "3"), ("parent_id", "1"), ("operation", "INDEX"), ("options", "RANGE SCAN"),
                  ("object_owner", "DBINE"), ("object_name", "B_IX"), ("cost", "3"), ("cardinality", "2"),
                  ("last_starts", "100"), ("last_output_rows", "150")]),
        ]
    }

    #[test]
    fn rows_become_a_tree() {
        let p = tree("q", &rows(), true, "Plan hash value: 1\n");
        let root = &p.root;
        assert_eq!(root.op, "SELECT STATEMENT");
        assert_eq!(root.total_cost, Some(700.0));
        assert_eq!(root.actual_ms, Some(12.0));
        assert!(root.warnings.iter().any(|w| w.starts_with("Estimación")));
        assert!(root.props.iter().any(|(k, v)| k == "optimizer" && v == "ALL_ROWS"));
        let join = &root.children[0];
        assert_eq!(join.op, "HASH JOIN");
        assert!(join.warnings.iter().any(|w| w.contains("temporal")));
        assert!(join.props.iter().any(|(k, _)| k == "access_predicates"));
        let [full, idx] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!((full.op.as_str(), full.detail.as_str(), full.object.as_deref()), ("TABLE ACCESS", "FULL", Some("DBINE.A")));
        assert!(full.warnings.iter().any(|w| w.starts_with("TABLE ACCESS FULL")));
        // 2 per start × 100 starts vs 150: close enough.
        assert_eq!(idx.executions, Some(100.0));
        assert!(idx.warnings.is_empty(), "{:?}", idx.warnings);
        assert!(has_stats(&rows()));
        assert_eq!(p.raw, "Plan hash value: 1");
    }

    #[test]
    fn estimated_rows_ignore_stats() {
        let p = tree("q", &rows(), false, "");
        assert!(!p.actual);
        assert_eq!(p.root.actual_rows, None);
        assert!(select_list(&["last_starts"]).ends_with("TO_CHAR(last_starts) AS last_starts"));
        assert!(explainable("MERGE") && !explainable("CREATE"));
    }
}

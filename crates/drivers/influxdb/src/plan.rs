//! Plans of the three InfluxDB languages:
//! - SQL (v3, DataFusion): `EXPLAIN` / `EXPLAIN ANALYZE` text, one
//!   operator per line (`AggregateExec: mode=…, metrics=[…]`) nested by
//!   indentation;
//! - InfluxQL (1.x): `EXPLAIN` lists `KEY: value` lines per expression;
//!   `EXPLAIN ANALYZE` draws a tree (`└── select`, `├── key: value`);
//! - Flux (2.x): no estimated plan; the `profiler` package adds the query
//!   plan (a DOT digraph) and per-operator durations to the run.

use dbine_driver::PlanNode;
use serde_json::Value as J;

/// Build a tree from `(depth, node)` in document order.
fn nest(items: Vec<(usize, PlanNode)>) -> Option<PlanNode> {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let fold = |stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, depth: usize| {
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            let (_, done) = stack.pop().expect("non-empty");
            match stack.last_mut() {
                Some(top) => top.1.children.push(done),
                None => roots.push(done),
            }
        }
    };
    for (d, n) in items {
        fold(&mut stack, &mut roots, d);
        stack.push((d, n));
    }
    fold(&mut stack, &mut roots, 0);
    match roots.len() {
        0 => None,
        1 => roots.pop(),
        _ => Some(PlanNode { op: "PLAN".into(), children: roots, ..Default::default() }),
    }
}

/// `a=1, b=[x, y], c` split on the commas outside brackets.
fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 && chars.peek() == Some(&' ') => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// `1.24ms`, `13.2µs`, `822ns`, `2.5s`, `1m3s` → milliseconds.
pub fn duration_ms(v: &str) -> Option<f64> {
    let v = v.trim();
    let unit_at = v.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let n: f64 = v[..unit_at].parse().ok()?;
    let (factor, rest) = match &v[unit_at..] {
        u if u.starts_with("ms") => (1.0, &u[2..]),
        u if u.starts_with("µs") => (0.001, &u["µs".len()..]),
        u if u.starts_with("us") => (0.001, &u[2..]),
        u if u.starts_with("ns") => (0.000_001, &u[2..]),
        u if u.starts_with('s') => (1000.0, &u[1..]),
        u if u.starts_with('m') => (60_000.0, &u[1..]),
        u if u.starts_with('h') => (3_600_000.0, &u[1..]),
        _ => return None,
    };
    Some(n * factor + if rest.is_empty() { 0.0 } else { duration_ms(rest)? })
}

// ------------------------------------------------------------------- SQL

/// A DataFusion text plan (logical or physical, with or without metrics).
pub fn datafusion_tree(text: &str) -> Option<PlanNode> {
    let items = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let depth = l.len() - l.trim_start().len();
            (depth, datafusion_node(l.trim()))
        })
        .collect();
    nest(items)
}

fn datafusion_node(line: &str) -> PlanNode {
    let (op, rest) = match line.split_once(": ") {
        Some((op, rest)) if !op.contains(' ') => (op, rest),
        _ => match line.strip_suffix(':') {
            Some(op) => (op, ""),
            None => (line, ""),
        },
    };
    let mut n = PlanNode { op: op.to_string(), ..Default::default() };
    let mut detail = Vec::new();
    for part in split_top(rest) {
        if let Some(m) = part.strip_prefix("metrics=[").and_then(|m| m.strip_suffix(']')) {
            for metric in split_top(m) {
                let Some((k, v)) = metric.split_once('=') else { continue };
                match k {
                    "output_rows" => n.actual_rows = v.parse().ok(),
                    "elapsed_compute" => n.actual_ms = duration_ms(v),
                    "spill_count" if v != "0" => n.warnings.push("Volcó datos a disco (spill)".into()),
                    _ => {}
                }
                n.props.push((k.to_string(), v.to_string()));
            }
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) if !k.contains(' ') && !k.is_empty() => n.props.push((k.to_string(), v.to_string())),
            Some((k, v)) if op == "TableScan" => {
                // `cpu projection=[…]`: the table, then a key.
                let (table, key) = k.split_once(' ').unwrap_or((k, ""));
                n.object = Some(table.to_string());
                n.props.push((key.to_string(), v.to_string()));
            }
            _ if op == "TableScan" && n.object.is_none() => n.object = Some(part.clone()),
            _ => detail.push(part),
        }
    }
    n.detail = detail.join(", ");
    n
}

// -------------------------------------------------------------- InfluxQL

/// InfluxQL `EXPLAIN` rows: `EXPRESSION: …` starts a group, the lines after
/// it (`NUMBER OF SHARDS: 1`…) are its figures.
pub fn influxql_explain_tree(lines: &[String]) -> PlanNode {
    let mut root = PlanNode { op: "SELECT".into(), ..Default::default() };
    for l in lines {
        let Some((k, v)) = l.split_once(": ") else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k == "EXPRESSION" {
            root.children.push(PlanNode { op: "Expresión".into(), detail: v.to_string(), ..Default::default() });
            continue;
        }
        let target = match root.children.last_mut() {
            Some(c) => c,
            None => &mut root,
        };
        let label = match k {
            "NUMBER OF SHARDS" => "Shards",
            "NUMBER OF SERIES" => "Series",
            "CACHED VALUES" => "Valores en caché",
            "NUMBER OF FILES" => "Archivos TSM",
            "NUMBER OF BLOCKS" => "Bloques",
            "SIZE OF BLOCKS" => "Tamaño de los bloques (bytes)",
            other => other,
        };
        target.props.push((label.to_string(), v.to_string()));
    }
    if root.children.len() == 1 {
        let mut only = root.children.pop().expect("one");
        only.op = "SELECT".into();
        return only;
    }
    root
}

/// InfluxQL `EXPLAIN ANALYZE`: a drawn tree whose `key: value` leaves are
/// the figures of the node above (`labels` groups more of them).
pub fn influxql_analyze_tree(lines: &[String]) -> Option<PlanNode> {
    // (depth, node, is_labels)
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    // Index into `items` of the latest node at each depth.
    let mut last_at: Vec<Option<usize>> = Vec::new();
    for l in lines {
        let start = l.find(|c: char| !matches!(c, ' ' | '│' | '├' | '└' | '─')).unwrap_or(l.len());
        let body = l[start..].trim();
        if body.is_empty() || body == "." {
            continue;
        }
        let depth = l[..start].chars().count() / 4;
        if let Some((k, v)) = body.split_once(": ") {
            // A figure of the closest node above; `labels` hands it to its parent.
            let mut d = depth;
            while d > 0 {
                d -= 1;
                if let Some(Some(i)) = last_at.get(d) {
                    if items[*i].1.op == "labels" && d > 0 {
                        continue;
                    }
                    let n = &mut items[*i].1;
                    match k {
                        "total_time" => n.actual_ms = duration_ms(v),
                        "execution_time" if n.actual_ms.is_none() => n.actual_ms = duration_ms(v),
                        "measurement" => n.object = Some(v.to_string()),
                        _ => {}
                    }
                    n.props.push((k.to_string(), v.to_string()));
                    break;
                }
            }
            continue;
        }
        last_at.truncate(depth);
        last_at.resize(depth, None);
        last_at.push(Some(items.len()));
        items.push((depth, PlanNode { op: body.to_string(), ..Default::default() }));
    }
    let items = items.into_iter().filter(|(_, n)| n.op != "labels").collect();
    nest(items)
}

// ------------------------------------------------------------------ Flux

/// The `flux/query-plan` digraph: nodes (`"ReadRange2"` with an optional
/// `// details` comment line) and edges (`"a" -> "b"`, data flowing to b).
pub fn flux_plan_tree(dot: &str) -> Option<PlanNode> {
    let mut nodes: Vec<(String, String)> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    for l in dot.lines().map(str::trim) {
        if let Some(c) = l.strip_prefix("//") {
            if let Some(last) = nodes.last_mut() {
                last.1 = c.trim().to_string();
            }
            continue;
        }
        let quoted: Vec<&str> = l.split('"').skip(1).step_by(2).collect();
        if l.contains("->") && quoted.len() >= 2 {
            edges.push((quoted[0].to_string(), quoted[1].to_string()));
            for q in &quoted[..2] {
                if !nodes.iter().any(|(n, _)| n == q) {
                    nodes.push((q.to_string(), String::new()));
                }
            }
        } else if quoted.len() == 1 && !nodes.iter().any(|(n, _)| n == quoted[0]) {
            nodes.push((quoted[0].to_string(), String::new()));
        }
    }
    if nodes.is_empty() {
        return None;
    }
    fn build(label: &str, nodes: &[(String, String)], edges: &[(String, String)], seen: &mut Vec<String>) -> PlanNode {
        seen.push(label.to_string());
        let details = nodes.iter().find(|(n, _)| n == label).map(|(_, d)| d.clone()).unwrap_or_default();
        let op = label.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
        let mut n = PlanNode { op: if op.is_empty() { label.to_string() } else { op }, ..Default::default() };
        n.props.push(("Nodo".into(), label.to_string()));
        for d in split_top(&details) {
            match d.split_once(" = ") {
                Some((k, v)) => n.props.push((k.trim().to_string(), v.trim().to_string())),
                None if !d.is_empty() => n.props.push(("Detalle".into(), d)),
                None => {}
            }
        }
        for (from, _) in edges.iter().filter(|(_, to)| to == label) {
            if !seen.contains(from) {
                n.children.push(build(from, nodes, edges, seen));
            }
        }
        n
    }
    // Roots: nodes whose output goes nowhere.
    let roots: Vec<&String> = nodes.iter().map(|(n, _)| n).filter(|n| !edges.iter().any(|(from, _)| from == *n)).collect();
    let mut seen = Vec::new();
    let mut trees: Vec<PlanNode> = roots.iter().map(|r| build(r, &nodes, &edges, &mut seen)).collect();
    Some(if trees.len() == 1 { trees.pop().expect("one") } else { PlanNode { op: "PLAN".into(), children: trees, ..Default::default() } })
}

/// A profiler row as `(column, value)` pairs.
pub type Row = Vec<(String, J)>;

fn get<'a>(r: &'a Row, k: &str) -> Option<&'a J> {
    r.iter().find(|(c, _)| c == k).map(|(_, v)| v)
}

fn ns_ms(v: Option<&J>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())).map(|ns| ns / 1_000_000.0)
}

/// The run's plan from the profiler's tables: `profiler/query` (totals and
/// the plan digraph) and `profiler/operator` (one row per operator label).
pub fn flux_profile_tree(query: Option<&Row>, operators: &[Row]) -> PlanNode {
    let dot = query.and_then(|q| get(q, "flux/query-plan")).and_then(J::as_str).unwrap_or_default();
    let mut tree = flux_plan_tree(dot).unwrap_or_else(|| PlanNode {
        op: "Consulta Flux".into(),
        children: operators
            .iter()
            .map(|o| PlanNode {
                op: get(o, "Label").and_then(J::as_str).unwrap_or_default().to_string(),
                props: vec![("Nodo".into(), get(o, "Label").and_then(J::as_str).unwrap_or_default().to_string())],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    fn apply(n: &mut PlanNode, operators: &[Row]) {
        let label = n.props.iter().find(|(k, _)| k == "Nodo").map(|(_, v)| v.clone()).unwrap_or_default();
        if let Some(o) = operators.iter().find(|o| get(o, "Label").and_then(J::as_str) == Some(label.as_str())) {
            n.actual_ms = ns_ms(get(o, "DurationSum"));
            n.executions = get(o, "Count").and_then(J::as_f64);
            if let Some(t) = get(o, "Type").and_then(J::as_str) {
                n.detail = t.trim_start_matches('*').to_string();
            }
            for (k, label) in [("MinDuration", "Duración mínima (ms)"), ("MaxDuration", "Duración máxima (ms)"), ("MeanDuration", "Duración media (ms)")] {
                if let Some(ms) = ns_ms(get(o, k)) {
                    n.props.push((label.into(), format!("{ms:.3}")));
                }
            }
        }
        for c in &mut n.children {
            apply(c, operators);
        }
    }
    apply(&mut tree, operators);
    let Some(q) = query else { return tree };
    let mut root = PlanNode { op: "Consulta Flux".into(), actual_ms: ns_ms(get(q, "TotalDuration")), ..Default::default() };
    for (k, label) in [
        ("CompileDuration", "Compilación (ms)"),
        ("QueueDuration", "En cola (ms)"),
        ("PlanDuration", "Planificación (ms)"),
        ("ExecuteDuration", "Ejecución (ms)"),
    ] {
        if let Some(ms) = ns_ms(get(q, k)) {
            root.props.push((label.into(), format!("{ms:.3}")));
        }
    }
    for (k, label) in [
        ("MaxAllocated", "Memoria máxima (bytes)"),
        ("TotalAllocated", "Memoria total (bytes)"),
        ("influxdb/scanned-values", "Valores leídos"),
        ("influxdb/scanned-bytes", "Bytes leídos"),
        ("RuntimeErrors", "Errores"),
    ] {
        if let Some(v) = get(q, k).filter(|v| !v.is_null() && v.as_str() != Some("")) {
            root.props.push((label.into(), v.as_str().map_or_else(|| v.to_string(), str::to_string)));
        }
    }
    root.children.push(tree);
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations() {
        assert_eq!(duration_ms("1.5ms"), Some(1.5));
        assert_eq!(duration_ms("250µs"), Some(0.25));
        assert_eq!(duration_ms("2s"), Some(2000.0));
        assert_eq!(duration_ms("1m3s"), Some(63_000.0));
        assert!(duration_ms("x").is_none());
    }

    #[test]
    fn datafusion_plans() {
        let t = "\
AggregateExec: mode=FinalPartitioned, gby=[host@0 as host], aggr=[avg(cpu.v)], metrics=[output_rows=2, elapsed_compute=1.243841ms, spill_count=0]
  CoalesceBatchesExec: target_batch_size=8192, metrics=[output_rows=2, elapsed_compute=13.256µs]
    DeduplicateExec: [host@0 ASC,time@2 ASC], metrics=[output_rows=2]
      RecordBatchesExec: chunks=1 [Buffer=1], projection=[host, v]
    FilterExec: host@0 = a
";
        let root = datafusion_tree(t).unwrap();
        assert_eq!(root.op, "AggregateExec");
        assert_eq!(root.actual_rows, Some(2.0));
        assert!((root.actual_ms.unwrap() - 1.243841).abs() < 1e-9);
        assert!(root.props.contains(&("gby".to_string(), "[host@0 as host]".to_string())));
        let coalesce = &root.children[0];
        assert_eq!(coalesce.children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["DeduplicateExec", "FilterExec"]);
        assert_eq!(coalesce.children[0].detail, "[host@0 ASC,time@2 ASC]");
        assert_eq!(coalesce.children[1].detail, "host@0 = a");
        let logical = datafusion_tree("Sort: cpu.host ASC NULLS LAST\n  TableScan: cpu projection=[host, v], partial_filters=[x]").unwrap();
        assert_eq!(logical.children[0].object.as_deref(), Some("cpu"));
        assert!(logical.children[0].props.contains(&("projection".to_string(), "[host, v]".to_string())));
    }

    #[test]
    fn influxql_explain() {
        let lines: Vec<String> = ["EXPRESSION: mean(v::float)", "NUMBER OF SHARDS: 1", "NUMBER OF SERIES: 2"].map(String::from).to_vec();
        let root = influxql_explain_tree(&lines);
        assert_eq!(root.op, "SELECT");
        assert_eq!(root.detail, "mean(v::float)");
        assert_eq!(root.props[1], ("Series".to_string(), "2".to_string()));
        let two: Vec<String> = ["EXPRESSION: a", "NUMBER OF SHARDS: 1", "EXPRESSION: b", "NUMBER OF SHARDS: 2"].map(String::from).to_vec();
        assert_eq!(influxql_explain_tree(&two).children.len(), 2);
    }

    #[test]
    fn influxql_analyze() {
        let lines: Vec<String> = [
            ".",
            "└── select",
            "    ├── execution_time: 33.208µs",
            "    ├── total_time: 603.001µs",
            "    └── build_cursor",
            "        ├── labels",
            "        │   └── statement: SELECT mean(v::float) FROM d.autogen.cpu GROUP BY host",
            "        └── iterator_scanner",
            "            ├── labels",
            "            │   └── expr: mean(v::float)",
            "            └── create_iterator",
            "                ├── labels",
            "                │   ├── measurement: cpu",
            "                │   └── shard_id: 2",
            "                ├── cursors_ref: 2",
            "                └── planning_time: 292.792µs",
            "",
        ]
        .map(String::from)
        .to_vec();
        let root = influxql_analyze_tree(&lines).unwrap();
        assert_eq!(root.op, "select");
        assert!((root.actual_ms.unwrap() - 0.603001).abs() < 1e-9);
        let cursor = &root.children[0];
        assert_eq!(cursor.op, "build_cursor");
        assert_eq!(cursor.props[0].0, "statement");
        let create = &cursor.children[0].children[0];
        assert_eq!(create.op, "create_iterator");
        assert_eq!(create.object.as_deref(), Some("cpu"));
        assert!(create.props.contains(&("cursors_ref".to_string(), "2".to_string())));
    }

    #[test]
    fn flux_profiles() {
        let dot = "digraph {\n  \"ReadRange2\"\n  // start = 0, stop = now\n  \"filter3\"\n  \"mean4\"\n\n  \"ReadRange2\" -> \"filter3\"\n  \"filter3\" -> \"mean4\"\n}\n";
        let q: Row = vec![
            ("TotalDuration".into(), json!(8_592_430)),
            ("ExecuteDuration".into(), json!(7_365_886)),
            ("flux/query-plan".into(), json!(dot)),
        ];
        let ops: Vec<Row> = vec![vec![
            ("Type".into(), json!("*universe.filterTransformation")),
            ("Label".into(), json!("filter3")),
            ("Count".into(), json!(1)),
            ("DurationSum".into(), json!(1_434_294)),
        ]];
        let root = flux_profile_tree(Some(&q), &ops);
        assert_eq!(root.op, "Consulta Flux");
        assert!((root.actual_ms.unwrap() - 8.59243).abs() < 1e-9);
        let mean = &root.children[0];
        assert_eq!(mean.op, "mean");
        let filter = &mean.children[0];
        assert_eq!(filter.op, "filter");
        assert_eq!(filter.detail, "universe.filterTransformation");
        assert!((filter.actual_ms.unwrap() - 1.434294).abs() < 1e-9);
        let read = &filter.children[0];
        assert_eq!(read.op, "ReadRange");
        assert!(read.props.contains(&("start".to_string(), "0".to_string())));
    }
}

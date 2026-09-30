//! Execution plans of the engines behind ODBC, one reader per plan format.
//! What each engine answers (and how it's asked) is in `explain.rs`.

use dbine_driver::plan::tree_from_indented_text;
use dbine_driver::PlanNode;
use std::collections::HashMap;

/// Build a tree from `(depth, node)` in document order.
pub fn nest(items: Vec<(usize, PlanNode)>) -> Option<PlanNode> {
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

/// A tree as indented text (the raw form of plans read from tables).
pub fn render(root: &PlanNode) -> String {
    fn walk(n: &PlanNode, depth: usize, out: &mut String) {
        out.push_str(&"  ".repeat(depth));
        out.push_str(&n.op);
        if !n.detail.is_empty() {
            out.push_str(&format!(" ({})", n.detail));
        }
        if let Some(o) = &n.object {
            out.push_str(&format!(" [{o}]"));
        }
        if let Some(c) = n.total_cost {
            out.push_str(&format!(" cost={c}"));
        }
        if let Some(r) = n.est_rows {
            out.push_str(&format!(" rows={r}"));
        }
        if let Some(r) = n.actual_rows {
            out.push_str(&format!(" actual_rows={r}"));
        }
        if let Some(ms) = n.actual_ms {
            out.push_str(&format!(" ms={ms}"));
        }
        out.push('\n');
        for c in &n.children {
            walk(c, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(root, 0, &mut out);
    out
}

/// `1.23K`, `2M`, `1,234`, `5.0E+3` → number.
pub fn number(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches(['.', ',']).replace(',', "");
    let (n, mult) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1e3),
        'M' => (&s[..s.len() - 1], 1e6),
        'B' | 'G' => (&s[..s.len() - 1], 1e9),
        'T' => (&s[..s.len() - 1], 1e12),
        _ => (s.as_str(), 1.0),
    };
    n.trim().parse::<f64>().ok().map(|v| v * mult)
}

/// The value after `key` up to a space or one of `,)]`: `rows=12` → `12`.
fn after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let i = text.find(key)? + key.len();
    let rest = &text[i..];
    let end = rest.find(|c: char| c.is_whitespace() || matches!(c, ',' | ')' | ']')).unwrap_or(rest.len());
    Some(&rest[..end])
}

// ------------------------------------------------------------ SQL Server

/// Rows of `SET SHOWPLAN_ALL` / `SET STATISTICS PROFILE` (column name →
/// text), grouped into one tree per statement.
pub fn sqlserver_trees(rows: &[HashMap<String, String>]) -> Vec<(String, PlanNode)> {
    let get = |r: &HashMap<String, String>, k: &str| r.get(k).cloned().unwrap_or_default();
    let f = |r: &HashMap<String, String>, k: &str| r.get(k).and_then(|v| v.trim().parse::<f64>().ok());
    let mut order: Vec<String> = Vec::new();
    for r in rows {
        let id = get(r, "StmtId");
        if !order.contains(&id) {
            order.push(id);
        }
    }
    let mut out = Vec::new();
    for id in order {
        let group: Vec<&HashMap<String, String>> = rows.iter().filter(|r| get(r, "StmtId") == id).collect();
        let stmt_row = group.iter().find(|r| get(r, "PhysicalOp").is_empty()).copied();
        let ops: Vec<&HashMap<String, String>> = group.iter().filter(|r| !get(r, "PhysicalOp").is_empty()).copied().collect();
        let node_of = |r: &HashMap<String, String>| {
            let physical = get(r, "PhysicalOp");
            let logical = get(r, "LogicalOp");
            let argument = get(r, "Argument");
            let mut n = PlanNode {
                detail: if logical != physical { logical } else { String::new() },
                op: physical,
                total_cost: f(r, "TotalSubtreeCost"),
                est_rows: f(r, "EstimateRows"),
                actual_rows: f(r, "Rows"),
                executions: f(r, "Executes"),
                ..Default::default()
            };
            if let Some(o) = argument.find("OBJECT:(").map(|i| &argument[i + 8..]) {
                let obj = o.split(')').next().unwrap_or_default();
                n.object = Some(obj.split(", ").next().unwrap_or(obj).to_string());
            }
            for (k, label) in [
                ("Argument", "Argumento"),
                ("DefinedValues", "Valores definidos"),
                ("OutputList", "Lista de salida"),
                ("EstimateIO", "E/S estimada"),
                ("EstimateCPU", "CPU estimada"),
                ("AvgRowSize", "Tamaño medio de fila"),
                ("EstimateExecutions", "Ejecuciones estimadas"),
                ("Parallel", "Paralelo"),
            ] {
                let v = get(r, k);
                if !v.is_empty() {
                    n.props.push((label.into(), v));
                }
            }
            let w = get(r, "Warnings");
            if !w.is_empty() {
                n.warnings.push(if w.contains("NO STATS") {
                    format!("Faltan estadísticas: {w}")
                } else if w.contains("NO JOIN PREDICATE") {
                    "Join sin predicado".into()
                } else {
                    w
                });
            }
            if n.op.contains("Table Scan") || (n.op == "Clustered Index Scan" && !argument.contains("WHERE:")) {
                n.warnings.push("Recorre la tabla completa".into());
            }
            n
        };
        let ids: Vec<String> = ops.iter().map(|r| get(r, "NodeId")).collect();
        fn build(
            r: &HashMap<String, String>,
            ops: &[&HashMap<String, String>],
            node_of: &dyn Fn(&HashMap<String, String>) -> PlanNode,
            depth: usize,
        ) -> PlanNode {
            let mut n = node_of(r);
            let id = r.get("NodeId").cloned().unwrap_or_default();
            if depth < 200 {
                n.children = ops
                    .iter()
                    .filter(|c| c.get("Parent") == Some(&id) && c.get("NodeId") != Some(&id))
                    .map(|c| build(c, ops, node_of, depth + 1))
                    .collect();
            }
            n
        }
        let tops: Vec<PlanNode> = ops
            .iter()
            .filter(|r| !ids.contains(&get(r, "Parent")) || get(r, "Parent") == get(r, "NodeId"))
            .map(|r| build(r, &ops, &node_of, 0))
            .collect();
        let text = stmt_row.map(|r| get(r, "StmtText")).unwrap_or_default().trim().to_string();
        let mut root = PlanNode {
            op: stmt_row.map(|r| get(r, "Type")).filter(|t| !t.is_empty()).unwrap_or_else(|| "STATEMENT".into()),
            total_cost: stmt_row.and_then(|r| f(r, "TotalSubtreeCost")),
            est_rows: stmt_row.and_then(|r| f(r, "EstimateRows")),
            actual_rows: stmt_row.and_then(|r| f(r, "Rows")),
            self_cost: Some(0.0),
            children: tops,
            ..Default::default()
        };
        if root.total_cost.is_none() {
            root.total_cost = root.children.iter().filter_map(|c| c.total_cost).reduce(|a, b| a + b);
        }
        out.push((text, root));
    }
    out
}

// ----------------------------------------------------------------- Db2

/// `EXPLAIN_OPERATOR` row.
#[derive(Debug, Clone, Default)]
pub struct Db2Op {
    pub id: String,
    pub kind: String,
    pub total_cost: Option<f64>,
    pub io_cost: Option<f64>,
    pub cpu_cost: Option<f64>,
    pub first_row_cost: Option<f64>,
}

/// `EXPLAIN_STREAM` row.
#[derive(Debug, Clone, Default)]
pub struct Db2Stream {
    pub source_type: String,
    pub source_id: String,
    pub target_type: String,
    pub target_id: String,
    pub object: String,
    pub count: Option<f64>,
}

/// Db2 LUW explain tables → tree: operators joined by their streams (a
/// stream from an operator into another makes the first a child; a stream
/// from a data object names the table or index the operator reads).
pub fn db2_tree(ops: &[Db2Op], streams: &[Db2Stream]) -> PlanNode {
    let is_op = |t: &str| t.trim() == "O";
    fn build(op: &Db2Op, ops: &[Db2Op], streams: &[Db2Stream], depth: usize) -> PlanNode {
        let is_op = |t: &str| t.trim() == "O";
        let output = streams.iter().find(|s| is_op(&s.source_type) && s.source_id == op.id);
        let inputs: Vec<&Db2Stream> = streams.iter().filter(|s| is_op(&s.target_type) && s.target_id == op.id).collect();
        let mut n = PlanNode {
            op: op.kind.trim().to_string(),
            total_cost: op.total_cost,
            est_rows: output.and_then(|s| s.count),
            ..Default::default()
        };
        n.props.push(("Operador".into(), op.id.clone()));
        for (k, v) in [("Costo de E/S", op.io_cost), ("Costo de CPU", op.cpu_cost), ("Costo hasta la primera fila", op.first_row_cost)] {
            if let Some(v) = v {
                n.props.push((k.into(), format!("{v}")));
            }
        }
        for s in inputs.iter().filter(|s| s.source_type.trim() == "D") {
            if n.object.is_none() {
                n.object = Some(s.object.clone());
            } else {
                n.props.push(("Objeto".into(), s.object.clone()));
            }
            if let Some(c) = s.count {
                n.props.push((format!("Filas de {}", s.object), format!("{c}")));
            }
        }
        if n.op == "TBSCAN" && inputs.iter().any(|s| s.source_type.trim() == "D") {
            n.warnings.push("Recorre la tabla completa".into());
        }
        if depth < 200 {
            n.children = inputs
                .iter()
                .filter(|s| is_op(&s.source_type))
                .filter_map(|s| ops.iter().find(|o| o.id == s.source_id))
                .map(|o| build(o, ops, streams, depth + 1))
                .collect();
        }
        if n.est_rows.is_none() && n.children.len() == 1 {
            n.est_rows = n.children[0].est_rows;
        }
        n
    }
    let roots: Vec<PlanNode> = ops
        .iter()
        .filter(|o| !streams.iter().any(|s| is_op(&s.source_type) && s.source_id == o.id && is_op(&s.target_type)))
        .map(|o| build(o, ops, streams, 0))
        .collect();
    match roots.len() {
        1 => roots.into_iter().next().expect("one"),
        _ => PlanNode { op: "PLAN".into(), children: roots, ..Default::default() },
    }
}

/// Db2 for z/OS `PLAN_TABLE` row.
#[derive(Debug, Clone, Default)]
pub struct ZRow {
    pub qblock: String,
    pub method: String,
    pub table: String,
    pub access_type: String,
    pub match_cols: String,
    pub index: String,
    pub index_only: String,
    pub prefetch: String,
}

/// `PLAN_TABLE` steps → tree: per query block, each step joins the next
/// table to what came before (METHOD 1 nested loop, 2 merge scan, 4
/// hybrid; 3 is an extra sort).
pub fn db2zos_tree(rows: &[ZRow]) -> PlanNode {
    let mut blocks: Vec<(String, PlanNode)> = Vec::new();
    for r in rows {
        let access = |r: &ZRow| {
            let (op, warn) = match r.access_type.trim() {
                "R" => ("Table space scan", Some("Recorre el table space completo")),
                "I" => ("Index scan", None),
                "I1" => ("One-fetch index scan", None),
                "N" => ("Index scan (IN-list)", None),
                "M" | "MX" | "MI" | "MU" => ("Multiple index access", None),
                "H" => ("Hash join access", None),
                "" => ("Access", None),
                other => (other, None),
            };
            let mut n = PlanNode { op: op.to_string(), object: Some(r.table.clone()).filter(|t| !t.is_empty()), ..Default::default() };
            if let Some(w) = warn {
                n.warnings.push(w.into());
            }
            for (k, v) in [("Índice", &r.index), ("Columnas coincidentes", &r.match_cols), ("Solo índice", &r.index_only), ("Prefetch", &r.prefetch)] {
                if !v.trim().is_empty() {
                    n.props.push((k.into(), v.trim().to_string()));
                }
            }
            n
        };
        let step = access(r);
        let prev = match blocks.iter().position(|(b, _)| *b == r.qblock) {
            Some(i) => Some(blocks.remove(i).1),
            None => None,
        };
        let node = match (prev, r.method.trim()) {
            (None, _) => step,
            (Some(p), "3") => PlanNode { op: "Sort".into(), children: vec![p], ..Default::default() },
            (Some(p), m) => PlanNode {
                op: match m {
                    "1" => "Nested loop join",
                    "2" => "Merge scan join",
                    "4" => "Hybrid join",
                    _ => "Join",
                }
                .into(),
                children: vec![p, step],
                ..Default::default()
            },
        };
        blocks.push((r.qblock.clone(), node));
    }
    match blocks.len() {
        1 => blocks.pop().expect("one").1,
        _ => PlanNode {
            op: "QUERY".into(),
            children: blocks
                .into_iter()
                .map(|(b, n)| PlanNode { op: format!("Bloque {b}"), children: vec![n], ..Default::default() })
                .collect(),
            ..Default::default()
        },
    }
}

// --------------------------------------------------------- Sybase ASE

/// `SET SHOWPLAN ON` text → one tree per `QUERY PLAN FOR STATEMENT n`.
/// Operators are the `|NAME Operator` lines, nested by their bars; the
/// lines below an operator with its bars are its details.
pub fn sybase_trees(text: &str) -> Vec<PlanNode> {
    let mut plans: Vec<(PlanNode, Vec<(usize, PlanNode)>)> = Vec::new();
    let mut from_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("QUERY PLAN FOR STATEMENT") || plans.is_empty() {
            let root = PlanNode { op: "STATEMENT".into(), detail: t.trim_end_matches('.').to_string(), ..Default::default() };
            plans.push((root, Vec::new()));
            if t.starts_with("QUERY PLAN FOR STATEMENT") {
                continue;
            }
        }
        let (root, items) = plans.last_mut().expect("a plan");
        if !t.starts_with('|') {
            if let Some(kind) = t.strip_prefix("The type of query is ") {
                root.op = kind.trim_end_matches('.').to_string();
            } else if t.starts_with("Total estimated I/O cost") {
                root.total_cost = t.rsplit(':').next().and_then(number);
            }
            root.props.push(("Detalle".into(), t.to_string()));
            continue;
        }
        let body_at = line.find(|c: char| c != '|' && !c.is_whitespace()).unwrap_or(line.len());
        let depth = line[..body_at].matches('|').count();
        let body = line[body_at..].trim();
        if body.is_empty() {
            continue;
        }
        if let Some(i) = body.find(" Operator") {
            let name = body[..i].trim_start_matches("ROOT:").to_string();
            let rest = body[i + " Operator".len()..].trim();
            let mut n = PlanNode { op: name, ..Default::default() };
            for part in rest.split(')').map(|p| p.trim().trim_start_matches('(').trim()).filter(|p| !p.is_empty()) {
                match part.split_once(" = ").or_else(|| part.split_once(": ")) {
                    Some(("VA", v)) => n.props.push(("VA".into(), v.to_string())),
                    Some((k, v)) => {
                        if n.detail.is_empty() {
                            n.detail = v.to_string();
                        }
                        n.props.push((k.to_string(), v.to_string()));
                    }
                    None => n.props.push(("Detalle".into(), part.to_string())),
                }
            }
            items.push((depth, n));
            from_table = false;
            continue;
        }
        // A detail of the latest operator at this depth.
        let Some((_, n)) = items.iter_mut().rev().find(|(d, _)| *d == depth) else { continue };
        if from_table {
            n.object = Some(body.to_string());
            from_table = false;
        } else if body == "FROM TABLE" || body == "FROM CACHE" {
            from_table = true;
        } else if body.starts_with("Table Scan") {
            n.warnings.push("Recorre la tabla completa".into());
        } else if let Some((k, v)) = body.split_once(" : ") {
            n.props.push((k.trim().to_string(), v.trim().to_string()));
        } else {
            n.props.push(("Detalle".into(), body.to_string()));
        }
    }
    plans
        .into_iter()
        .map(|(mut root, items)| {
            if let Some(tree) = nest(items) {
                root.children.push(tree);
            }
            root
        })
        .collect()
}

// ------------------------------------------------------------- Impala

/// Impala `EXPLAIN`: `NN:OPERATOR [details]` lines; operators in a straight
/// column feed the one above, `|--` starts the second input of a join
/// (its own column), `|  key: value` lines are details.
pub fn impala_tree(text: &str) -> PlanNode {
    struct N {
        node: PlanNode,
        parent: Option<usize>,
    }
    let mut nodes: Vec<N> = Vec::new();
    let mut root = PlanNode { op: "QUERY".into(), ..Default::default() };
    // Latest node at each column.
    let mut at_col: HashMap<usize, usize> = HashMap::new();
    let mut last: Option<usize> = None;
    for line in text.lines() {
        let start = line.find(|c: char| c != '|' && c != ' ' && c != '-').unwrap_or(line.len());
        let body = line[start..].trim();
        if body.is_empty() {
            continue;
        }
        let branch = line[..start].contains("|--");
        let is_op = body == "PLAN-ROOT SINK"
            || body.split_once(':').is_some_and(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()));
        if !is_op {
            match last {
                Some(i) => {
                    let n = &mut nodes[i].node;
                    for part in body.split_whitespace() {
                        if let Some(v) = part.strip_prefix("cardinality=") {
                            n.est_rows = number(v);
                        }
                    }
                    match body.split_once(": ") {
                        Some((k, v)) => n.props.push((k.to_string(), v.to_string())),
                        None => n.props.push(("Detalle".into(), body.to_string())),
                    }
                    if body.contains("stats: unavailable") || body.contains("missing stats") {
                        n.warnings.push("Faltan estadísticas".into());
                    }
                }
                None => {
                    if body.starts_with("WARNING") {
                        root.warnings.push("Faltan estadísticas en algunas tablas".into());
                    }
                    root.props.push(("Detalle".into(), body.to_string()));
                }
            }
            continue;
        }
        let col = start;
        let parent = if branch { at_col.get(&col.saturating_sub(3)).copied() } else { at_col.get(&col).copied() }
            .or(if branch { last } else { None });
        let (id, op) = match body.split_once(':') {
            Some((id, rest)) if body != "PLAN-ROOT SINK" => (Some(id.to_string()), rest.trim().to_string()),
            _ => (None, body.to_string()),
        };
        let (op, detail) = match op.split_once(" [") {
            Some((o, d)) => (o.to_string(), d.trim_end_matches(']').to_string()),
            None => (op, String::new()),
        };
        let mut n = PlanNode { op, ..Default::default() };
        if n.op.starts_with("SCAN") {
            n.object = Some(detail.split(',').next().unwrap_or_default().trim().to_string()).filter(|s| !s.is_empty());
        } else {
            n.detail = detail;
        }
        if let Some(id) = id {
            n.props.push(("Id".into(), id));
        }
        nodes.push(N { node: n, parent });
        let i = nodes.len() - 1;
        at_col.retain(|c, _| *c <= col);
        at_col.insert(col, i);
        last = Some(i);
    }
    // Children in document order.
    fn build(i: usize, nodes: &[N]) -> PlanNode {
        let mut n = nodes[i].node.clone();
        n.children = (0..nodes.len()).filter(|&k| nodes[k].parent == Some(i)).map(|k| build(k, nodes)).collect();
        n
    }
    let tops: Vec<PlanNode> = (0..nodes.len()).filter(|&i| nodes[i].parent.is_none()).map(|i| build(i, &nodes)).collect();
    if tops.len() == 1 && root.props.is_empty() && root.warnings.is_empty() {
        return tops.into_iter().next().expect("one");
    }
    root.children = tops;
    root
}

// -------------------------------------------------------------- Hive

/// Hive `EXPLAIN` (indented stages and operators) with the row estimates
/// of `Statistics: Num rows: N …` taken into `est_rows`.
pub fn hive_tree(text: &str) -> PlanNode {
    let mut root = tree_from_indented_text(text);
    fn enrich(n: &mut PlanNode) {
        if let Some((_, v)) = n.props.iter().find(|(k, _)| k == "Statistics") {
            n.est_rows = after(v, "Num rows: ").and_then(number);
        }
        if let Some((_, v)) = n.props.iter().find(|(k, _)| k == "alias") {
            n.object = Some(v.clone());
        }
        for c in &mut n.children {
            enrich(c);
        }
    }
    enrich(&mut root);
    root
}

// ----------------------------------------------------------- Teradata

/// Teradata `EXPLAIN` prose: one node per numbered step (sub-steps of a
/// parallel step nested), with the step's estimates picked from the text.
pub fn teradata_tree(text: &str) -> PlanNode {
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    let mut root = PlanNode { op: "EXPLAIN".into(), ..Default::default() };
    let mut cur: Option<(usize, String, String)> = None;
    let mut last_step = false;
    // Indent of the top-level steps; sub-steps of a parallel step sit
    // further right.
    let mut base: Option<usize> = None;
    let flush = |cur: &mut Option<(usize, String, String)>, items: &mut Vec<(usize, PlanNode)>, base: usize| {
        if let Some((indent, num, text)) = cur.take() {
            items.push((if indent >= base + 4 { 2 } else { 1 }, teradata_step(&num, &text)));
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // "  3) We do an all-AMPs …"
        let numbered = t.split_once(") ").filter(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((num, rest)) = numbered {
            let b = *base.get_or_insert(indent);
            flush(&mut cur, &mut items, b);
            cur = Some((indent, num.to_string(), rest.to_string()));
        } else if let Some(rest) = t.strip_prefix("-> ") {
            flush(&mut cur, &mut items, base.unwrap_or(0));
            root.detail = rest.to_string();
            last_step = true;
        } else if last_step {
            root.detail.push(' ');
            root.detail.push_str(t);
        } else if let Some((_, _, text)) = cur.as_mut() {
            text.push(' ');
            text.push_str(t);
        } else {
            root.props.push(("Detalle".into(), t.to_string()));
        }
    }
    flush(&mut cur, &mut items, base.unwrap_or(0));
    if let Some(s) = after(&root.detail, "total estimated time is ") {
        root.props.push(("Tiempo total estimado (s)".into(), s.to_string()));
    }
    if let Some(tree) = nest(items) {
        if tree.op == "PLAN" {
            root.children = tree.children;
        } else {
            root.children.push(tree);
        }
    }
    root
}

fn teradata_step(num: &str, text: &str) -> PlanNode {
    let lower = text.to_ascii_lowercase();
    let op = [
        ("merge join", "Merge join"),
        ("product join", "Product join"),
        ("hash join", "Hash join"),
        ("nested join", "Nested join"),
        ("exclusion", "Exclusion join"),
        ("all-amps retrieve", "All-AMPs retrieve"),
        ("single-amp retrieve", "Single-AMP retrieve"),
        ("group-amps", "Group-AMPs retrieve"),
        ("sum step", "Sum step"),
        ("sort", "Sort"),
        ("lock", "Lock"),
        ("in parallel", "Parallel steps"),
        ("insert", "Insert"),
        ("update", "Update"),
        ("delete", "Delete"),
        ("spool", "Spool"),
    ]
    .iter()
    .find(|(k, _)| lower.contains(k))
    .map_or("Step", |(_, v)| v);
    let mut n = PlanNode { op: op.to_string(), detail: text.to_string(), ..Default::default() };
    n.props.push(("Paso".into(), num.to_string()));
    if let Some(i) = lower.find(" from ") {
        let obj = text[i + 6..].split_whitespace().next().unwrap_or_default().trim_end_matches([',', '.']);
        if !obj.is_empty() && !obj.eq_ignore_ascii_case("spool") {
            n.object = Some(obj.to_string());
        }
    }
    if let Some(i) = lower.find("confidence to be ") {
        n.est_rows = text[i + 17..].split_whitespace().next().and_then(number);
    }
    if let Some(i) = lower.find("estimated time for this step is ") {
        let s = text[i + 32..].split_whitespace().next().unwrap_or_default();
        n.props.push(("Tiempo estimado (s)".into(), s.to_string()));
    }
    if let Some(i) = lower.find(" confidence") {
        if let Some(level) = lower[..i].split_whitespace().last() {
            n.props.push(("Confianza".into(), level.to_string()));
        }
    }
    if lower.contains("all-rows scan") {
        n.warnings.push("Recorre todas las filas (all-rows scan)".into());
    }
    if lower.contains("product join") {
        n.warnings.push("Product join".into());
    }
    if lower.contains("no confidence") {
        n.warnings.push("Sin estadísticas (no confidence)".into());
    }
    n
}

// ------------------------------------------------------------- Vertica

/// Vertica `EXPLAIN`: the `Access Path:` tree, one `+-` operator per line
/// with `[Cost: c, Rows: r]`, nested by the column of its `+`.
pub fn vertica_tree(text: &str) -> Option<PlanNode> {
    let mut items: Vec<(usize, PlanNode)> = Vec::new();
    let mut in_path = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Access Path:") {
            in_path = true;
            continue;
        }
        if !in_path {
            continue;
        }
        if t.is_empty() || t.starts_with("----") || t.starts_with("GraphViz") {
            if !items.is_empty() {
                break;
            }
            continue;
        }
        if let Some(plus) = line.find("+-") {
            if line[..plus].chars().all(|c| c == ' ' || c == '|') {
                let mut rest = line[plus + 1..].trim_start_matches('-');
                // `+-- Outer -> STORAGE ACCESS…`, `+---> GROUPBY…`
                let mut side = String::new();
                if let Some(arrow) = rest.find('>') {
                    let before = rest[..arrow].trim().trim_end_matches('-').trim();
                    side = before.to_string();
                    rest = &rest[arrow + 1..];
                }
                let body = rest.trim();
                let (head, bracket) = match body.find(" [") {
                    Some(i) => (&body[..i], &body[i + 2..]),
                    None => (body, ""),
                };
                let mut n = PlanNode { op: head.trim().to_string(), detail: side, ..Default::default() };
                if let Some(obj) = n.op.split_once(" for ").map(|(_, o)| o.trim().to_string()) {
                    n.object = Some(obj);
                    n.op = n.op.split(" for ").next().unwrap_or_default().to_string();
                }
                n.total_cost = after(bracket, "Cost: ").and_then(number);
                n.est_rows = after(bracket, "Rows: ").and_then(number);
                if let Some(p) = after(body, "(PATH ID: ") {
                    n.props.push(("Path id".into(), p.to_string()));
                }
                if body.contains("NO STATISTICS") {
                    n.warnings.push("Sin estadísticas".into());
                }
                items.push((plus, n));
                continue;
            }
        }
        // `| |      Projection: public.t_super`
        let body = t.trim_start_matches(['|', ' ']).trim();
        if let (Some((k, v)), Some((_, n))) = (body.split_once(": "), items.last_mut()) {
            n.props.push((k.to_string(), v.to_string()));
        }
    }
    nest(items)
}

// ------------------------------------------------------------ Netezza

/// Netezza `EXPLAIN VERBOSE`: the `QUERY PLANTEXT:` section (PostgreSQL
/// style, `l:` / `r:` marking join inputs) when present, else everything.
pub fn netezza_tree(text: &str) -> PlanNode {
    let plantext = text.split_once("QUERY PLANTEXT:").map(|(_, p)| p).unwrap_or(text);
    let cleaned: String = plantext
        .lines()
        .map(|l| {
            let t = l.trim_start();
            let indent = &l[..l.len() - t.len()];
            match t.strip_prefix("l: ").or_else(|| t.strip_prefix("r: ")) {
                Some(rest) => format!("{indent}-> {rest}"),
                None => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut root = tree_from_indented_text(&cleaned);
    fn costs(n: &mut PlanNode) {
        if let Some(i) = n.op.find(" (cost=") {
            let figures = n.op[i + 2..].trim_end_matches(')').to_string();
            n.op = n.op[..i].trim().to_string();
            n.total_cost = after(&figures, "..").and_then(number);
            n.est_rows = after(&figures, "rows=").and_then(number);
            if let Some(c) = after(&figures, "conf=") {
                n.props.push(("Confianza".into(), c.to_string()));
            }
        }
        if let Some(t) = n.op.split_once(" table ").map(|(_, t)| t.split_whitespace().next().unwrap_or_default().trim_matches('"').to_string()) {
            n.object = Some(t);
        }
        if n.op.starts_with("Sequential Scan") {
            n.warnings.push("Recorre la tabla completa".into());
        }
        for c in &mut n.children {
            costs(c);
        }
    }
    costs(&mut root);
    root
}

// ------------------------------------------------------------- Dameng

/// Dameng (DM8) `EXPLAIN`: `N  #OP: [cost, rows, width]; details` lines,
/// nested by the indentation after the line number.
pub fn dameng_tree(text: &str) -> Option<PlanNode> {
    let mut items = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        let rest = &t[digits..];
        let Some(hash) = rest.find('#') else { continue };
        let depth = rest[..hash].len();
        let body = &rest[hash + 1..];
        let (op, figures) = body.split_once(": ").unwrap_or((body, ""));
        let mut n = PlanNode { op: op.trim().to_string(), ..Default::default() };
        if let Some(b) = figures.strip_prefix('[') {
            let (nums, tail) = b.split_once(']').unwrap_or((b, ""));
            let v: Vec<Option<f64>> = nums.split(',').map(number).collect();
            n.total_cost = v.first().copied().flatten();
            n.est_rows = v.get(1).copied().flatten();
            if let Some(Some(w)) = v.get(2) {
                n.props.push(("Ancho de fila".into(), format!("{w}")));
            }
            n.detail = tail.trim_start_matches(';').trim().to_string();
        }
        if n.op.starts_with("CSCN") {
            n.warnings.push("Recorre la tabla completa".into());
        }
        if let Some(obj) = n.detail.split_once('(').and_then(|(_, r)| r.split(')').next()).filter(|o| !o.is_empty()) {
            if n.op.contains("SCN") || n.op.contains("SEEK") {
                n.object = Some(obj.to_string());
            }
        }
        items.push((depth, n));
    }
    nest(items)
}

// ------------------------------------------------------------- Exasol

/// `EXA_USER_PROFILE_LAST_DAY` row of one statement.
#[derive(Debug, Clone, Default)]
pub struct ExaPart {
    pub name: String,
    pub info: String,
    pub object: String,
    pub object_rows: Option<f64>,
    pub out_rows: Option<f64>,
    pub duration_s: Option<f64>,
    pub cpu: Option<f64>,
    pub mem_mib: Option<f64>,
    pub remarks: String,
}

/// Exasol's profile parts run one after the other, each on the output of
/// the one before: a chain whose last part is the root.
pub fn exasol_tree(parts: &[ExaPart]) -> PlanNode {
    let mut chain: Option<PlanNode> = None;
    let mut total = 0.0;
    for p in parts {
        let mut n = PlanNode {
            op: p.name.clone(),
            detail: p.info.clone(),
            object: Some(p.object.clone()).filter(|o| !o.is_empty()),
            actual_rows: p.out_rows,
            actual_ms: p.duration_s.map(|s| s * 1000.0),
            ..Default::default()
        };
        total += p.duration_s.unwrap_or_default();
        for (k, v) in [("Filas del objeto", p.object_rows), ("CPU (%)", p.cpu), ("Memoria temporal (MiB)", p.mem_mib)] {
            if let Some(v) = v {
                n.props.push((k.into(), format!("{v}")));
            }
        }
        if !p.remarks.is_empty() {
            n.props.push(("Observaciones".into(), p.remarks.clone()));
        }
        if p.name.contains("INDEX CREATE") {
            n.warnings.push("Creó un índice durante la consulta".into());
        }
        if p.name.contains("SCAN") && p.object_rows.is_some_and(|r| r > 1_000_000.0) && p.info.contains("FULL") {
            n.warnings.push("Recorre la tabla completa".into());
        }
        if let Some(prev) = chain.take() {
            n.children.push(prev);
        }
        chain = Some(n);
    }
    let mut root = chain.unwrap_or_else(|| PlanNode { op: "PROFILE".into(), ..Default::default() });
    root.props.push(("Duración total (s)".into(), format!("{total:.3}")));
    root
}

// ------------------------------------------------------------- CUBRID

/// CUBRID `SHOW TRACE` (text): the plan and the trace statistics, with the
/// `(time: t, fetch: f, …, rows: r)` figures picked up.
pub fn cubrid_tree(text: &str) -> PlanNode {
    let mut root = tree_from_indented_text(text);
    fn figures(n: &mut PlanNode) {
        if let Some(t) = after(&n.op, "time: ").and_then(number) {
            n.actual_ms = Some(t);
        }
        if let Some(r) = after(&n.op, " rows: ").and_then(number) {
            n.actual_rows = Some(r);
        }
        if let Some(i) = n.op.find(" (") {
            n.detail = n.op[i + 1..].to_string();
            n.op = n.op[..i].to_string();
        }
        if let Some(t) = after(&n.detail, "table: ") {
            n.object = Some(t.to_string());
        }
        for c in &mut n.children {
            figures(c);
        }
    }
    figures(&mut root);
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn numbers() {
        assert_eq!(number("1.00K"), Some(1000.0));
        assert_eq!(number("2M"), Some(2_000_000.0));
        assert_eq!(number("1,234"), Some(1234.0));
        assert_eq!(number("5.0E+3"), Some(5000.0));
    }

    #[test]
    fn sql_server_rows() {
        let rows = vec![
            row(&[("StmtText", "select * from t where id > 1"), ("StmtId", "1"), ("NodeId", "1"), ("Parent", "0"), ("PhysicalOp", ""), ("Type", "SELECT"), ("TotalSubtreeCost", "0.5"), ("EstimateRows", "10")]),
            row(&[("StmtId", "1"), ("NodeId", "2"), ("Parent", "1"), ("PhysicalOp", "Nested Loops"), ("LogicalOp", "Inner Join"), ("TotalSubtreeCost", "0.5"), ("EstimateRows", "10"), ("Rows", "8"), ("Executes", "1")]),
            row(&[("StmtId", "1"), ("NodeId", "3"), ("Parent", "2"), ("PhysicalOp", "Clustered Index Seek"), ("LogicalOp", "Clustered Index Seek"), ("Argument", "OBJECT:([db].[dbo].[t].[PK_t]), SEEK:([id] > 1)"), ("TotalSubtreeCost", "0.2")]),
            row(&[("StmtId", "1"), ("NodeId", "4"), ("Parent", "2"), ("PhysicalOp", "Table Scan"), ("LogicalOp", "Table Scan"), ("Argument", "OBJECT:([db].[dbo].[u])"), ("Warnings", "NO STATS:([u].[x])")]),
        ];
        let trees = sqlserver_trees(&rows);
        assert_eq!(trees.len(), 1);
        let (text, root) = &trees[0];
        assert_eq!(text, "select * from t where id > 1");
        assert_eq!(root.op, "SELECT");
        assert_eq!(root.total_cost, Some(0.5));
        let nl = &root.children[0];
        assert_eq!((nl.op.as_str(), nl.detail.as_str()), ("Nested Loops", "Inner Join"));
        assert_eq!((nl.actual_rows, nl.executions), (Some(8.0), Some(1.0)));
        assert_eq!(nl.children[0].object.as_deref(), Some("[db].[dbo].[t].[PK_t]"));
        assert_eq!(nl.children[1].warnings.len(), 2);
    }

    #[test]
    fn db2_streams() {
        let op = |id: &str, kind: &str, cost: f64| Db2Op { id: id.into(), kind: kind.into(), total_cost: Some(cost), ..Default::default() };
        let st = |st: &str, sid: &str, tt: &str, tid: &str, obj: &str, n: f64| Db2Stream {
            source_type: st.into(),
            source_id: sid.into(),
            target_type: tt.into(),
            target_id: tid.into(),
            object: obj.into(),
            count: Some(n),
        };
        let ops = [op("1", "RETURN", 20.0), op("2", "HSJOIN", 19.0), op("3", "TBSCAN", 10.0), op("4", "IXSCAN", 5.0)];
        let streams = [
            st("O", "2", "O", "1", "", 7.0),
            st("O", "3", "O", "2", "", 100.0),
            st("O", "4", "O", "2", "", 7.0),
            st("D", "-1", "O", "3", "DB2INST1.T", 100.0),
            st("D", "-1", "O", "4", "DB2INST1.IX_U", 50.0),
        ];
        let root = db2_tree(&ops, &streams);
        assert_eq!(root.op, "RETURN");
        assert_eq!(root.est_rows, Some(7.0));
        let join = &root.children[0];
        assert_eq!(join.op, "HSJOIN");
        assert_eq!(join.children[0].object.as_deref(), Some("DB2INST1.T"));
        assert_eq!(join.children[0].warnings.len(), 1);
        assert_eq!(join.children[1].est_rows, Some(7.0));
    }

    #[test]
    fn db2zos_steps() {
        let r = |q: &str, _planno: &str, m: &str, t: &str, a: &str| ZRow {
            qblock: q.into(),
            method: m.into(),
            table: t.into(),
            access_type: a.into(),
            ..Default::default()
        };
        let root = db2zos_tree(&[r("1", "1", "0", "A", "R"), r("1", "2", "1", "B", "I"), r("1", "3", "3", "", "")]);
        assert_eq!(root.op, "Sort");
        let join = &root.children[0];
        assert_eq!(join.op, "Nested loop join");
        assert_eq!(join.children[0].warnings.len(), 1);
        assert_eq!(join.children[1].object.as_deref(), Some("B"));
    }

    #[test]
    fn sybase_showplan() {
        let t = "\
QUERY PLAN FOR STATEMENT 1 (at line 1).
Optimized using Serial Mode

    STEP 1
        The type of query is SELECT.

        2 operator(s) under root

       |ROOT:EMIT Operator (VA = 2)
       |
       |   |NESTED LOOP JOIN Operator (Join Type: Inner Join) (VA = 1)
       |   |
       |   |   |SCAN Operator (VA = 0)
       |   |   |  FROM TABLE
       |   |   |  titles
       |   |   |  Table Scan.
       |   |   |  Using I/O Size 16 Kbytes for data pages.
       |   |
       |   |   |SCAN Operator (VA = 3)
       |   |   |  FROM TABLE
       |   |   |  authors
       |   |   |  Index : au_idx
Total estimated I/O cost for statement 1 (at line 1): 81.
";
        let plans = sybase_trees(t);
        assert_eq!(plans.len(), 1);
        let root = &plans[0];
        assert_eq!(root.op, "SELECT");
        assert_eq!(root.total_cost, Some(81.0));
        let emit = &root.children[0];
        assert_eq!(emit.op, "EMIT");
        let join = &emit.children[0];
        assert_eq!(join.op, "NESTED LOOP JOIN");
        assert_eq!(join.detail, "Inner Join");
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].object.as_deref(), Some("titles"));
        assert_eq!(join.children[0].warnings, ["Recorre la tabla completa"]);
        assert!(join.children[1].props.contains(&("Index".to_string(), "au_idx".to_string())));
    }

    #[test]
    fn impala_explain() {
        let t = "\
Max Per-Host Resource Reservation: Memory=2.94MB Threads=5
WARNING: The following tables are missing relevant table and/or column statistics.
default.b

PLAN-ROOT SINK
|
05:AGGREGATE [FINALIZE]
|  output: count:merge(*)
|  row-size=8B cardinality=1
|
04:EXCHANGE [UNPARTITIONED]
|
03:HASH JOIN [INNER JOIN, BROADCAST]
|  hash predicates: a.id = b.id
|  row-size=8B cardinality=1.20K
|
|--02:EXCHANGE [BROADCAST]
|  |
|  01:SCAN HDFS [default.b]
|     row-size=4B cardinality=unavailable
|
00:SCAN HDFS [default.a]
   partitions=1/1 files=1 size=10B
   row-size=4B cardinality=3
";
        let root = impala_tree(t);
        assert_eq!(root.op, "QUERY");
        assert_eq!(root.warnings.len(), 1);
        let sink = &root.children[0];
        assert_eq!(sink.op, "PLAN-ROOT SINK");
        let agg = &sink.children[0];
        assert_eq!((agg.op.as_str(), agg.detail.as_str(), agg.est_rows), ("AGGREGATE", "FINALIZE", Some(1.0)));
        let join = &agg.children[0].children[0];
        assert_eq!(join.op, "HASH JOIN");
        assert_eq!(join.est_rows, Some(1200.0));
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].op, "EXCHANGE");
        assert_eq!(join.children[0].children[0].object.as_deref(), Some("default.b"));
        assert_eq!(join.children[1].object.as_deref(), Some("default.a"));
        assert_eq!(join.children[1].est_rows, Some(3.0));
    }

    #[test]
    fn hive_statistics() {
        let t = "\
STAGE DEPENDENCIES:
  Stage-0 is a root stage

STAGE PLANS:
  Stage: Stage-0
    Fetch Operator
      limit: -1
      Processor Tree:
        TableScan
          alias: t
          Statistics: Num rows: 5 Data size: 20 Basic stats: COMPLETE Column stats: NONE
          Select Operator
            Statistics: Num rows: 5 Data size: 20 Basic stats: COMPLETE Column stats: NONE
";
        let root = hive_tree(t);
        let scan = &root.children[1].children[0].children[0].children[0];
        assert_eq!(scan.op, "TableScan");
        assert_eq!(scan.object.as_deref(), Some("t"));
        assert_eq!(scan.est_rows, Some(5.0));
    }

    #[test]
    fn teradata_prose() {
        let t = "\
  1) First, we lock DB.T for read on a reserved RowHash to prevent global deadlock.
  2) Next, we do an all-AMPs RETRIEVE step from DB.T by way of an
     all-rows scan with no residual conditions into Spool 1
     (group_amps), which is built locally on the AMPs.  The size of
     Spool 1 is estimated with high confidence to be 1,200 rows (30,000
     bytes).  The estimated time for this step is 0.03 seconds.
  3) We execute the following steps in parallel.
       1) We do a single-AMP RETRIEVE step from DB.U by way of the unique primary index.
       2) We do an all-AMPs SUM step to aggregate from Spool 1.
  4) Finally, we send out an END TRANSACTION step to all AMPs involved
     in processing the request.
  -> The contents of Spool 1 are sent back to the user as the result of
     statement 1.  The total estimated time is 0.03 seconds.
";
        let root = teradata_tree(t);
        assert_eq!(root.children.len(), 4);
        let step2 = &root.children[1];
        assert_eq!(step2.op, "All-AMPs retrieve");
        assert_eq!(step2.object.as_deref(), Some("DB.T"));
        assert_eq!(step2.est_rows, Some(1200.0));
        assert_eq!(step2.warnings.len(), 1);
        assert_eq!(root.children[2].children.len(), 2);
        assert_eq!(root.children[2].children[0].op, "Single-AMP retrieve");
        assert!(root.props.contains(&("Tiempo total estimado (s)".to_string(), "0.03".to_string())));
    }

    #[test]
    fn vertica_access_path() {
        let t = "\
 ------------------------------
 QUERY PLAN DESCRIPTION:
 ------------------------------

 Access Path:
 +-GROUPBY HASH (LOCAL RESEGMENT GROUPS) [Cost: 21, Rows: 10] (PATH ID: 1)
 |  Aggregates: sum(t.x)
 |  Group By: t.y
 | +---> JOIN HASH [Cost: 20, Rows: 1K] (PATH ID: 2)
 | |      Join Cond: (a.id = b.id)
 | | +-- Outer -> STORAGE ACCESS for a [Cost: 5, Rows: 1K] (PATH ID: 3)
 | | |      Projection: public.a_super
 | | +-- Inner -> STORAGE ACCESS for b [Cost: 5, Rows: 10] (PATH ID: 4)

 ------------------------------
";
        let root = vertica_tree(t).unwrap();
        assert_eq!(root.op, "GROUPBY HASH (LOCAL RESEGMENT GROUPS)");
        assert_eq!((root.total_cost, root.est_rows), (Some(21.0), Some(10.0)));
        assert!(root.props.contains(&("Group By".to_string(), "t.y".to_string())));
        let join = &root.children[0];
        assert_eq!(join.op, "JOIN HASH");
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].detail, "Outer");
        assert_eq!(join.children[0].object.as_deref(), Some("a"));
        assert_eq!(join.children[0].est_rows, Some(1000.0));
        assert_eq!(join.children[1].op, "STORAGE ACCESS");
    }

    #[test]
    fn netezza_plantext() {
        let t = "\
QUERY SQL:
select count(*) from t join u on t.a = u.a

QUERY PLANTEXT:

Aggregate  (cost=0.0..2.5 rows=1 width=8 conf=0)
  l: Hash Join (cost=0.0..2.0 rows=10 width=4 conf=64)
      l: Sequential Scan table \"T\" (cost=0.0..0.0 rows=10 width=4 conf=100)
      r: Hash (cost=0.0..0.0 rows=5 width=4 conf=0)
          l: Sequential Scan table \"U\" (cost=0.0..0.0 rows=5 width=4 conf=100)
";
        let root = netezza_tree(t);
        assert_eq!(root.op, "Aggregate");
        assert_eq!(root.total_cost, Some(2.5));
        let join = &root.children[0];
        assert_eq!(join.op, "Hash Join");
        assert_eq!(join.est_rows, Some(10.0));
        assert_eq!(join.children[0].object.as_deref(), Some("T"));
        assert_eq!(join.children[0].warnings.len(), 1);
        assert_eq!(join.children[1].children[0].object.as_deref(), Some("U"));
    }

    #[test]
    fn dameng_lines() {
        let t = "1   #NSET2: [1, 3, 30] \n2     #PRJT2: [1, 3, 30]; exp_num(2), is_atom(FALSE) \n3       #CSCN2: [1, 3, 30]; INDEX33555484(T)\n";
        let root = dameng_tree(t).unwrap();
        assert_eq!(root.op, "NSET2");
        assert_eq!((root.total_cost, root.est_rows), (Some(1.0), Some(3.0)));
        let scan = &root.children[0].children[0];
        assert_eq!(scan.op, "CSCN2");
        assert_eq!(scan.object.as_deref(), Some("T"));
        assert_eq!(scan.warnings.len(), 1);
    }

    #[test]
    fn exasol_chain() {
        let p = |name: &str, rows: f64, d: f64| ExaPart { name: name.into(), out_rows: Some(rows), duration_s: Some(d), ..Default::default() };
        let root = exasol_tree(&[p("SCAN", 100.0, 0.01), p("JOIN", 10.0, 0.02), p("GROUP BY", 2.0, 0.005)]);
        assert_eq!(root.op, "GROUP BY");
        assert_eq!(root.actual_ms, Some(5.0));
        assert_eq!(root.children[0].children[0].op, "SCAN");
    }

    #[test]
    fn cubrid_trace() {
        let t = "Trace Statistics:\n  SELECT (time: 2, fetch: 3, ioread: 0)\n    SCAN (table: t), (heap time: 1, fetch: 2, ioread: 0, readrows: 5, rows: 5)\n";
        let root = cubrid_tree(t);
        let select = &root.children[0];
        assert_eq!(select.op, "SELECT");
        assert_eq!(select.actual_ms, Some(2.0));
        let scan = &select.children[0];
        assert_eq!(scan.op, "SCAN");
        assert_eq!(scan.actual_rows, Some(5.0));
        assert_eq!(scan.object.as_deref(), Some("t"));
    }
}

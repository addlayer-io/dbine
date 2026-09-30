//! BigQuery plans as [`PlanNode`] trees.
//!
//! - Estimated: a dry run (`jobs.insert` with `dryRun`), which validates
//!   and prices the statement without running it (nothing is billed).
//!   BigQuery builds its operator tree only when it runs a query, so this
//!   plan is the statement with the bytes it would process and one node
//!   per referenced table (with the table's size from `tables.get`).
//! - Actual: the job's `statistics.query.queryPlan`: its stages, each fed
//!   by `inputStages`; the final stage (the one nobody reads) is the root.
//!   Slot time goes to `self_cost`, so the UI's cost shares are shares of
//!   slot time.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;
use std::collections::BTreeMap;

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).filter(|n| n.is_finite())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `1536` → "1.5 KiB".
pub(crate) fn bytes(n: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", n as u64)
    } else {
        format!("{v:.1} {}", units[u])
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

/// `{projectId, datasetId, tableId}` → `project.dataset.table`.
pub(crate) fn table_name(t: &Value) -> String {
    ["projectId", "datasetId", "tableId"].iter().filter_map(|k| t.get(*k).and_then(Value::as_str)).collect::<Vec<_>>().join(".")
}

/// Statement types that have no plan worth showing (DDL, DCL…).
pub(crate) fn is_ddl(statement_type: &str) -> bool {
    let t = statement_type.to_ascii_uppercase();
    (t.starts_with("CREATE_") || t.starts_with("DROP_") || t.starts_with("ALTER_") || matches!(t.as_str(), "GRANT" | "REVOKE"))
        && t != "CREATE_TABLE_AS_SELECT"
}

/// Statements with no plan, told by their first words (DDL, DCL,
/// procedural statements), so no dry run is spent on them.
pub(crate) fn no_plan_text(stmt: &str) -> bool {
    let words: Vec<String> = stmt
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(64)
        .map(str::to_ascii_lowercase)
        .collect();
    match words.first().map(String::as_str) {
        Some("create") => !(words.iter().any(|w| w == "as") && words.iter().any(|w| w == "select")),
        Some("drop" | "alter" | "grant" | "revoke" | "declare" | "set" | "begin" | "commit" | "rollback" | "undrop") => true,
        _ => false,
    }
}

// ---- dry run -------------------------------------------------------------

/// The estimated plan of a dry-run job; `tables` are the referenced
/// tables' resources (`tables.get`), when they could be read.
pub(crate) fn dry_run(statement: &str, job: &Value, tables: &[(String, Option<Value>)]) -> Plan {
    let q = job.pointer("/statistics/query").cloned().unwrap_or(Value::Null);
    let st = q.get("statementType").map(text).filter(|s| !s.is_empty()).unwrap_or_else(|| "QUERY".into());
    let mut root = PlanNode { op: st, ..Default::default() };
    let processed = num(q.get("totalBytesProcessed")).or_else(|| num(job.pointer("/statistics/totalBytesProcessed")));
    if let Some(b) = processed {
        root.detail = format!("procesaría {}", bytes(b));
        root.props.push(("Bytes a procesar".into(), fmt_num(b)));
    }
    if let Some(a) = q.get("totalBytesProcessedAccuracy").and_then(Value::as_str) {
        root.props.push(("Precisión de la estimación".into(), a.to_string()));
    }
    root.props.push((
        "Nota".into(),
        "BigQuery arma las etapas al ejecutar: el plan con etapas es el real (Ejecutar con plan)".into(),
    ));
    for (name, meta) in tables {
        let mut t = PlanNode { op: "Tabla".into(), object: Some(name.clone()), ..Default::default() };
        if let Some(m) = meta {
            if let Some(kind) = m.get("type").and_then(Value::as_str) {
                t.op = match kind {
                    "VIEW" => "Vista".into(),
                    "MATERIALIZED_VIEW" => "Vista materializada".into(),
                    "EXTERNAL" => "Tabla externa".into(),
                    _ => "Tabla".into(),
                };
            }
            t.est_rows = num(m.get("numRows"));
            if let Some(b) = num(m.get("numBytes")) {
                t.detail = bytes(b);
                t.props.push(("Tamaño (bytes)".into(), fmt_num(b)));
            }
            if let Some(p) = m.get("timePartitioning") {
                let field = p.get("field").and_then(Value::as_str).unwrap_or("_PARTITIONTIME");
                t.props.push(("Particionada por".into(), format!("{field} ({})", p.get("type").map(text).unwrap_or_default())));
            }
            if let Some(p) = m.get("rangePartitioning") {
                t.props.push(("Particionada por rango".into(), p.get("field").map(text).unwrap_or_default()));
            }
            if let Some(Value::Array(f)) = m.pointer("/clustering/fields") {
                t.props.push(("Clustering".into(), f.iter().map(text).collect::<Vec<_>>().join(", ")));
            }
        }
        root.children.push(t);
    }
    Plan {
        statement: statement.into(),
        root,
        actual: false,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(job).unwrap_or_default(),
    }
}

// ---- queryPlan -----------------------------------------------------------

/// The measured plan of a finished job (`jobs.get`).
pub(crate) fn job_plan(statement: &str, job: &Value) -> Plan {
    let q = job.pointer("/statistics/query").cloned().unwrap_or(Value::Null);
    let stages = q.get("queryPlan").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut root = if stages.is_empty() {
        let mut n = PlanNode { op: q.get("statementType").map(text).filter(|s| !s.is_empty()).unwrap_or_else(|| "QUERY".into()), ..Default::default() };
        if q.get("cacheHit").and_then(Value::as_bool) == Some(true) {
            n.warnings.push("Resultado tomado de la caché: BigQuery no ejecutó la consulta (sin etapas)".into());
        } else {
            n.props.push(("Nota".into(), "BigQuery no informó etapas para este trabajo".into()));
        }
        n
    } else {
        stage_tree(&stages)
    };
    let mut top = Vec::new();
    if let Some(id) = job.pointer("/jobReference/jobId").and_then(Value::as_str) {
        top.push(("Trabajo".to_string(), id.to_string()));
    }
    if let Some(t) = q.get("statementType").and_then(Value::as_str) {
        top.push(("Tipo de sentencia".into(), t.to_string()));
    }
    for (label, key) in [
        ("Bytes procesados", "totalBytesProcessed"),
        ("Bytes facturados", "totalBytesBilled"),
        ("Slot-ms totales", "totalSlotMs"),
        ("Particiones procesadas", "totalPartitionsProcessed"),
        ("Filas modificadas (DML)", "numDmlAffectedRows"),
    ] {
        if let Some(v) = num(q.get(key)) {
            let shown = if key.starts_with("totalBytes") { format!("{} ({})", fmt_num(v), bytes(v)) } else { fmt_num(v) };
            top.push((label.into(), shown));
        }
    }
    if let Some(t) = q.get("billingTier") {
        top.push(("Nivel de facturación".into(), text(t)));
    }
    if let (Some(s), Some(e)) = (num(job.pointer("/statistics/startTime")), num(job.pointer("/statistics/endTime"))) {
        if e >= s {
            top.push(("Duración (ms)".into(), fmt_num(e - s)));
            if root.actual_ms.is_none() {
                root.actual_ms = Some(e - s);
            }
        }
    }
    root.props.splice(0..0, top);
    cumulate(&mut root);
    Plan {
        statement: statement.into(),
        root,
        actual: true,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&q).unwrap_or_default(),
    }
}

fn cumulate(n: &mut PlanNode) -> Option<f64> {
    let kids: Vec<Option<f64>> = n.children.iter_mut().map(cumulate).collect();
    if n.self_cost.is_none() && kids.iter().all(Option::is_none) {
        return None;
    }
    let t = n.self_cost.unwrap_or(0.0) + kids.into_iter().flatten().sum::<f64>();
    n.total_cost = Some(t);
    Some(t)
}

/// Stages into a tree by `inputStages`: a stage hangs from the first stage
/// that reads it; the root is the last stage nobody reads.
fn stage_tree(stages: &[Value]) -> PlanNode {
    let id_of = |s: &Value| s.get("id").map(text).unwrap_or_default();
    let mut consumer: BTreeMap<String, String> = BTreeMap::new();
    for s in stages {
        for input in s.get("inputStages").and_then(Value::as_array).into_iter().flatten() {
            consumer.entry(text(input)).or_insert_with(|| id_of(s));
        }
    }
    let by_id: BTreeMap<String, &Value> = stages.iter().map(|s| (id_of(s), s)).collect();
    fn build(id: &str, by_id: &BTreeMap<String, &Value>, consumer: &BTreeMap<String, String>, depth: usize) -> PlanNode {
        let s = by_id[id];
        let mut n = stage_node(s);
        if depth < 500 {
            for input in s.get("inputStages").and_then(Value::as_array).into_iter().flatten() {
                let i = text(input);
                if by_id.contains_key(&i) && consumer.get(&i).map(String::as_str) == Some(id) {
                    n.children.push(build(&i, by_id, consumer, depth + 1));
                }
            }
        }
        n
    }
    let roots: Vec<String> = stages.iter().map(id_of).filter(|id| !consumer.contains_key(id)).collect();
    let mut nodes: Vec<PlanNode> = roots.iter().rev().map(|id| build(id, &by_id, &consumer, 0)).collect();
    if nodes.len() == 1 {
        nodes.pop().expect("one")
    } else {
        PlanNode { op: "QUERY".into(), children: nodes, ..Default::default() }
    }
}

fn stage_node(s: &Value) -> PlanNode {
    let f = |k: &str| num(s.get(k));
    let name = s.get("name").map(text).unwrap_or_default();
    // "S02: Join+" → op "Join+", stage id kept in props.
    let op = name.split_once(": ").map_or(name.as_str(), |(_, o)| o).to_string();
    let mut n = PlanNode { op, ..Default::default() };
    n.props.push(("Etapa".into(), name.clone()));
    n.actual_rows = f("recordsWritten");
    n.self_cost = f("slotMs");
    n.executions = f("completedParallelInputs").or_else(|| f("parallelInputs"));
    n.actual_ms = match (f("startMs"), f("endMs")) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => f("computeMsMax"),
    };
    let mut kinds = Vec::new();
    let mut tables = Vec::new();
    for step in s.get("steps").and_then(Value::as_array).into_iter().flatten() {
        let kind = step.get("kind").map(text).unwrap_or_default();
        let subs: Vec<String> = step.get("substeps").and_then(Value::as_array).into_iter().flatten().map(text).collect();
        for sub in &subs {
            if let Some(t) = sub.strip_prefix("FROM ") {
                if !t.starts_with("__stage") {
                    tables.push(t.trim().to_string());
                }
            }
        }
        n.props.push((kind.clone(), subs.join("; ")));
        kinds.push(kind);
    }
    n.detail = kinds.join(" → ");
    if !tables.is_empty() {
        n.object = Some(tables.join(", "));
    }
    for (label, key) in [
        ("Registros leídos", "recordsRead"),
        ("Registros escritos", "recordsWritten"),
        ("Entradas paralelas", "parallelInputs"),
        ("Cómputo prom. (ms)", "computeMsAvg"),
        ("Cómputo máx. (ms)", "computeMsMax"),
        ("Espera prom. (ms)", "waitMsAvg"),
        ("Lectura prom. (ms)", "readMsAvg"),
        ("Escritura prom. (ms)", "writeMsAvg"),
        ("Bytes de shuffle", "shuffleOutputBytes"),
        ("Bytes de shuffle derramados", "shuffleOutputBytesSpilled"),
        ("Slot-ms", "slotMs"),
    ] {
        if let Some(v) = f(key) {
            n.props.push((label.into(), fmt_num(v)));
        }
    }
    if let Some(st) = s.get("status").and_then(Value::as_str) {
        n.props.push(("Estado".into(), st.to_string()));
    }
    if let Some(sp) = f("shuffleOutputBytesSpilled").filter(|b| *b > 0.0) {
        n.warnings.push(format!("El shuffle derramó {} a disco", bytes(sp)));
    }
    if let (Some(avg), Some(max)) = (f("computeMsAvg"), f("computeMsMax")) {
        if max >= 1000.0 && max > 5.0 * avg.max(1.0) {
            n.warnings.push(format!("Desbalance: el trabajador más lento tardó {} ms (promedio {} ms)", fmt_num(max), fmt_num(avg)));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jobs.get of `SELECT c.name, SUM(o.total) FROM ds.orders o JOIN
    /// ds.customers c ON o.cid = c.id GROUP BY 1` (shape as BigQuery
    /// returns it).
    const JOB: &str = r#"{
 "jobReference": {"projectId": "p", "jobId": "job_abc", "location": "US"},
 "statistics": {"creationTime": "1700000000000", "startTime": "1700000000100", "endTime": "1700000002600",
  "query": {
   "statementType": "SELECT", "totalBytesProcessed": "104857600", "totalBytesBilled": "104857600",
   "totalSlotMs": "9000", "cacheHit": false, "billingTier": 1,
   "queryPlan": [
    {"name": "S00: Input", "id": "0", "startMs": "1700000000200", "endMs": "1700000001200",
     "waitMsAvg": "1", "readMsAvg": "20", "computeMsAvg": "100", "computeMsMax": "4000", "writeMsAvg": "5",
     "shuffleOutputBytes": "3000000", "shuffleOutputBytesSpilled": "0", "recordsRead": "5000000", "recordsWritten": "5000000",
     "parallelInputs": "40", "completedParallelInputs": "40", "status": "COMPLETE", "slotMs": "6000",
     "steps": [{"kind": "READ", "substeps": ["$1:cid, $2:total", "FROM p.ds.orders"]},
               {"kind": "WRITE", "substeps": ["$1, $2", "TO __stage02_output"]}]},
    {"name": "S01: Input", "id": "1", "recordsRead": "1000", "recordsWritten": "1000", "computeMsAvg": "5", "computeMsMax": "6",
     "slotMs": "200", "status": "COMPLETE",
     "steps": [{"kind": "READ", "substeps": ["$10:id, $11:name", "FROM p.ds.customers"]}]},
    {"name": "S02: Join+", "id": "2", "inputStages": ["0", "1"], "recordsRead": "5001000", "recordsWritten": "12",
     "computeMsAvg": "300", "computeMsMax": "350", "shuffleOutputBytesSpilled": "2097152", "slotMs": "2500", "status": "COMPLETE",
     "steps": [{"kind": "READ", "substeps": ["FROM __stage00_output", "FROM __stage01_output"]},
               {"kind": "JOIN", "substeps": ["INNER HASH JOIN EACH WITH ALL ON $1 = $10"]},
               {"kind": "AGGREGATE", "substeps": ["GROUP BY $11", "$30 := SUM($2)"]}]},
    {"name": "S03: Output", "id": "3", "inputStages": ["2"], "recordsRead": "12", "recordsWritten": "12",
     "computeMsAvg": "2", "computeMsMax": "2", "slotMs": "300", "status": "COMPLETE",
     "steps": [{"kind": "WRITE", "substeps": ["$11, $30", "TO __stage03_output"]}]}
   ]
  }
 }
}"#;

    #[test]
    fn stages_into_a_tree() {
        let job: Value = serde_json::from_str(JOB).unwrap();
        let p = job_plan("q", &job);
        assert!(p.actual);
        let root = &p.root;
        assert_eq!(root.op, "Output");
        assert!(root.props.iter().any(|(k, v)| k == "Bytes facturados" && v.starts_with("104857600 (100.0 MiB)")));
        assert!(root.props.iter().any(|(k, v)| k == "Duración (ms)" && v == "2500"));
        assert_eq!(root.total_cost, Some(9000.0));
        let join = &root.children[0];
        assert_eq!((join.op.as_str(), join.detail.as_str()), ("Join+", "READ → JOIN → AGGREGATE"));
        assert!(join.object.is_none());
        assert!(join.warnings.iter().any(|w| w.contains("2.0 MiB")));
        let [orders, cust] = &join.children[..] else { panic!("{join:#?}") };
        assert_eq!((orders.object.as_deref(), orders.actual_rows), (Some("p.ds.orders"), Some(5_000_000.0)));
        assert_eq!(orders.actual_ms, Some(1000.0));
        assert_eq!(orders.executions, Some(40.0));
        assert!(orders.warnings.iter().any(|w| w.starts_with("Desbalance")));
        assert!(cust.warnings.is_empty());
        assert_eq!(cust.self_cost, Some(200.0));
    }

    #[test]
    fn cached_result_has_no_stages() {
        let job: Value = serde_json::from_str(r#"{"statistics":{"query":{"statementType":"SELECT","cacheHit":true,"totalBytesBilled":"0"}}}"#).unwrap();
        let p = job_plan("q", &job);
        assert_eq!(p.root.op, "SELECT");
        assert!(p.root.warnings[0].contains("caché"));
    }

    #[test]
    fn dry_run_plan() {
        let job: Value = serde_json::from_str(
            r#"{"statistics":{"totalBytesProcessed":"3145728","query":{"statementType":"SELECT","totalBytesProcessed":"3145728",
                "totalBytesProcessedAccuracy":"PRECISE","referencedTables":[{"projectId":"p","datasetId":"ds","tableId":"orders"}]}}}"#,
        )
        .unwrap();
        let meta: Value = serde_json::from_str(
            r#"{"type":"TABLE","numRows":"5000000","numBytes":"734003200","timePartitioning":{"type":"DAY","field":"day"},
                "clustering":{"fields":["cid"]}}"#,
        )
        .unwrap();
        let p = dry_run("q", &job, &[("p.ds.orders".into(), Some(meta))]);
        assert!(!p.actual);
        assert_eq!((p.root.op.as_str(), p.root.detail.as_str()), ("SELECT", "procesaría 3.0 MiB"));
        let t = &p.root.children[0];
        assert_eq!((t.op.as_str(), t.object.as_deref(), t.est_rows), ("Tabla", Some("p.ds.orders"), Some(5_000_000.0)));
        assert!(t.props.iter().any(|(k, v)| k == "Particionada por" && v == "day (DAY)"));
        assert_eq!(table_name(&serde_json::json!({"projectId":"p","datasetId":"d","tableId":"t"})), "p.d.t");
        assert!(is_ddl("CREATE_TABLE") && !is_ddl("CREATE_TABLE_AS_SELECT") && !is_ddl("SELECT"));
        assert!(no_plan_text("CREATE TABLE t (a INT64)") && !no_plan_text("create table t as select 1"));
        assert!(!no_plan_text("with x as (select 1) select * from x") && no_plan_text("DECLARE x INT64"));
    }
}

//! SQL Server Showplan XML → [`Plan`] trees, as SSMS draws them.
//!
//! A batch's XML holds one `Stmt*` element per statement; those with a
//! `QueryPlan` become plans. Each `RelOp` is an operator; its child
//! operators are the `RelOp`s nested inside it (at any depth, but not
//! inside another `RelOp`). Actual figures come from
//! `RunTimeInformation/RunTimeCountersPerThread`, summed over threads.

use dbine_driver::{Plan, PlanNode};
use roxmltree::{Document, Node};

/// Column name of the result set carrying a showplan.
pub const SHOWPLAN_COLUMN: &str = "Microsoft SQL Server 2005 XML Showplan";

pub fn parse_showplan(xml: &str, actual: bool) -> Result<Vec<Plan>, String> {
    let doc = Document::parse(xml).map_err(|e| e.to_string())?;
    let mut plans = Vec::new();
    for stmt in doc.descendants().filter(|n| n.is_element() && n.tag_name().name().starts_with("Stmt")) {
        let Some(qp) = child(stmt, "QueryPlan") else { continue };
        let Some(rel) = child(qp, "RelOp") else { continue };
        let mut root = rel_op(rel);
        // Statement-level node, as SSMS shows it (SELECT / INSERT… with the
        // statement's cost), so the first operator isn't mistaken for it.
        let kind = stmt.attribute("StatementType").unwrap_or("STATEMENT").to_string();
        let mut top = PlanNode {
            op: kind.clone(),
            total_cost: attr_f(stmt, "StatementSubTreeCost").or(root.total_cost),
            self_cost: Some(0.0),
            est_rows: attr_f(stmt, "StatementEstRows"),
            actual_rows: root.actual_rows,
            ..Default::default()
        };
        top.warnings = plan_warnings(qp);
        top.warnings.extend(missing_indexes(qp));
        for (k, v) in [
            ("Nivel de optimización", stmt.attribute("StatementOptmLevel")),
            ("Motivo de fin de optimización", stmt.attribute("StatementOptmEarlyAbortReason")),
            ("Hash del plan", stmt.attribute("QueryPlanHash")),
            ("Grado de paralelismo", qp.attribute("DegreeOfParallelism")),
            ("Memoria concedida (KB)", child(qp, "MemoryGrantInfo").and_then(|m| m.attribute("GrantedMemory"))),
            ("Tiempo de compilación (ms)", qp.attribute("CompileTime")),
            ("CPU de compilación (ms)", qp.attribute("CompileCPU")),
            ("Nivel de compatibilidad", stmt.attribute("CardinalityEstimationModelVersion")),
        ] {
            if let Some(v) = v {
                top.props.push((k.to_string(), v.to_string()));
            }
        }
        if let Some(t) = child(qp, "QueryTimeStats") {
            for (k, a) in [("CPU total (ms)", "CpuTime"), ("Tiempo total (ms)", "ElapsedTime")] {
                if let Some(v) = t.attribute(a) {
                    top.props.push((k.to_string(), v.to_string()));
                }
            }
            top.actual_ms = attr_f(t, "ElapsedTime");
        }
        fill_self_costs(&mut root);
        top.children.push(root);
        plans.push(Plan {
            statement: stmt.attribute("StatementText").unwrap_or_default().trim().to_string(),
            root: top,
            actual,
            raw_format: "showplan_xml".into(),
            raw: xml.to_string(),
        });
    }
    Ok(plans)
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn attr_f(n: Node, name: &str) -> Option<f64> {
    n.attribute(name).and_then(|v| v.parse().ok())
}

/// `RelOp`s directly below this one (not inside a deeper `RelOp`).
fn child_ops<'a, 'i>(rel: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    let mut out = Vec::new();
    let mut stack: Vec<Node> = rel.children().filter(|c| c.is_element()).collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if n.tag_name().name() == "RelOp" {
            out.push(n);
            continue;
        }
        let mut kids: Vec<Node> = n.children().filter(|c| c.is_element()).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

/// First element named `name` below `n` that belongs to this operator, not
/// to a nested `RelOp`.
fn own_descendant<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    let mut stack: Vec<Node> = n.children().filter(|c| c.is_element()).collect();
    stack.reverse();
    while let Some(c) = stack.pop() {
        match c.tag_name().name() {
            "RelOp" => continue,
            t if t == name => return Some(c),
            _ => {
                let mut kids: Vec<Node> = c.children().filter(|k| k.is_element()).collect();
                kids.reverse();
                stack.extend(kids);
            }
        }
    }
    None
}

fn rel_op(rel: Node) -> PlanNode {
    let physical = rel.attribute("PhysicalOp").unwrap_or("?").to_string();
    let logical = rel.attribute("LogicalOp").unwrap_or_default().to_string();
    let mut n = PlanNode {
        detail: if logical != physical { logical } else { String::new() },
        op: physical,
        total_cost: attr_f(rel, "EstimatedTotalSubtreeCost"),
        est_rows: attr_f(rel, "EstimateRows").or_else(|| attr_f(rel, "EstimatedRowsRead")),
        ..Default::default()
    };

    // The operator's own element (IndexScan, Hash, NestedLoops…): the first
    // element child that isn't bookkeeping.
    let body = rel.children().find(|c| {
        c.is_element() && !matches!(c.tag_name().name(), "OutputList" | "RunTimeInformation" | "Warnings" | "MemoryFractions" | "RunTimePartitionSummary" | "InternalInfo")
    });

    if let Some(body) = body {
        if let Some(obj) = own_descendant(body, "Object") {
            let clean = |a: &str| obj.attribute(a).map(|v| v.trim_matches(['[', ']']).to_string());
            let parts: Vec<String> = ["Schema", "Table", "Index"].iter().filter_map(|a| clean(a)).collect();
            if !parts.is_empty() {
                n.object = Some(parts.join("."));
            }
        }
        for (label, tag) in [
            ("Predicado de búsqueda", "SeekPredicates"),
            ("Predicado", "Predicate"),
            ("Residual", "ProbeResidual"),
            ("Claves hash", "HashKeysProbe"),
        ] {
            if let Some(p) = body.children().find(|c| c.is_element() && c.tag_name().name() == tag) {
                let text: Vec<&str> = p
                    .descendants()
                    .filter_map(|d| d.attribute("ScalarString"))
                    .take(1)
                    .collect();
                let cols = || -> String {
                    p.descendants()
                        .filter(|d| d.tag_name().name() == "ColumnReference")
                        .filter_map(|d| d.attribute("Column"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let v = text.first().map(|s| s.to_string()).unwrap_or_else(cols);
                if !v.is_empty() {
                    n.props.push((label.to_string(), v));
                }
            }
        }
        for a in ["Ordered", "ScanDirection", "ForcedIndex", "Optimized", "WithUnorderedPrefetch", "Lookup"] {
            if let Some(v) = body.attribute(a) {
                n.props.push((a.to_string(), v.to_string()));
            }
        }
        if body.attribute("Lookup") == Some("true") {
            n.detail = "Key Lookup".into();
        }
    }

    if let Some(outputs) = child(rel, "OutputList") {
        let cols: Vec<&str> = outputs.children().filter_map(|c| c.attribute("Column")).collect();
        if !cols.is_empty() {
            n.props.push(("Columnas de salida".into(), cols.join(", ")));
        }
    }

    // Actual figures, summed over threads (elapsed: the slowest thread).
    if let Some(rt) = child(rel, "RunTimeInformation") {
        let (mut rows, mut execs, mut ms, mut reads) = (0.0, 0.0, None::<f64>, 0.0);
        for t in rt.children().filter(|c| c.tag_name().name() == "RunTimeCountersPerThread") {
            rows += attr_f(t, "ActualRows").unwrap_or(0.0);
            execs += attr_f(t, "ActualExecutions").unwrap_or(0.0);
            reads += attr_f(t, "ActualLogicalReads").unwrap_or(0.0);
            if let Some(e) = attr_f(t, "ActualElapsedms") {
                ms = Some(ms.map_or(e, |m: f64| m.max(e)));
            }
        }
        n.actual_rows = Some(rows);
        n.executions = Some(execs);
        n.actual_ms = ms;
        if reads > 0.0 {
            n.props.push(("Lecturas lógicas".into(), reads.to_string()));
        }
        if let Some(est) = n.est_rows {
            let est_total = est * execs.max(1.0);
            if rows > 0.0 && est_total > 0.0 && (rows / est_total > 10.0 || est_total / rows > 10.0) {
                n.warnings.push(format!("Estimación desviada: {est_total:.0} estimadas vs {rows:.0} reales"));
            }
        }
    }

    for (k, a) in [
        ("Costo de E/S estimado", "EstimateIO"),
        ("Costo de CPU estimado", "EstimateCPU"),
        ("Tamaño promedio de fila (B)", "AvgRowSize"),
        ("Rebinds estimados", "EstimateRebinds"),
        ("Rewinds estimados", "EstimateRewinds"),
        ("Filas leídas estimadas", "EstimatedRowsRead"),
        ("Modo de ejecución", "EstimatedExecutionMode"),
        ("Paralelo", "Parallel"),
        ("Id de nodo", "NodeId"),
    ] {
        if let Some(v) = rel.attribute(a) {
            n.props.push((k.to_string(), v.to_string()));
        }
    }

    if let Some(w) = child(rel, "Warnings") {
        n.warnings.extend(warning_texts(w));
    }

    n.children = child_ops(rel).into_iter().map(rel_op).collect();
    n
}

/// SSMS's per-operator cost: its subtree cost minus its children's.
fn fill_self_costs(n: &mut PlanNode) {
    for c in &mut n.children {
        fill_self_costs(c);
    }
    if let Some(total) = n.total_cost {
        let below: f64 = n.children.iter().filter_map(|c| c.total_cost).sum();
        n.self_cost = Some((total - below).max(0.0));
    }
}

fn plan_warnings(qp: Node) -> Vec<String> {
    child(qp, "Warnings").map(warning_texts).unwrap_or_default()
}

fn warning_texts(w: Node) -> Vec<String> {
    let mut out = Vec::new();
    for c in w.children().filter(|c| c.is_element()) {
        let text = match c.tag_name().name() {
            "PlanAffectingConvert" => format!(
                "Conversión implícita que afecta el plan: {}",
                c.attribute("Expression").unwrap_or_default()
            ),
            "SpillToTempDb" => "Volcado a tempdb (memoria insuficiente)".to_string(),
            "NoJoinPredicate" => "Join sin predicado (producto cartesiano)".to_string(),
            "ColumnsWithNoStatistics" => "Columnas sin estadísticas".to_string(),
            "UnmatchedIndexes" => "Índices filtrados que no se pudieron usar".to_string(),
            "MemoryGrantWarning" => format!(
                "Advertencia de memoria concedida: {}",
                c.attribute("GrantWarningKind").unwrap_or_default()
            ),
            "HashSpillDetails" | "SortSpillDetails" => "Volcado a disco en hash/sort".to_string(),
            other => other.to_string(),
        };
        out.push(text);
    }
    for (a, text) in [("NoJoinPredicate", "Join sin predicado (producto cartesiano)")] {
        if w.attribute(a) == Some("true") || w.attribute(a) == Some("1") {
            out.push(text.to_string());
        }
    }
    out
}

/// "Índice faltante (impacto 87%): CREATE NONCLUSTERED INDEX …" per group.
fn missing_indexes(qp: Node) -> Vec<String> {
    let Some(mi) = child(qp, "MissingIndexes") else { return Vec::new() };
    let mut out = Vec::new();
    for group in mi.children().filter(|c| c.tag_name().name() == "MissingIndexGroup") {
        let impact = group.attribute("Impact").unwrap_or("?");
        for idx in group.children().filter(|c| c.tag_name().name() == "MissingIndex") {
            let table = format!(
                "{}.{}",
                idx.attribute("Schema").unwrap_or("[dbo]"),
                idx.attribute("Table").unwrap_or("?")
            );
            let cols = |usage: &str| -> Vec<String> {
                idx.children()
                    .filter(|g| g.tag_name().name() == "ColumnGroup" && g.attribute("Usage") == Some(usage))
                    .flat_map(|g| g.children().filter_map(|c| c.attribute("Name").map(str::to_string)))
                    .collect()
            };
            let mut keys = cols("EQUALITY");
            keys.extend(cols("INEQUALITY"));
            let include = cols("INCLUDE");
            let mut sql = format!("CREATE NONCLUSTERED INDEX [IX_sugerido] ON {table} ({})", keys.join(", "));
            if !include.is_empty() {
                sql.push_str(&format!(" INCLUDE ({})", include.join(", ")));
            }
            out.push(format!("Índice faltante (impacto {impact}%): {sql}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.564">
<BatchSequence><Batch><Statements>
<StmtSimple StatementText="select * from t where a = 1" StatementType="SELECT" StatementSubTreeCost="0.5" StatementEstRows="10">
 <QueryPlan DegreeOfParallelism="1">
  <MissingIndexes><MissingIndexGroup Impact="87.5"><MissingIndex Database="[db]" Schema="[dbo]" Table="[t]">
   <ColumnGroup Usage="EQUALITY"><Column Name="[a]" ColumnId="2"/></ColumnGroup>
   <ColumnGroup Usage="INCLUDE"><Column Name="[b]" ColumnId="3"/></ColumnGroup>
  </MissingIndex></MissingIndexGroup></MissingIndexes>
  <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="0.5">
   <OutputList><ColumnReference Column="a"/></OutputList>
   <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="200" ActualExecutions="1" ActualElapsedms="3"/></RunTimeInformation>
   <NestedLoops Optimized="false">
    <RelOp NodeId="1" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="10" EstimatedTotalSubtreeCost="0.1">
     <IndexScan Ordered="true"><Object Database="[db]" Schema="[dbo]" Table="[t]" Index="[ix]"/>
      <SeekPredicates><SeekPredicateNew><SeekKeys><Prefix ScanType="EQ"><RangeColumns><ColumnReference Column="a"/></RangeColumns></Prefix></SeekKeys></SeekPredicateNew></SeekPredicates>
     </IndexScan>
    </RelOp>
    <RelOp NodeId="2" PhysicalOp="Clustered Index Seek" LogicalOp="Clustered Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.3">
     <Warnings><PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(int,[b],0)"/></Warnings>
     <IndexScan Lookup="true"><Object Schema="[dbo]" Table="[t]" Index="[pk]"/>
      <Predicate><ScalarOperator ScalarString="[t].[c]&gt;(5)"/></Predicate>
     </IndexScan>
    </RelOp>
   </NestedLoops>
  </RelOp>
 </QueryPlan>
</StmtSimple>
<StmtSimple StatementText="set nocount on" StatementType="SET ON/OFF"/>
</Statements></Batch></BatchSequence></ShowPlanXML>"#;

    #[test]
    fn showplan_becomes_a_tree() {
        let plans = parse_showplan(XML, true).unwrap();
        assert_eq!(plans.len(), 1, "statements without a plan are skipped");
        let p = &plans[0];
        assert_eq!(p.statement, "select * from t where a = 1");
        assert_eq!(p.root.op, "SELECT");
        let nl = &p.root.children[0];
        assert_eq!(nl.op, "Nested Loops");
        assert_eq!(nl.detail, "Inner Join");
        assert_eq!(nl.actual_rows, Some(200.0));
        assert!(nl.warnings.iter().any(|w| w.contains("desviada")), "10 estimated vs 200 actual");
        assert_eq!(nl.children.len(), 2);
        assert_eq!(nl.object, None, "a join reads no table of its own");
        assert!((nl.self_cost.unwrap() - 0.1).abs() < 1e-9, "0.5 - 0.1 - 0.3");
        let seek = &nl.children[0];
        assert_eq!(seek.object.as_deref(), Some("dbo.t.ix"));
        let lookup = &nl.children[1];
        assert_eq!(lookup.detail, "Key Lookup");
        assert!(lookup.warnings[0].contains("CONVERT_IMPLICIT"));
        assert!(lookup.props.iter().any(|(k, v)| k == "Predicado" && v.contains("[t].[c]")));
    }

    #[test]
    fn missing_index_is_suggested() {
        let p = &parse_showplan(XML, false).unwrap()[0];
        let w = p.root.warnings.iter().find(|w| w.starts_with("Índice faltante")).unwrap();
        assert!(w.contains("87.5%"));
        assert!(w.contains("ON [dbo].[t] ([a]) INCLUDE ([b])"), "{w}");
    }
}

//! Dremio's text plans (`EXPLAIN PLAN INCLUDING ALL ATTRIBUTES FOR`, the
//! format it inherits from Drill) and query profiles.
//!
//! Each plan line is `MM-OO<indent>Operator(args) : rowType = …: rowcount
//! = N, cumulative cost = {a rows, b cpu, c io, d network, e memory}, id =
//! K`, where `MM-OO` is the major fragment and the operator id inside it,
//! and the indentation gives the nesting. A profile (`/profiles/{id}.json`)
//! reports, per fragment and operator, the records each input received and
//! the processing time: an operator's actual rows are what its parent
//! received from it.

use dbine_driver::{Plan, PlanNode};
use serde_json::Value;
use std::collections::HashMap;

/// A plan node with its `(major fragment, operator id)`.
struct Line {
    indent: usize,
    id: Option<(i64, i64)>,
    node: PlanNode,
}

fn parse_id(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// `{25.0 rows, 25.0 cpu, 25.0 io, 0.0 network, 0.0 memory}` → pairs.
fn cost_parts(s: &str) -> Vec<(String, f64)> {
    s.trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .filter_map(|p| {
            let mut it = p.split_whitespace();
            let v: f64 = it.next()?.parse().ok()?;
            Some((it.next()?.to_string(), v))
        })
        .collect()
}

fn node_of(text: &str) -> PlanNode {
    let (head, attrs) = match text.find(" : ") {
        Some(i) => (&text[..i], &text[i + 3..]),
        None => (text, ""),
    };
    let (op, detail) = match head.find('(') {
        Some(i) => (head[..i].trim(), head[i + 1..].trim_end().trim_end_matches(')')),
        None => (head.trim(), ""),
    };
    let mut n = PlanNode { op: op.to_string(), detail: detail.to_string(), ..Default::default() };
    // `table=[[cp, a.parquet]]` (Drill style) or `table=["$scratch".t]`.
    if let Some(i) = detail.find("table=[") {
        let rest = &detail[i + 7..];
        let t = match rest.strip_prefix('[') {
            Some(r) => r.split("]]").next().unwrap_or(r).split(", ").collect::<Vec<_>>().join("."),
            None => rest.split(']').next().unwrap_or(rest).replace('"', ""),
        };
        if !t.is_empty() {
            n.object = Some(t);
        }
    }
    if let Some(i) = attrs.find("rowcount = ") {
        n.est_rows = attrs[i + 11..].split(',').next().and_then(|v| v.trim().parse().ok());
    }
    if let Some(i) = attrs.find("cumulative cost = {") {
        let rest = &attrs[i + 18..];
        if let Some(end) = rest.find('}') {
            let parts = cost_parts(&rest[..=end]);
            let get = |k: &str| parts.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap_or(0.0);
            n.total_cost = Some(get("cpu") + get("io") + get("network"));
            for (k, v) in parts {
                n.props.push((format!("costo acumulado ({k})"), v.to_string()));
            }
        }
    }
    if let Some(i) = attrs.find("rowType = ") {
        let rt = attrs[i + 10..].split("): ").next().unwrap_or("");
        n.props.push(("rowType".into(), format!("{rt})")));
    }
    n
}

/// The operator tree of a plan text, with each node's `(major, op id)`.
fn tree(text: &str) -> (PlanNode, Vec<Option<(i64, i64)>>) {
    let mut lines: Vec<Line> = Vec::new();
    for raw in text.lines() {
        let t = raw.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        let (id, rest) = match t.split_once(char::is_whitespace) {
            Some((id, rest)) if parse_id(id).is_some() => (parse_id(id), rest),
            _ => (None, t),
        };
        let indent = rest.len() - rest.trim_start().len();
        lines.push(Line { indent, id, node: node_of(rest.trim()) });
    }
    // Build by indentation; remember each node's id in pre-order.
    fn build(lines: &mut std::iter::Peekable<std::vec::IntoIter<Line>>, ids: &mut Vec<Option<(i64, i64)>>) -> Option<PlanNode> {
        let line = lines.next()?;
        ids.push(line.id);
        let mut node = line.node;
        while lines.peek().is_some_and(|l| l.indent > line.indent) {
            if let Some(c) = build(lines, ids) {
                node.children.push(c);
            }
        }
        Some(node)
    }
    let mut ids = Vec::new();
    let mut it = lines.into_iter().peekable();
    let mut roots = Vec::new();
    while let Some(n) = build(&mut it, &mut ids) {
        roots.push(n);
    }
    let root = if roots.len() == 1 {
        roots.pop().unwrap_or_default()
    } else {
        PlanNode { op: "PLAN".into(), children: roots, ..Default::default() }
    };
    (root, ids)
}

pub fn estimated(stmt: &str, text: &str) -> Plan {
    Plan { statement: stmt.to_string(), root: tree(text).0, actual: false, raw_format: "text".into(), raw: text.to_string() }
}

/// Per operator: records received per input, processing ns, peak memory,
/// minor fragments.
#[derive(Default, Debug)]
struct OpStats {
    inputs: Vec<f64>,
    nanos: f64,
    peak: f64,
    minors: usize,
    kind: String,
    output: Option<f64>,
}

fn op_stats(profile: &Value) -> HashMap<(i64, i64), OpStats> {
    let mut out: HashMap<(i64, i64), OpStats> = HashMap::new();
    for f in profile.get("fragmentProfile").and_then(Value::as_array).into_iter().flatten() {
        let major = f.get("majorFragmentId").and_then(Value::as_i64).unwrap_or(0);
        for m in f.get("minorFragmentProfile").and_then(Value::as_array).into_iter().flatten() {
            for o in m.get("operatorProfile").and_then(Value::as_array).into_iter().flatten() {
                let id = o.get("operatorId").and_then(Value::as_i64).unwrap_or(0);
                let s = out.entry((major, id)).or_default();
                s.minors += 1;
                s.nanos += o.get("processNanos").and_then(Value::as_f64).unwrap_or(0.0);
                s.peak = s.peak.max(o.get("peakLocalMemoryAllocated").and_then(Value::as_f64).unwrap_or(0.0));
                if let Some(k) = o.get("operatorTypeName").and_then(Value::as_str) {
                    s.kind = k.to_string();
                }
                if let Some(n) = o.get("outputRecords").and_then(Value::as_f64) {
                    s.output = Some(s.output.unwrap_or(0.0) + n);
                }
                for (i, inp) in o.get("inputProfile").and_then(Value::as_array).into_iter().flatten().enumerate() {
                    if s.inputs.len() <= i {
                        s.inputs.resize(i + 1, 0.0);
                    }
                    s.inputs[i] += inp.get("records").and_then(Value::as_f64).unwrap_or(0.0);
                }
            }
        }
    }
    out
}

/// The plan with the profile's actual figures.
pub fn actual(stmt: &str, text: &str, profile: &Value) -> Plan {
    let (mut root, ids) = tree(text);
    let stats = op_stats(profile);
    fn fill(n: &mut PlanNode, ids: &[Option<(i64, i64)>], at: &mut usize, stats: &HashMap<(i64, i64), OpStats>, rows_in: Option<f64>) {
        let me = ids.get(*at).copied().flatten();
        *at += 1;
        let s = me.and_then(|id| stats.get(&id));
        n.actual_rows = s.and_then(|s| s.output).or(rows_in).or_else(|| s.and_then(|s| s.inputs.first().copied()));
        if let Some(s) = s {
            n.actual_ms = Some(s.nanos / 1e6);
            n.executions = Some(s.minors as f64);
            n.props.push(("memoria pico".into(), format!("{:.0} bytes", s.peak)));
            if !s.kind.is_empty() {
                n.props.push(("operador".into(), s.kind.clone()));
            }
        }
        for (i, c) in n.children.iter_mut().enumerate() {
            let input = s.and_then(|s| s.inputs.get(i).copied());
            fill(c, ids, at, stats, input);
        }
    }
    let mut at = 0;
    fill(&mut root, &ids, &mut at, &stats, None);
    Plan { statement: stmt.to_string(), root, actual: true, raw_format: "text".into(), raw: text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT: &str = "00-00    Screen : rowType = RecordType(BIGINT EXPR$1): rowcount = 2.5, cumulative cost = {112.75 rows, 900.25 cpu, 30.0 io, 0.0 network, 528.0 memory}, id = 1557
00-01      HashJoin(condition=[=($0, $1)], joinType=[inner]) : rowType = RecordType(ANY a): rowcount = 25.0, cumulative cost = {60.0 rows, 370.0 cpu, 30.0 io, 0.0 network, 88.0 memory}, id = 1551
00-03        Scan(table=[[cp, tpch/nation.parquet]], groupscan=[x]) : rowType = RecordType(ANY a): rowcount = 25.0, cumulative cost = {25.0 rows, 25.0 cpu, 25.0 io, 0.0 network, 0.0 memory}, id = 1549
00-02        Scan(table=[[cp, tpch/region.parquet]], groupscan=[y]) : rowType = RecordType(ANY b): rowcount = 5.0, cumulative cost = {5.0 rows, 5.0 cpu, 5.0 io, 0.0 network, 0.0 memory}, id = 1550
";

    #[test]
    fn text_plan_tree() {
        let p = estimated("q", TEXT);
        assert_eq!(p.root.op, "Screen");
        assert_eq!(p.root.est_rows, Some(2.5));
        assert_eq!(p.root.total_cost, Some(930.25));
        let join = &p.root.children[0];
        assert_eq!((join.op.as_str(), join.children.len()), ("HashJoin", 2));
        assert!(join.detail.starts_with("condition="));
        assert_eq!(join.children[0].object.as_deref(), Some("cp.tpch/nation.parquet"));
        let tf = estimated("q", "00-00    Screen\n00-01      TableFunction(columns=[`id`], Table Function Type=[DATA_FILE_SCAN], table=[\"$scratch\".dbg]) : rowcount = 2.0\n");
        assert_eq!(tf.root.children[0].object.as_deref(), Some("$scratch.dbg"));
        let plain = estimated("q", "00-00    Screen\n00-01      Project(a=[$0])\n");
        assert_eq!(plain.root.children[0].op, "Project");
    }

    #[test]
    fn profile_figures() {
        let profile = json!({"fragmentProfile": [{"majorFragmentId": 0, "minorFragmentProfile": [{"operatorProfile": [
            {"operatorId": 0, "operatorTypeName": "SCREEN", "inputProfile": [{"records": 5}], "processNanos": 1000000},
            {"operatorId": 1, "operatorTypeName": "HASH_JOIN", "inputProfile": [{"records": 25}, {"records": 5}], "processNanos": 3000000},
            {"operatorId": 3, "inputProfile": [{"records": 25}], "processNanos": 0},
            {"operatorId": 2, "inputProfile": [{"records": 5}], "processNanos": 0}
        ]}]}]});
        let p = actual("q", TEXT, &profile);
        assert!(p.actual);
        assert_eq!(p.root.actual_rows, Some(5.0));
        let join = &p.root.children[0];
        assert_eq!((join.actual_rows, join.actual_ms), (Some(5.0), Some(3.0)));
        assert_eq!(join.children[0].actual_rows, Some(25.0));
        assert_eq!(join.children[1].actual_rows, Some(5.0));
        let with_output = json!({"fragmentProfile": [{"majorFragmentId": 0, "minorFragmentProfile": [{"operatorProfile": [
            {"operatorId": 3, "inputProfile": [{"records": 25}], "outputRecords": 7}
        ]}]}]});
        assert_eq!(actual("q", TEXT, &with_output).root.children[0].children[0].actual_rows, Some(7.0));
    }
}

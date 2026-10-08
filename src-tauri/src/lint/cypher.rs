//! Cypher rules (Neo4j, Memgraph, Neptune).

use super::lex::{closing, tokens, Options, Tok, C};
use super::Finding;

/// Words that start a clause: they end a MATCH pattern.
const CLAUSES: &[&str] = &[
    "match", "optional", "where", "return", "with", "create", "merge", "set", "delete", "detach", "remove", "unwind", "call", "order",
    "skip", "limit", "union", "foreach", "yield", "load", "using", "on", "finish",
];

/// One comma-separated part of a pattern.
#[derive(Default)]
struct Part {
    vars: Vec<String>,
    /// A node with a label or a property map, or a typed relationship.
    anchored: bool,
    /// The first node, for the report.
    first: Option<(usize, usize)>,
}

pub(super) fn lint(script: &str, out: &mut Vec<Finding>) {
    let all = tokens(script, Options { backtick_names: true, ..Options::default() });
    for stmt in all.split(|x| x.p(';') && x.depth == 0) {
        statement(stmt, out);
    }
}

fn statement(t: &[Tok], out: &mut Vec<Finding>) {
    let mut bound: Vec<String> = Vec::new();
    let mut filtered = false;
    let mut i = 0;
    while i < t.len() {
        let x = &t[i];
        if x.is("where") {
            filtered = true;
        }
        if x.is("as") || x.is("yield") || x.is("unwind") {
            // Names a WITH / UNWIND / CALL brings in.
            if let Some(n) = t.get(i + 1).filter(|n| matches!(n.k, C::Word | C::Name)) {
                bound.push(n.body().to_string());
            }
        }
        if x.is("detach") && t.get(i + 1).is_some_and(|n| n.is("delete")) && !filtered {
            out.push(Finding::new("detach-delete-all", x.start, t[i + 1].end));
        }
        let pattern = x.is("match") || x.is("merge") || x.is("create");
        if !pattern {
            i += 1;
            continue;
        }
        let d = x.depth;
        let end = (i + 1..t.len()).find(|&k| t[k].depth < d || (t[k].depth == d && CLAUSES.iter().any(|c| t[k].is(c)))).unwrap_or(t.len());
        let parts = parts(t, i + 1, end, d);
        if x.is("match") {
            for p in &parts {
                let known = p.vars.iter().any(|v| bound.contains(v));
                if !p.anchored && !known {
                    let (a, b) = p.first.unwrap_or((x.start, x.end));
                    out.push(Finding::new("match-without-label", a, b));
                }
            }
            // Parts that share no name with the ones before (directly or
            // through another part) multiply their rows.
            let mut group: Vec<usize> = (0..parts.len()).collect();
            for n in 0..parts.len() {
                for m in 0..n {
                    if parts[n].vars.iter().any(|v| parts[m].vars.contains(v)) {
                        let (from, to) = (group[n], group[m]);
                        group.iter_mut().filter(|g| **g == from).for_each(|g| *g = to);
                    }
                }
            }
            for (n, p) in parts.iter().enumerate().skip(1) {
                if !group[..n].contains(&group[n]) {
                    let (a, b) = p.first.unwrap_or((x.start, x.end));
                    out.push(Finding::new("cartesian-product", a, b));
                }
            }
            // A property map filters as a WHERE does.
            filtered |= (i + 1..end).any(|k| t[k].p('{'));
        }
        for p in parts {
            bound.extend(p.vars);
        }
        i = end;
    }
}

fn parts(t: &[Tok], a: usize, b: usize, d: u32) -> Vec<Part> {
    let mut out = vec![Part::default()];
    let mut i = a;
    while i < b {
        let x = &t[i];
        if x.p(',') && x.depth == d {
            out.push(Part::default());
            i += 1;
            continue;
        }
        let part = out.last_mut().unwrap();
        // `p = (a)-->(b)`: the path's name.
        if matches!(x.k, C::Word | C::Name) && t.get(i + 1).is_some_and(|n| n.p('=')) && x.depth == d {
            part.vars.push(x.body().to_string());
            i += 2;
            continue;
        }
        if x.p('(') && !(i > a && t[i - 1].k == C::Word) {
            // A node: (var:Label {props}).
            let close = closing(t, i).min(b);
            part.first.get_or_insert((x.start, t.get(close).map_or(x.end, |c| c.end)));
            element(t, i + 1, close, part);
            i = close + 1;
            continue;
        }
        if x.p('[') {
            // A relationship: [var:TYPE *1..3 {props}].
            let close = closing(t, i).min(b);
            element(t, i + 1, close, part);
            i = close + 1;
            continue;
        }
        i += 1;
    }
    out.retain(|p| p.first.is_some());
    out
}

/// The inside of a node or relationship.
fn element(t: &[Tok], a: usize, b: usize, part: &mut Part) {
    let mut k = a;
    if let Some(v) = t.get(k).filter(|v| k < b && matches!(v.k, C::Word | C::Name)) {
        part.vars.push(v.body().to_string());
        k += 1;
    }
    if (k..b).any(|j| t[j].p(':') || t[j].p('{')) {
        part.anchored = true;
    }
}

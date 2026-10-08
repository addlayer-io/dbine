//! CQL rules (Cassandra, ScyllaDB, Amazon Keyspaces): the SQL tokens and
//! the SQL rules that apply, plus full scans and batches.

use super::sql::{blocks, leading_wildcard, select_star, str_body, tokens, K, Tok};
use super::Finding;
use dbine_driver::sql::{split_script, unsafe_statements, ScriptDialect, StatementKind};

pub(super) fn lint(script: &str, d: &ScriptDialect, out: &mut Vec<Finding>) {
    for u in unsafe_statements(script, d) {
        out.push(Finding::new("dml-without-where", u.start, u.start + u.keyword.len()).param("keyword", u.keyword));
    }
    let units: Vec<_> = split_script(script, &d.statements()).into_iter().filter(|u| u.kind != StatementKind::ClientCommand).collect();
    let toks: Vec<Vec<Tok>> = units.iter().map(|u| tokens(&u.text, u.start, d)).collect();
    for t in &toks {
        let bs = blocks(t);
        select_star(t, &bs, out);
        for i in 0..t.len() {
            leading_wildcard(t, i, out);
            if t[i].is("allow") && t.get(i + 1).is_some_and(|x| x.is("filtering")) {
                out.push(Finding::new("allow-filtering", t[i].start, t[i + 1].end));
            }
        }
        for b in &bs {
            let Some((from, _)) = b.clause("from") else { continue };
            if b.clause("where").is_some() {
                continue;
            }
            // The system keyspaces are small.
            if t.get(from + 1).is_some_and(|x| x.ident().starts_with("system")) && t.get(from + 2).is_some_and(|x| x.p('.')) {
                continue;
            }
            let table = table_name(t, from + 1);
            out.push(Finding::new("no-partition-key", t[b.select].start, t[from + 1].end.max(t[from].end)).param("table", table));
        }
    }
    batches(&toks, out);
}

/// `ks.table` from `at`, as written.
fn table_name(t: &[Tok], at: usize) -> String {
    let mut s = String::new();
    let mut k = at;
    while let Some(x) = t.get(k).filter(|x| x.named()) {
        s.push_str(x.text);
        if t.get(k + 1).is_some_and(|p| p.p('.')) {
            s.push('.');
            k += 2;
        } else {
            break;
        }
    }
    s
}

/// The statements of a `BEGIN [UNLOGGED|LOGGED|COUNTER] BATCH … APPLY BATCH`
/// that write to more than one table, or to one table with different keys
/// (the first column = value of their WHERE, or the first value of an
/// INSERT): a batch is atomic per partition and costs a coordinator round
/// across them.
fn batches(units: &[Vec<Tok>], out: &mut Vec<Finding>) {
    let mut open: Option<(usize, usize)> = None;
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut reported = false;
    for t in units {
        let mut i = 0;
        if t.first().is_some_and(|x| x.is("begin")) {
            if let Some(b) = (1..t.len().min(4)).find(|&k| t[k].is("batch")) {
                open = Some((t[0].start, t[b].end));
                seen.clear();
                reported = false;
                i = b + 1;
            }
        }
        let Some((bs, be)) = open else { continue };
        let apply = t.iter().position(|x| x.is("apply"));
        let stmt = &t[i..apply.unwrap_or(t.len())];
        if let Some(key) = target(stmt) {
            let differs = seen.iter().any(|s| s.0 != key.0 || (s.1 != key.1 && !s.1.is_empty() && !key.1.is_empty()));
            if differs && !reported {
                reported = true;
                out.push(Finding::new("batch-partitions", bs, be));
            }
            seen.push(key);
        }
        if apply.is_some() {
            open = None;
        }
    }
}

/// (table, partition guess) of an INSERT / UPDATE / DELETE.
fn target(t: &[Tok]) -> Option<(String, String)> {
    let first = t.first()?;
    let table_at = if first.is("insert") {
        t.iter().position(|x| x.is("into"))? + 1
    } else if first.is("update") {
        1
    } else if first.is("delete") {
        t.iter().position(|x| x.is("from"))? + 1
    } else {
        return None;
    };
    let table = table_name(t, table_at).to_lowercase();
    let key = if first.is("insert") {
        let values = t.iter().position(|x| x.is("values"))?;
        let column = t[table_at..values].iter().position(|x| x.p('(')).and_then(|p| t.get(table_at + p + 1)).map(|c| c.ident()).unwrap_or_default();
        t.get(values + 2).map(|v| format!("{column}={}", value(v))).unwrap_or_default()
    } else {
        let w = t.iter().position(|x| x.is("where"))?;
        match (t.get(w + 1), t.get(w + 2), t.get(w + 3)) {
            (Some(c), Some(eq), Some(v)) if eq.p('=') => format!("{}={}", c.ident(), value(v)),
            _ => String::new(),
        }
    };
    Some((table, key))
}

fn value(x: &Tok) -> String {
    if x.k == K::Str { str_body(x.text).to_string() } else { x.text.to_string() }
}

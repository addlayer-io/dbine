//! MongoDB shell rules (MongoDB, FerretDB, DocumentDB).

use super::lex::{closing, tokens, Options, Tok, C};
use super::Finding;

pub(super) fn lint(script: &str, out: &mut Vec<Finding>) {
    let t = tokens(script, Options { regex: true, ..Options::default() });
    for i in 0..t.len() {
        let x = &t[i];
        let method = x.k == C::Word && i > 0 && t[i - 1].p('.') && t.get(i + 1).is_some_and(|n| n.p('('));
        if method {
            let open = i + 1;
            let close = closing(&t, open);
            let empty_filter = empty_first_arg(&t, open, close);
            match x.text {
                "deleteMany" | "remove" | "updateMany" if empty_filter.is_some_and(|all| all || x.text != "updateMany") => {
                    out.push(Finding::new("write-all", x.start, t.get(close).map_or(x.end, |c| c.end)).param("call", call(&t, i, close)));
                }
                "find" if empty_filter.is_some() => {
                    out.push(Finding::new("read-all", x.start, t.get(close).map_or(x.end, |c| c.end)).param("call", call(&t, i, close)));
                }
                _ => {}
            }
        }
        if is_key(&t, i, "$where") {
            out.push(Finding::new("where-operator", x.start, x.end));
        }
        unanchored(&t, i, out);
    }
}

/// `Some(true)` for `({}…)`, `Some(false)` for `()`, `None` with a filter.
fn empty_first_arg(t: &[Tok], open: usize, close: usize) -> Option<bool> {
    if close == open + 1 {
        return Some(false);
    }
    let braces = t.get(open + 1).is_some_and(|x| x.p('{')) && t.get(open + 2).is_some_and(|x| x.p('}'));
    (braces && t.get(open + 3).is_some_and(|x| x.p(',') || x.p(')'))).then_some(true)
}

/// `deleteMany({})` as written, spaces collapsed.
fn call(t: &[Tok], method: usize, close: usize) -> String {
    let end = (method + 4).min(close);
    let mut s: String = t[method..=end.min(t.len() - 1)].iter().map(|x| x.text).collect();
    if end < close {
        s.push_str("…)");
    }
    s
}

/// `key:` as a word or a quoted string (`{ $where: … }`, `{"$where": …}`).
pub(super) fn is_key(t: &[Tok], i: usize, key: &str) -> bool {
    let x = &t[i];
    let named = (x.k == C::Word && x.text == key) || (x.k == C::Str && x.body() == key);
    named && t.get(i + 1).is_some_and(|n| n.p(':'))
}

/// A regex that doesn't start with `^`: `/abc/`, `{ $regex: "abc" }`.
pub(super) fn unanchored(t: &[Tok], i: usize, out: &mut Vec<Finding>) {
    let x = &t[i];
    let pattern = if x.k == C::Regex {
        let body = &x.text[1..];
        body.rfind('/').map(|p| &body[..p]).unwrap_or(body)
    } else if x.k == C::Str && i >= 2 && is_key(t, i - 2, "$regex") {
        x.body()
    } else {
        return;
    };
    if !pattern.starts_with('^') && !pattern.starts_with("\\A") {
        out.push(Finding::new("unanchored-regex", x.start, x.end).param("pattern", x.text));
    }
}

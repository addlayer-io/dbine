//! HTTP console rules: Elasticsearch / OpenSearch and Solr (`METHOD /path`
//! plus a JSON body), and CouchDB (the same lines, and Mango documents).

use super::lex::{closing, tokens, Options, Tok, C};
use super::mongo::{is_key, unanchored};
use super::Finding;

const METHODS: &[&str] = &["get", "post", "put", "delete", "head", "patch"];

/// A request: its method token, its path (rest of the line) and its body.
struct Request<'s, 't> {
    method: &'t Tok<'s>,
    path: &'s str,
    path_end: usize,
    body: &'t [Tok<'s>],
}

fn requests<'s, 't>(script: &'s str, t: &'t [Tok<'s>]) -> Vec<Request<'s, 't>> {
    let starts: Vec<usize> = (0..t.len())
        .filter(|&i| {
            let first_on_line = i == 0 || script[t[i - 1].end..t[i].start].contains('\n');
            first_on_line && t[i].depth == 0 && METHODS.iter().any(|m| t[i].is(m))
        })
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &i)| {
            let line_end = script[t[i].end..].find('\n').map_or(script.len(), |p| t[i].end + p);
            let next = starts.get(n + 1).copied().unwrap_or(t.len());
            let body_from = (i + 1..next).find(|&k| t[k].start >= line_end).unwrap_or(next);
            Request { method: &t[i], path: script[t[i].end..line_end].trim(), path_end: line_end, body: &t[body_from..next] }
        })
        .collect()
}

/// A `*` or `?` that starts a term: `name:*abc`, `*abc`. Not `*:*`, `abc*`.
fn leading_term_wildcard(s: &str) -> bool {
    let b = s.as_bytes();
    (0..b.len()).any(|i| {
        matches!(b[i], b'*' | b'?')
            && (i == 0 || matches!(b[i - 1], b' ' | b':' | b'(' | b'=' | b'+'))
            && b.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

fn keyed<'a>(t: &'a [Tok<'a>], key: &str) -> impl Iterator<Item = usize> + 'a {
    let key = key.to_string();
    (0..t.len()).filter(move |&i| is_key(t, i, &key))
}

pub(super) fn lint_search(script: &str, out: &mut Vec<Finding>) {
    let t = tokens(script, Options { line_start_comments: true, ..Options::default() });
    for r in requests(script, &t) {
        let b = r.body;
        let whole = |rule| Finding::new(rule, r.method.start, r.path_end).param("call", format!("{} {}", r.method.text.to_uppercase(), r.path));
        // ES / OpenSearch: _delete_by_query of everything.
        if r.path.contains("_delete_by_query") && (keyed(b, "query").next().is_none() || keyed(b, "match_all").next().is_some()) {
            out.push(whole("write-all"));
        }
        // Solr: {"delete": {"query": "*:*"}} or <delete><query>*:*</query></delete>.
        let solr_json = keyed(b, "delete").next().is_some() && keyed(b, "query").any(|k| b.get(k + 2).is_some_and(|v| v.k == C::Str && v.body().trim() == "*:*"));
        let solr_xml = b.first().zip(b.last()).is_some_and(|(f, l)| {
            let raw = &script[f.start..l.end];
            raw.contains("<delete>") && raw.contains("*:*")
        });
        if solr_json || solr_xml {
            out.push(whole("write-all"));
        }
        // Leading wildcards: ES wildcard / query_string, Solr q=.
        for w in keyed(b, "wildcard") {
            let Some(open) = (w + 2 < b.len() && b[w + 2].p('{')).then_some(w + 2) else { continue };
            let close = closing(b, open);
            for k in open..close.min(b.len()) {
                let v = &b[k];
                if v.k == C::Str && !b.get(k + 1).is_some_and(|n| n.p(':')) && v.body().starts_with(['*', '?']) {
                    out.push(Finding::new("leading-wildcard", v.start, v.end).param("pattern", v.text));
                }
            }
        }
        for k in keyed(b, "query").chain(keyed(b, "q")) {
            if let Some(v) = b.get(k + 2).filter(|v| v.k == C::Str && leading_term_wildcard(v.body())) {
                out.push(Finding::new("leading-wildcard", v.start, v.end).param("pattern", v.text));
            }
        }
        if let Some(q) = r.path.split(['?', '&']).find(|p| p.starts_with("q=")) {
            if leading_term_wildcard(&q[2..].replace("%20", " ").replace("%3A", ":").replace("%3a", ":")) {
                out.push(Finding::new("leading-wildcard", r.method.start, r.path_end).param("pattern", q));
            }
        }
    }
}

pub(super) fn lint_couchdb(script: &str, out: &mut Vec<Finding>) {
    let t = tokens(script, Options { line_start_comments: true, ..Options::default() });
    for i in 0..t.len() {
        if is_key(&t, i, "selector") && t.get(i + 2).is_some_and(|x| x.p('{')) && t.get(i + 3).is_some_and(|x| x.p('}')) {
            out.push(Finding::new("read-all", t[i].start, t[i + 3].end).param("call", "\"selector\": {}"));
        }
        unanchored(&t, i, out);
    }
}

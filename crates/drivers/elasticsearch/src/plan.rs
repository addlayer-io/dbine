//! Search plans for Elasticsearch / OpenSearch (and the Lucene query
//! parser Solr reuses).
//!
//! - Estimated: `_validate/query?explain=true&rewrite=true` gives the
//!   rewritten Lucene query as text (`+title:star #year:[1970 TO …]`);
//!   [`lucene_tree`] turns it into a tree of clauses.
//! - Actual: the search runs with `"profile": true`; each shard's query
//!   tree, collectors, aggregations and fetch phase become the tree, with
//!   their times.
//! - SQL: Elasticsearch translates it to Query DSL (`_sql/translate`), which
//!   is then planned as a search; OpenSearch gives its own operator tree
//!   (`_plugins/_sql/_explain`).

use crate::json::J;
use dbine_driver::{Plan, PlanNode};

pub fn num(j: Option<&J>) -> Option<f64> {
    match j? {
        J::Num(n) => n.as_f64(),
        J::Str(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn ms(nanos: Option<f64>) -> Option<f64> {
    nanos.map(|n| (n / 1_000.0).round() / 1_000.0)
}

// ---------------------------------------------------------------------------
// Lucene query strings.

/// Top-level clauses of a Lucene query string: split on whitespace outside
/// parentheses, brackets and quotes. `Name [x=y]` stays one clause.
fn clauses(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut quote) = (0i32, false);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quote {
            cur.push(c);
            if c == '\\' && i + 1 < chars.len() {
                cur.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                quote = false;
            }
        } else if c == '"' {
            quote = true;
            cur.push(c);
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
            cur.push(c);
        } else if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            cur.push(c);
        } else if c.is_whitespace() && depth <= 0 {
            // `FieldExistsQuery [field=x]`: a bracket after a bare name.
            let next = chars[i..].iter().find(|c| !c.is_whitespace());
            let name = cur.trim_start_matches(['+', '-', '#']);
            let bare = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
            if next == Some(&'[') && bare {
                cur.push(' ');
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// The index of the `)` closing the `(` at `open`.
fn closing(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quote = false;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '"' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// A Lucene query's `toString()` (what `_validate/query?explain`, Solr's
/// `parsedquery_toString` and the profile's descriptions print) as a tree.
pub fn lucene_tree(s: &str) -> PlanNode {
    let s = s.trim();
    let parts = clauses(s);
    if parts.len() == 1 && !parts[0].starts_with(['+', '-', '#']) {
        return clause(&parts[0]);
    }
    let mut n = PlanNode { op: "BooleanQuery".into(), detail: clip(s, 300), ..Default::default() };
    for p in parts {
        let (occur, body) = match p.chars().next() {
            Some('+') => ("MUST", &p[1..]),
            Some('#') => ("FILTER", &p[1..]),
            Some('-') => ("MUST_NOT", &p[1..]),
            _ => ("SHOULD", p.as_str()),
        };
        let mut c = clause(body);
        c.props.insert(0, ("occur".into(), occur.into()));
        c.detail = format!("{occur} {}", c.detail);
        n.children.push(c);
    }
    n
}

/// One clause (no occur prefix).
fn clause(s: &str) -> PlanNode {
    let s = s.trim();
    let mut n = PlanNode { detail: clip(s, 300), ..Default::default() };
    // (sub query)^boost / (a b)~2
    if s.starts_with('(') {
        if let Some(end) = closing(s, 0) {
            let mut inner = lucene_tree(&s[1..end]);
            let suffix = &s[end + 1..];
            if let Some(b) = suffix.strip_prefix('^') {
                inner.props.push(("boost".into(), b.to_string()));
            } else if let Some(m) = suffix.strip_prefix('~') {
                inner.props.push(("minimum_should_match".into(), m.to_string()));
            }
            return inner;
        }
    }
    if s == "*:*" {
        n.op = "MatchAllDocsQuery".into();
        return n;
    }
    // Name(args): IndexOrDocValuesQuery(…), ConstantScore(…), MatchNoDocsQuery("…")…
    if let Some(open) = s.find('(') {
        let name = &s[..open];
        if !name.is_empty() && !name.contains(':') && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            n.op = name.to_string();
            if let Some(end) = closing(s, open) {
                let inner = &s[open + 1..end];
                n.detail = clip(inner, 300);
                if name == "ConstantScore" || name == "BoostQuery" {
                    n.children.push(lucene_tree(inner));
                }
                if let Some(b) = s[end + 1..].strip_prefix('^') {
                    n.props.push(("boost".into(), b.to_string()));
                }
            }
            if name.contains("Script") {
                n.warnings.push("Script: se evalúa documento por documento".into());
            }
            return n;
        }
    }
    // Name [field=x]
    if let Some((name, rest)) = s.split_once(" [") {
        if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            n.op = name.to_string();
            n.detail = format!("[{rest}");
            return n;
        }
    }
    let Some((field, value)) = split_field(s) else {
        n.op = "Query".into();
        return n;
    };
    n.object = Some(field.to_string());
    let (value, boost) = match value.rsplit_once('^') {
        Some((v, b)) if b.parse::<f64>().is_ok() && !v.ends_with('\\') => (v, Some(b)),
        _ => (value, None),
    };
    if let Some(b) = boost {
        n.props.push(("boost".into(), b.to_string()));
    }
    n.op = if value.starts_with('[') || value.starts_with('{') {
        "RangeQuery"
    } else if value.starts_with('"') {
        "PhraseQuery"
    } else if value.starts_with('/') {
        "RegexpQuery"
    } else if value.contains('*') || value.contains('?') {
        if value.starts_with('*') || value.starts_with('?') {
            n.warnings.push("Comodín inicial: recorre todo el diccionario de términos".into());
        }
        "WildcardQuery"
    } else if value.rsplit_once('~').is_some_and(|(_, d)| d.parse::<f64>().is_ok()) {
        "FuzzyQuery"
    } else {
        "TermQuery"
    }
    .into();
    n
}

/// `field:value`, the first unescaped `:` outside quotes and brackets.
fn split_field(s: &str) -> Option<(&str, &str)> {
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        if matches!(c, '"' | '[' | '{' | '(') {
            return None;
        }
        if c == ':' && prev != '\\' && i > 0 {
            return Some((&s[..i], &s[i + 1..]));
        }
        prev = c;
    }
    None
}

// ---------------------------------------------------------------------------
// Elasticsearch / OpenSearch searches.

/// Aggregations of a search body, as nodes (sub-aggregations as children).
pub fn aggs_nodes(aggs: &J) -> Vec<PlanNode> {
    let mut out = Vec::new();
    for (name, spec) in aggs.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
        let mut n = PlanNode { detail: name.clone(), ..Default::default() };
        for (k, v) in spec.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
            match k.as_str() {
                "aggs" | "aggregations" => n.children.extend(aggs_nodes(v)),
                "meta" => {}
                _ => {
                    n.op = k.clone();
                    n.object = v.get("field").map(J::text);
                    for (pk, pv) in v.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
                        n.props.push((pk.clone(), clip(&pv.text(), 500)));
                    }
                }
            }
        }
        out.push(n);
    }
    out
}

/// Body keys other than the query and the aggregations, for the root's props.
fn body_props(n: &mut PlanNode, body: Option<&J>) {
    for (k, v) in body.and_then(J::as_obj).map(Vec::as_slice).unwrap_or(&[]) {
        if !matches!(k.as_str(), "query" | "aggs" | "aggregations" | "profile") {
            n.props.push((k.clone(), clip(&v.text(), 500)));
        }
    }
}

/// Estimated plan of a search on `target` (the index expression) from the
/// `_validate/query?explain&rewrite` reply.
pub fn estimated_search(statement: &str, target: &str, body: Option<&J>, validate: &J) -> Plan {
    let mut root = PlanNode { op: "Search".into(), object: Some(target.to_string()), ..Default::default() };
    body_props(&mut root, body);
    let expl = validate.get("explanations").and_then(J::as_arr).unwrap_or(&[]);
    let per_index = expl.len() > 1;
    for e in expl {
        let index = e.get("index").map(J::text).unwrap_or_default();
        let mut q = match (e.get("explanation").and_then(J::as_str), e.get("error")) {
            (Some(x), _) => lucene_tree(x),
            (None, err) => PlanNode {
                op: "Query".into(),
                warnings: vec![format!("Consulta inválida: {}", err.map(J::text).unwrap_or_default())],
                ..Default::default()
            },
        };
        if per_index {
            q = PlanNode { op: "Index".into(), object: Some(index), children: vec![q], ..Default::default() };
        }
        root.children.push(q);
    }
    if validate.get("valid").and_then(J::as_bool) == Some(false) && expl.is_empty() {
        root.warnings.push("La consulta no es válida".into());
    }
    let aggs = body.and_then(|b| b.get("aggs").or_else(|| b.get("aggregations")));
    if let Some(a) = aggs {
        root.children.push(PlanNode { op: "Aggregations".into(), children: aggs_nodes(a), ..Default::default() });
    }
    Plan { statement: statement.to_string(), root, actual: false, raw_format: "json".into(), raw: validate.pretty() }
}

/// A profile entry (query, aggregation or fetch phase) and its children.
fn profile_node(p: &J, name_key: &str, detail_key: &str) -> PlanNode {
    let desc = p.get(detail_key).map(J::text).unwrap_or_default();
    let mut n = PlanNode {
        op: p.get(name_key).map(J::text).unwrap_or_else(|| "?".into()),
        detail: clip(&desc, 300),
        actual_ms: ms(num(p.get("time_in_nanos"))),
        ..Default::default()
    };
    if name_key == "type" && detail_key == "description" && !desc.is_empty() {
        // A query: its own Lucene text tells the field.
        let own = lucene_tree(&desc);
        if own.children.is_empty() {
            n.object = own.object;
            n.warnings = own.warnings;
        }
    }
    if n.op.contains("Script") && !n.warnings.iter().any(|w| w.starts_with("Script")) {
        n.warnings.push("Script: se evalúa documento por documento".into());
    }
    for key in ["breakdown", "debug"] {
        for (k, v) in p.get(key).and_then(J::as_obj).map(Vec::as_slice).unwrap_or(&[]) {
            if !matches!(v, J::Num(x) if x.as_f64() == Some(0.0)) {
                n.props.push((k.clone(), clip(&v.text(), 500)));
            }
        }
    }
    if let Some(r) = p.get("reason") {
        n.props.push(("reason".into(), r.text()));
    }
    for c in p.get("children").and_then(J::as_arr).unwrap_or(&[]) {
        n.children.push(profile_node(c, name_key, detail_key));
    }
    n
}

/// Actual plan of a search that ran with `"profile": true`.
pub fn profiled_search(statement: &str, target: &str, body: Option<&J>, resp: &J) -> Plan {
    let mut root = PlanNode { op: "Search".into(), object: Some(target.to_string()), ..Default::default() };
    root.actual_ms = num(resp.get("took"));
    root.actual_rows = num(resp.at(&["hits", "total", "value"])).or_else(|| num(resp.at(&["hits", "total"])));
    if let Some(r) = resp.at(&["hits", "total", "relation"]).and_then(J::as_str).filter(|r| *r != "eq") {
        root.props.push(("hits.total.relation".into(), r.to_string()));
    }
    for (k, path) in [("shards", &["_shards", "total"][..]), ("timed_out", &["timed_out"][..])] {
        if let Some(v) = resp.at(path) {
            root.props.push((k.into(), v.text()));
        }
    }
    body_props(&mut root, body);
    if resp.get("timed_out").and_then(J::as_bool) == Some(true) {
        root.warnings.push("La búsqueda superó el tiempo límite".into());
    }
    if num(resp.at(&["_shards", "failed"])).unwrap_or(0.0) > 0.0 {
        root.warnings.push("Fallaron algunos shards".into());
    }
    for sh in resp.at(&["profile", "shards"]).and_then(J::as_arr).unwrap_or(&[]) {
        let id = sh.get("id").map(J::text).unwrap_or_default();
        let index = sh.get("index").map(J::text);
        // `[node][index][shard]` when the shard doesn't say (OpenSearch).
        let parts: Vec<&str> = id.trim_matches(['[', ']']).split("][").collect();
        let object = match (&index, num(sh.get("shard_id"))) {
            (Some(i), Some(n)) => format!("{i}[{n}]"),
            _ if parts.len() == 3 => format!("{}[{}]", parts[1], parts[2]),
            _ => id.clone(),
        };
        let mut shard = PlanNode { op: "Shard".into(), object: Some(object), detail: id, ..Default::default() };
        if let Some(node) = sh.get("node_id") {
            shard.props.push(("node_id".into(), node.text()));
        }
        for s in sh.get("searches").and_then(J::as_arr).unwrap_or(&[]) {
            if let Some(r) = ms(num(s.get("rewrite_time"))) {
                shard.props.push(("rewrite_time (ms)".into(), r.to_string()));
            }
            let queries: Vec<PlanNode> =
                s.get("query").and_then(J::as_arr).unwrap_or(&[]).iter().map(|q| profile_node(q, "type", "description")).collect();
            let mut collectors: Vec<PlanNode> =
                s.get("collector").and_then(J::as_arr).unwrap_or(&[]).iter().map(|c| profile_node(c, "name", "reason")).collect();
            // The collectors consume what the query matches.
            match collectors.first_mut() {
                Some(c) => c.children.extend(queries),
                None => collectors = queries,
            }
            shard.children.extend(collectors);
        }
        let aggs = sh.get("aggregations").and_then(J::as_arr).unwrap_or(&[]);
        if !aggs.is_empty() {
            let children: Vec<PlanNode> = aggs.iter().map(|a| profile_node(a, "type", "description")).collect();
            let total: f64 = children.iter().filter_map(|c| c.actual_ms).sum();
            let actual_ms = Some((total * 1000.0).round() / 1000.0);
            shard.children.push(PlanNode { op: "Aggregations".into(), actual_ms, children, ..Default::default() });
        }
        if let Some(f) = sh.get("fetch") {
            let mut fetch = profile_node(f, "type", "description");
            fetch.op = "Fetch".into();
            shard.children.push(fetch);
        }
        let total: f64 = shard.children.iter().filter_map(|c| c.actual_ms).sum();
        shard.actual_ms = Some((total * 1000.0).round() / 1000.0);
        root.children.push(shard);
    }
    let raw = resp.get("profile").map(|p| J::Obj(vec![("profile".into(), p.clone())])).unwrap_or(J::Null);
    Plan { statement: statement.to_string(), root, actual: true, raw_format: "json".into(), raw: raw.pretty() }
}

/// Elasticsearch SQL: a node for the statement, with the translated DSL as
/// props, over the search plan of that DSL (when there is one).
pub fn sql_plan(statement: &str, dsl: &J, search: Option<Plan>) -> Plan {
    let mut root = PlanNode { op: "SQL".into(), detail: clip(statement, 300), ..Default::default() };
    for (k, v) in dsl.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
        root.props.push((k.clone(), clip(&v.text(), 2000)));
    }
    let (actual, raw) = match search {
        Some(p) => {
            root.actual_ms = p.root.actual_ms;
            root.actual_rows = p.root.actual_rows;
            root.children.push(p.root);
            (p.actual, format!("{{\n\"translate\": {},\n\"plan\": {}\n}}", dsl.pretty(), if p.raw.is_empty() { "null" } else { &p.raw }))
        }
        None => (false, dsl.pretty()),
    };
    Plan { statement: statement.to_string(), root, actual, raw_format: "json".into(), raw }
}

/// `sourceBuilder={…}` and `indexName=…` of an OpenSearch index scan.
pub fn os_scan_request(request: &str) -> Option<(String, J)> {
    let index = request.split("indexName=").nth(1)?.split([',', ')']).next()?.trim().to_string();
    let start = request.find("sourceBuilder=")? + "sourceBuilder=".len();
    let s = &request[start..];
    let mut depth = 0;
    let mut quote = false;
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        match c {
            '"' if prev != '\\' => quote = !quote,
            '{' if !quote => depth += 1,
            '}' if !quote => {
                depth -= 1;
                if depth == 0 {
                    return J::parse(&s[..=i]).ok().map(|j| (index, j));
                }
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// OpenSearch SQL `_explain` (`root: {name, description, children}`) as a
/// tree. `scan` adds, under each index scan, the plan of its pushed-down
/// search (keyed by index name).
pub fn os_sql_plan(statement: &str, explain: &J, scan: &mut dyn FnMut(&str, &J) -> Option<PlanNode>) -> Plan {
    fn walk(j: &J, scan: &mut dyn FnMut(&str, &J) -> Option<PlanNode>) -> PlanNode {
        let mut n = PlanNode { op: j.get("name").map(J::text).unwrap_or_else(|| "?".into()), ..Default::default() };
        for (k, v) in j.get("description").and_then(J::as_obj).map(Vec::as_slice).unwrap_or(&[]) {
            let t = v.text();
            if k == "request" {
                if let Some((index, dsl)) = os_scan_request(&t) {
                    n.object = Some(index.clone());
                    for (dk, dv) in dsl.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
                        n.props.push((dk.clone(), clip(&dv.text(), 2000)));
                    }
                    if let Some(sub) = scan(&index, &dsl) {
                        n.actual_ms = sub.actual_ms;
                        n.actual_rows = sub.actual_rows;
                        n.children.extend(sub.children);
                    }
                    continue;
                }
            }
            if n.detail.is_empty() {
                n.detail = clip(&t, 300);
            }
            n.props.push((k.clone(), clip(&t, 2000)));
        }
        for c in j.get("children").and_then(J::as_arr).unwrap_or(&[]) {
            n.children.push(walk(c, scan));
        }
        n
    }
    let root = match explain.get("root") {
        Some(r) => walk(r, scan),
        // Legacy engine: the DSL itself.
        None => {
            let mut n = PlanNode { op: "SQL".into(), detail: clip(statement, 300), ..Default::default() };
            for (k, v) in explain.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
                n.props.push((k.clone(), clip(&v.text(), 2000)));
            }
            n
        }
    };
    Plan { statement: statement.to_string(), root, actual: false, raw_format: "json".into(), raw: explain.pretty() }
}

/// The index a SQL statement reads (`… FROM <name> …`).
pub fn sql_from(stmt: &str) -> Option<String> {
    let mut words = stmt.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '(' || c == ')');
    while let Some(w) = words.next() {
        if w.eq_ignore_ascii_case("from") {
            let name = words.find(|w| !w.is_empty())?;
            let name = name.trim_matches(|c| c == '"' || c == '`' || c == '\'');
            return (!name.is_empty()).then(|| name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lucene_boolean_tree() {
        let t = lucene_tree(
            "+(title:star title:wars) -title:*x #IndexOrDocValuesQuery(indexQuery=year:[1970 TO 2147483647], dvQuery=year:[1970 TO 2147483647]) #genre:scifi",
        );
        assert_eq!(t.op, "BooleanQuery");
        assert_eq!(t.children.len(), 4);
        let should = &t.children[0];
        assert_eq!(should.op, "BooleanQuery");
        assert_eq!(should.props[0], ("occur".into(), "MUST".into()));
        assert_eq!(should.children[0].op, "TermQuery");
        assert_eq!(should.children[0].object.as_deref(), Some("title"));
        assert_eq!(should.children[0].detail, "SHOULD title:star");
        let wild = &t.children[1];
        assert_eq!(wild.op, "WildcardQuery");
        assert_eq!(wild.detail, "MUST_NOT title:*x");
        assert!(wild.warnings[0].starts_with("Comodín inicial"));
        assert_eq!(t.children[2].op, "IndexOrDocValuesQuery");
        assert_eq!(t.children[3].op, "TermQuery");
        assert_eq!(t.children[3].props[0].1, "FILTER");
    }

    #[test]
    fn lucene_leaves() {
        assert_eq!(lucene_tree("*:*").op, "MatchAllDocsQuery");
        assert_eq!(lucene_tree("year:[1970 TO *]").op, "RangeQuery");
        assert_eq!(lucene_tree("title:\"star wars\"").op, "PhraseQuery");
        assert_eq!(lucene_tree("title:star~2").op, "FuzzyQuery");
        let b = lucene_tree("title:star^2.0");
        assert_eq!(b.op, "TermQuery");
        assert_eq!(b.props[0], ("boost".into(), "2.0".into()));
        let e = lucene_tree("+FieldExistsQuery [field=genre] +title:x");
        assert_eq!(e.children[0].op, "FieldExistsQuery");
        let c = lucene_tree("ConstantScore(title:x)^3.0");
        assert_eq!(c.children[0].op, "TermQuery");
        let m = lucene_tree("(a:1 b:2)~2");
        assert_eq!(m.op, "BooleanQuery");
        assert!(m.props.contains(&("minimum_should_match".into(), "2".into())));
    }

    #[test]
    fn estimated_from_validate() {
        let v = J::parse(r#"{"_shards":{"total":1,"successful":1,"failed":0},"valid":true,"explanations":[{"index":"films","valid":true,"explanation":"+title:star #genre:scifi"}]}"#).unwrap();
        let body = J::parse(r#"{"size":5,"query":{},"aggs":{"g":{"terms":{"field":"genre"},"aggs":{"y":{"avg":{"field":"year"}}}}}}"#).unwrap();
        let p = estimated_search("GET /films/_search", "films", Some(&body), &v);
        assert!(!p.actual);
        assert_eq!(p.root.op, "Search");
        assert_eq!(p.root.props, vec![("size".to_string(), "5".to_string())]);
        assert_eq!(p.root.children[0].op, "BooleanQuery");
        let aggs = &p.root.children[1];
        assert_eq!(aggs.children[0].op, "terms");
        assert_eq!(aggs.children[0].object.as_deref(), Some("genre"));
        assert_eq!(aggs.children[0].children[0].op, "avg");
    }

    const PROFILE: &str = r#"{"took":12,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},
      "hits":{"total":{"value":2,"relation":"eq"},"max_score":1.2,"hits":[]},
      "profile":{"shards":[{"id":"[n1][films][0]","node_id":"n1","shard_id":0,"index":"films","cluster":"(local)",
        "searches":[{"query":[{"type":"BooleanQuery","description":"+title:star #year:[1970 TO 2147483647]","time_in_nanos":2000000,
            "breakdown":{"score":1000,"score_count":2,"next_doc":0},
            "children":[{"type":"TermQuery","description":"title:star","time_in_nanos":500000,"breakdown":{}},
                        {"type":"PointRangeQuery","description":"year:[1970 TO 2147483647]","time_in_nanos":300000,"breakdown":{}}]}],
          "rewrite_time":15000,
          "collector":[{"name":"QueryPhaseCollector","reason":"search_query_phase","time_in_nanos":900000,
            "children":[{"name":"SimpleTopScoreDocCollector","reason":"search_top_hits","time_in_nanos":250000}]}]}],
        "aggregations":[{"type":"GlobalOrdinalsStringTermsAggregator","description":"g","time_in_nanos":1500000,"breakdown":{},"debug":{"total_buckets":2}}],
        "fetch":{"type":"fetch","description":"","time_in_nanos":3000000,"breakdown":{},
          "children":[{"type":"FetchSourcePhase","description":"","time_in_nanos":20000,"breakdown":{}}]}}]}}"#;

    #[test]
    fn profile_tree() {
        let r = J::parse(PROFILE).unwrap();
        let p = profiled_search("GET /films/_search", "films", None, &r);
        assert!(p.actual);
        assert_eq!(p.root.actual_ms, Some(12.0));
        assert_eq!(p.root.actual_rows, Some(2.0));
        let shard = &p.root.children[0];
        assert_eq!(shard.object.as_deref(), Some("films[0]"));
        let coll = &shard.children[0];
        assert_eq!(coll.op, "QueryPhaseCollector");
        assert_eq!(coll.actual_ms, Some(0.9));
        assert_eq!(coll.children[0].op, "SimpleTopScoreDocCollector");
        let q = &coll.children[1];
        assert_eq!(q.op, "BooleanQuery");
        assert_eq!(q.actual_ms, Some(2.0));
        assert!(q.props.contains(&("score_count".into(), "2".into())));
        assert!(!q.props.iter().any(|(k, _)| k == "next_doc"));
        assert_eq!(q.children[1].object.as_deref(), Some("year"));
        assert_eq!(shard.children[1].op, "Aggregations");
        assert_eq!(shard.children[1].children[0].props[0], ("total_buckets".into(), "2".into()));
        assert_eq!(shard.children[2].op, "Fetch");
        assert!(p.raw.starts_with("{\n  \"profile\""));
    }

    #[test]
    fn opensearch_sql_explain() {
        let e = J::parse(r#"{"root":{"name":"ProjectOperator","description":{"fields":"[title]"},"children":[{"name":"OpenSearchIndexScan","description":{"request":"OpenSearchQueryRequest(indexName=films, sourceBuilder={\"from\":0,\"size\":5,\"query\":{\"range\":{\"year\":{\"from\":1970}}},\"sort\":[{\"year\":{\"order\":\"asc\"}}]}, needClean=true, searchDone=false)"},"children":[]}]}}"#).unwrap();
        let mut seen = Vec::new();
        let p = os_sql_plan("SELECT …", &e, &mut |i, dsl| {
            seen.push((i.to_string(), dsl.get("size").map(J::text)));
            Some(PlanNode { children: vec![PlanNode { op: "RangeQuery".into(), ..Default::default() }], ..Default::default() })
        });
        assert_eq!(p.root.op, "ProjectOperator");
        assert_eq!(p.root.detail, "[title]");
        let scan = &p.root.children[0];
        assert_eq!(scan.object.as_deref(), Some("films"));
        assert!(scan.props.iter().any(|(k, _)| k == "sort"));
        assert_eq!(scan.children[0].op, "RangeQuery");
        assert_eq!(seen, vec![("films".to_string(), Some("5".to_string()))]);
    }

    #[test]
    fn sql_from_finds_the_index() {
        assert_eq!(sql_from("SELECT a FROM \"films\" WHERE x"), Some("films".into()));
        assert_eq!(sql_from("SHOW TABLES"), None);
    }
}

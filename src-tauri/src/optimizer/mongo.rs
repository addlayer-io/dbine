//! MongoDB: `$where` (JavaScript run for every document, no index) turned
//! into query operators when the expression is simple comparisons of
//! fields with constants joined by `&&`. JavaScript compares across types
//! and doesn't look into arrays the way query operators do, so the
//! candidate asks to be compared before it's trusted (`verify`).

use super::{Candidate, Source};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub fn where_to_operators(src: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = src[from..].find("$where") {
        let at = from + p;
        from = at + 6;
        let Some((pair_start, pair_end, js)) = where_pair(src, at) else { continue };
        let Some(filter) = translate(&js) else { continue };
        let quoted_keys = src[pair_start..at].ends_with(['"', '\'']);
        let text = filter
            .iter()
            .map(|(k, v)| {
                let simple = k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit());
                let key = if simple && !quoted_keys { k.clone() } else { Value::String(k.clone()).to_string() };
                format!("{key}: {}", mongo_value(v))
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = src.to_string();
        sql.replace_range(pair_start..pair_end, &text);
        out.push(Candidate {
            id: format!("rule-mongo_where-{}", out.len()),
            source: Source::Rule,
            rule: Some("mongo_where".into()),
            params: BTreeMap::from([("expr".to_string(), js.chars().take(120).collect())]),
            title: None,
            explanation: None,
            sql,
            verify: true,
        });
    }
    out
}

/// `{ "$gt": 30 }` as mongosh writes it (keys unquoted).
fn mongo_value(v: &Value) -> String {
    match v {
        Value::Object(m) => format!("{{ {} }}", m.iter().map(|(k, v)| format!("{k}: {}", mongo_value(v))).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// The `$where: "…"` pair starting with the key at `at` (its quote
/// included): where it starts and ends, and the JavaScript.
fn where_pair(src: &str, at: usize) -> Option<(usize, usize, String)> {
    let b = src.as_bytes();
    let mut start = at;
    let mut i = at + 6;
    if at > 0 && matches!(b[at - 1], b'"' | b'\'') {
        if b.get(i) != Some(&b[at - 1]) {
            return None;
        }
        start = at - 1;
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let q = *b.get(i)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let mut js = String::new();
    let mut j = i + 1;
    let chars: Vec<(usize, char)> = src[j..].char_indices().map(|(o, c)| (j + o, c)).collect();
    let mut k = 0;
    loop {
        let &(pos, c) = chars.get(k)?;
        if c == '\\' {
            let &(_, n) = chars.get(k + 1)?;
            js.push(n);
            k += 2;
            continue;
        }
        if c as u32 == q as u32 {
            j = pos + 1;
            break;
        }
        js.push(c);
        k += 1;
    }
    Some((start, j, js))
}

/// `this.a > 1 && this.b == 'x'` (or a function returning it) as a filter.
pub fn translate(js: &str) -> Option<Map<String, Value>> {
    let mut e = js.trim();
    if let Some(body) = e.strip_prefix("function") {
        let body = body.trim_start().strip_prefix("()")?.trim();
        let body = body.strip_prefix('{')?.strip_suffix('}')?.trim();
        let body = body.strip_prefix("return")?.trim();
        e = body.strip_suffix(';').unwrap_or(body).trim();
    }
    if e.contains("||") || e.is_empty() {
        return None;
    }
    let mut filter = Map::new();
    for part in e.split("&&") {
        let mut p = part.trim();
        while p.starts_with('(') && p.ends_with(')') {
            p = p[1..p.len() - 1].trim();
        }
        let (path, op, value) = comparison(p)?;
        let mongo_op = match op {
            "==" | "===" => None,
            "!=" | "!==" => Some("$ne"),
            ">" => Some("$gt"),
            ">=" => Some("$gte"),
            "<" => Some("$lt"),
            "<=" => Some("$lte"),
            _ => return None,
        };
        // `=== null` doesn't match a missing field, `{f: null}` does.
        if value.is_null() && (op == "===" || op == "!==") {
            return None;
        }
        // Order across types isn't the same in JavaScript and in queries.
        if mongo_op.is_some_and(|o| o != "$ne") && !value.is_number() && !value.is_string() {
            return None;
        }
        match (filter.get_mut(&path), mongo_op) {
            (None, None) => {
                filter.insert(path, value);
            }
            (None, Some(o)) => {
                filter.insert(path, Value::Object(Map::from_iter([(o.to_string(), value)])));
            }
            (Some(Value::Object(m)), Some(o)) if !m.contains_key(o) && m.keys().all(|k| k.starts_with('$')) => {
                m.insert(o.to_string(), value);
            }
            (Some(Value::Object(m)), None) if m.keys().all(|k| k.starts_with('$')) && !m.contains_key("$eq") => {
                m.insert("$eq".into(), value);
            }
            (Some(existing), Some(o)) if !existing.is_object() => {
                let eq = existing.take();
                *existing = Value::Object(Map::from_iter([("$eq".to_string(), eq), (o.to_string(), value)]));
            }
            _ => return None,
        }
    }
    Some(filter)
}

/// `this.f OP literal` or `literal OP this.f`.
fn comparison(p: &str) -> Option<(String, &'static str, Value)> {
    const OPS: &[(&str, &str)] = &[("===", "==="), ("!==", "!=="), ("==", "=="), ("!=", "!="), (">=", ">="), ("<=", "<="), (">", ">"), ("<", "<")];
    let (pos, op) = OPS.iter().filter_map(|(o, canon)| p.find(o).map(|i| (i, (*o, *canon)))).min_by_key(|(i, (o, _))| (*i, std::cmp::Reverse(o.len())))?;
    let (l, r) = (p[..pos].trim(), p[pos + op.0.len()..].trim());
    let flip = |o: &'static str| match o {
        ">" => "<",
        "<" => ">",
        ">=" => "<=",
        "<=" => ">=",
        x => x,
    };
    if let Some(path) = field(l) {
        return Some((path, op.1, literal(r)?));
    }
    let path = field(r)?;
    Some((path, flip(op.1), literal(l)?))
}

fn field(s: &str) -> Option<String> {
    let path = s.strip_prefix("this.").or_else(|| s.strip_prefix("obj."))?;
    // `this.a.length` is JavaScript's, not a field.
    let ok = !path.is_empty() && path.split('.').all(|p| !p.is_empty() && p != "length" && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$'));
    ok.then(|| path.to_string())
}

fn literal(s: &str) -> Option<Value> {
    match s {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        "null" => return Some(Value::Null),
        _ => {}
    }
    if let Some(q) = s.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let inner = s.strip_prefix(q)?.strip_suffix(q)?;
        if inner.contains(q) || inner.contains('\\') {
            return None;
        }
        return Some(Value::String(inner.to_string()));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::from(i));
    }
    let f: f64 = s.parse().ok()?;
    serde_json::Number::from_f64(f).map(Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(s: &str) -> Vec<String> {
        where_to_operators(s).into_iter().map(|c| c.sql).collect()
    }

    #[test]
    fn simple_where_becomes_operators() {
        assert_eq!(rewrite(r#"db.people.find({ $where: "this.age > 30" })"#), vec![r#"db.people.find({ age: { $gt: 30 } })"#]);
        assert_eq!(
            rewrite(r#"db.people.find({ city: "X", $where: "this.age >= 18 && this.age < 65 && this.name == 'Ana'" }).limit(5)"#),
            vec![r#"db.people.find({ city: "X", age: { $gte: 18, $lt: 65 }, name: "Ana" }).limit(5)"#]
        );
        assert_eq!(rewrite(r#"{"find": "p", "filter": {"$where": "function() { return 10 < this.a.b; }"}}"#), vec![r#"{"find": "p", "filter": {"a.b": { $gt: 10 }}}"#]);
        assert_eq!(rewrite("db.p.find({$where: 'this.x != null'})"), vec!["db.p.find({x: { $ne: null }})"]);
        assert!(where_to_operators("db.p.find({$where: 'this.x == 1'})")[0].verify);
    }

    #[test]
    fn complex_where_is_left_alone() {
        for js in [
            "this.a > 1 || this.b < 2",
            "this.a.length > 3",
            "this.a > this.b",
            "Math.abs(this.a) > 1",
            "this.a === null",
            "this.a > true",
            "this.a == 1 && this.a == 2",
            "function(x) { return this.a > 1; }",
        ] {
            assert!(rewrite(&format!("db.p.find({{$where: \"{js}\"}})")).is_empty(), "{js}");
        }
        assert!(rewrite("db.p.find({a: 1})").is_empty());
    }
}

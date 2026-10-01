//! A command's reply as a table. The shape depends on the command:
//! `HGETALL` gives `field, value` pairs, `ZRANGE … WITHSCORES` gives
//! `member, score`, `XRANGE` one row per entry with its fields as columns,
//! `INFO` its `section, field, value` lines, an array one row per element
//! in a `value` column, and a scalar a single `result` (`value` for the
//! `GET` family).

use dbine_driver::{json_bytes, json_f64, json_i64};
use redis::Value;
use serde_json::Value as J;

#[derive(Debug, Default)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<J>>,
    /// A note for the messages panel (a SCAN's next cursor).
    pub message: Option<String>,
}

impl Table {
    fn new(columns: &[&'static str]) -> Self {
        Self { columns: columns.iter().map(|c| c.to_string()).collect(), ..Default::default() }
    }

    #[cfg(test)]
    fn column_names(&self) -> Vec<String> {
        self.columns.clone()
    }
}

const WITH_SCORES: &[&str] = &[
    "ZRANGE", "ZRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGE", "ZREVRANGEBYSCORE", "ZREVRANGEBYLEX", "ZRANDMEMBER",
    "ZUNION", "ZINTER", "ZDIFF", "ZPOPMIN", "ZPOPMAX",
];

const VALUE_SCALARS: &[&str] = &["GET", "GETEX", "GETDEL", "GETSET", "GETRANGE", "JSON.GET", "HGET", "LINDEX"];

pub fn shape(args: &[Vec<u8>], v: Value) -> Table {
    let name = args.first().map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
    let has_arg = |w: &str| args.iter().skip(1).any(|a| a.eq_ignore_ascii_case(w.as_bytes()));
    let v = unwrap_attribute(v);

    match (name.as_str(), &v) {
        ("INFO", Value::BulkString(_) | Value::VerbatimString { .. }) => return info_table(&text_of(&v)),
        ("XRANGE" | "XREVRANGE", Value::Array(_)) => return stream_table(v),
        ("SCAN" | "SSCAN" | "HSCAN" | "ZSCAN", Value::Array(items)) if items.len() == 2 => {
            let cursor = text_of(&items[0]);
            let Value::Array(items) = v else { unreachable!() };
            let mut t = match name.as_str() {
                "HSCAN" => pairs(&["field", "value"], flat(items.into_iter().nth(1)), false),
                "ZSCAN" => pairs(&["member", "score"], flat(items.into_iter().nth(1)), true),
                _ => values(flat(items.into_iter().nth(1))),
            };
            t.message = Some(format!("{name}: siguiente cursor {cursor}{}", if cursor == "0" { " (fin)" } else { "" }));
            return t;
        }
        _ => {}
    }

    let score_pairs = WITH_SCORES.contains(&name.as_str()) && has_arg("WITHSCORES");
    match v {
        Value::Map(entries) => {
            let mut t = Table::new(if score_pairs { &["member", "score"] } else { &["field", "value"] });
            for (k, val) in entries {
                let val = if score_pairs { score(&val) } else { cell(&val) };
                t.rows.push(vec![cell(&k), val]);
            }
            t
        }
        Value::Array(items) | Value::Set(items) => {
            if score_pairs && items.iter().all(|i| matches!(i, Value::Array(p) if p.len() == 2)) && !items.is_empty() {
                // RESP3 replies nest each pair.
                let flat = items.into_iter().flat_map(|i| if let Value::Array(p) = i { p } else { vec![] }).collect();
                return pairs(&["member", "score"], flat, true);
            }
            if score_pairs {
                return pairs(&["member", "score"], items, true);
            }
            if name == "HGETALL"
                || (name == "CONFIG" && args.get(1).is_some_and(|s| s.eq_ignore_ascii_case(b"GET")))
                || (name == "HRANDFIELD" && has_arg("WITHVALUES"))
            {
                return pairs(&["field", "value"], items, false);
            }
            values(items)
        }
        Value::Nil if VALUE_SCALARS.contains(&name.as_str()) => scalar("value", J::Null),
        other => scalar(if VALUE_SCALARS.contains(&name.as_str()) { "value" } else { "result" }, cell(&other)),
    }
}

fn unwrap_attribute(v: Value) -> Value {
    match v {
        Value::Attribute { data, .. } => unwrap_attribute(*data),
        v => v,
    }
}

fn flat(v: Option<Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) | Some(Value::Set(a)) => a,
        Some(Value::Map(m)) => m.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        Some(Value::Nil) | None => Vec::new(),
        Some(other) => vec![other],
    }
}

fn scalar(column: &'static str, v: J) -> Table {
    let mut t = Table::new(&[column]);
    t.rows.push(vec![v]);
    t
}

fn values(items: Vec<Value>) -> Table {
    let mut t = Table::new(&["value"]);
    t.rows = items.iter().map(|i| vec![cell(i)]).collect();
    t
}

fn pairs(columns: &[&'static str], items: Vec<Value>, scored: bool) -> Table {
    let mut t = Table::new(columns);
    let mut it = items.into_iter();
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or(Value::Nil);
        t.rows.push(vec![cell(&k), if scored { score(&v) } else { cell(&v) }]);
    }
    t
}

fn score(v: &Value) -> J {
    match v {
        Value::Double(f) => json_f64(*f),
        Value::Int(i) => json_i64(*i),
        other => {
            let s = text_of(other);
            s.parse::<f64>().map_or_else(|_| s.into(), json_f64)
        }
    }
}

/// `XRANGE`: `id` plus every field seen, in order of appearance.
fn stream_table(v: Value) -> Table {
    let Value::Array(entries) = v else { return Table::default() };
    let mut names: Vec<String> = Vec::new();
    let mut parsed = Vec::with_capacity(entries.len());
    for e in entries {
        let mut parts = flat(Some(e)).into_iter();
        let id = parts.next().map(|i| text_of(&i)).unwrap_or_default();
        let mut fields = Vec::new();
        let mut kv = flat(parts.next()).into_iter();
        while let Some(k) = kv.next() {
            let k = text_of(&k);
            if !names.contains(&k) {
                names.push(k.clone());
            }
            fields.push((k, cell(&kv.next().unwrap_or(Value::Nil))));
        }
        parsed.push((id, fields));
    }
    let mut t = Table { columns: std::iter::once("id".to_string()).chain(names.clone()).collect(), ..Default::default() };
    for (id, mut fields) in parsed {
        let mut row = vec![J::from(id)];
        for n in &names {
            let pos = fields.iter().position(|(k, _)| k == n);
            row.push(pos.map_or(J::Null, |p| fields.swap_remove(p).1));
        }
        t.rows.push(row);
    }
    t
}

/// `INFO`'s `# Section` headers and `field:value` lines.
fn info_table(text: &str) -> Table {
    let mut t = Table::new(&["section", "field", "value"]);
    let mut section = String::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(s) = line.strip_prefix('#') {
            section = s.trim().to_string();
        } else if let Some((k, v)) = line.split_once(':') {
            t.rows.push(vec![section.clone().into(), k.into(), v.into()]);
        }
    }
    t
}

/// A value as text (for ids, cursors and field names).
pub fn text_of(v: &Value) -> String {
    match v {
        Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        Value::SimpleString(s) => s.clone(),
        Value::VerbatimString { text, .. } => text.clone(),
        Value::Okay => "OK".into(),
        Value::Int(i) => i.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => String::new(),
        other => to_json(other).to_string(),
    }
}

/// A reply as a result cell: strings as text (binary as `0x…`), numbers
/// as numbers, anything nested as compact JSON.
pub fn cell(v: &Value) -> J {
    match v {
        Value::Nil => J::Null,
        Value::Int(i) => json_i64(*i),
        Value::Double(f) => json_f64(*f),
        Value::Boolean(b) => J::Bool(*b),
        Value::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => s.into(),
            Err(_) => json_bytes(b),
        },
        Value::SimpleString(s) => s.as_str().into(),
        Value::VerbatimString { text, .. } => text.as_str().into(),
        Value::Okay => "OK".into(),
        Value::Attribute { data, .. } => cell(data),
        // A command that failed inside an EXEC, as redis-cli shows it.
        Value::ServerError(e) => format!("(error) {} {}", e.code(), e.details().unwrap_or_default()).trim_end().into(),
        other => match to_json(other) {
            J::String(s) => J::String(s),
            j => j.to_string().into(),
        },
    }
}

/// A reply as JSON: maps with text keys become objects.
pub fn to_json(v: &Value) -> J {
    match v {
        Value::Array(a) | Value::Set(a) => J::Array(a.iter().map(to_json).collect()),
        Value::Push { data, .. } => J::Array(data.iter().map(to_json).collect()),
        Value::Map(m) => {
            let mut o = serde_json::Map::new();
            for (k, val) in m {
                o.insert(text_of(k), to_json(val));
            }
            J::Object(o)
        }
        Value::Attribute { data, .. } => to_json(data),
        Value::BigNumber(n) => J::String(format!("{n:?}")),
        Value::ServerError(e) => J::String(e.to_string()),
        Value::Nil => J::Null,
        Value::Int(i) => json_i64(*i),
        Value::Double(f) => json_f64(*f),
        Value::Boolean(b) => J::Bool(*b),
        other => cell_text(other),
    }
}

fn cell_text(v: &Value) -> J {
    match v {
        Value::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => s.into(),
            Err(_) => json_bytes(b),
        },
        other => text_of(other).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }
    fn args(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn get_is_a_value_and_nil_is_null() {
        let t = shape(&args("GET k"), b("hi"));
        assert_eq!(t.column_names(), ["value"]);
        assert_eq!(t.rows, vec![vec![J::from("hi")]]);
        let t = shape(&args("GET missing"), Value::Nil);
        assert_eq!(t.rows, vec![vec![J::Null]]);
    }

    #[test]
    fn scalars_go_in_result() {
        let t = shape(&args("DBSIZE"), Value::Int(3));
        assert_eq!(t.column_names(), ["result"]);
        assert_eq!(t.rows[0][0], J::from(3));
        assert_eq!(shape(&args("SET a 1"), Value::Okay).rows[0][0], J::from("OK"));
    }

    #[test]
    fn hgetall_is_field_value() {
        let t = shape(&args("HGETALL h"), Value::Array(vec![b("a"), b("1"), b("b"), b("2")]));
        assert_eq!(t.column_names(), ["field", "value"]);
        assert_eq!(t.rows, vec![vec![J::from("a"), J::from("1")], vec![J::from("b"), J::from("2")]]);
        // RESP3 map.
        let t = shape(&args("HGETALL h"), Value::Map(vec![(b("a"), b("1"))]));
        assert_eq!(t.rows, vec![vec![J::from("a"), J::from("1")]]);
    }

    #[test]
    fn zrange_withscores_is_member_score() {
        let t = shape(&args("ZRANGE z 0 -1 withscores"), Value::Array(vec![b("m"), b("1.5")]));
        assert_eq!(t.column_names(), ["member", "score"]);
        assert_eq!(t.rows, vec![vec![J::from("m"), J::from(1.5)]]);
        let t = shape(&args("ZRANGE z 0 -1"), Value::Array(vec![b("m")]));
        assert_eq!(t.column_names(), ["value"]);
    }

    #[test]
    fn arrays_are_one_row_per_element_nested_as_json() {
        let t = shape(
            &args("LRANGE l 0 -1"),
            Value::Array(vec![b("x"), Value::Nil, Value::Array(vec![b("a"), Value::Int(1)])]),
        );
        assert_eq!(t.column_names(), ["value"]);
        assert_eq!(t.rows, vec![vec![J::from("x")], vec![J::Null], vec![J::from(r#"["a",1]"#)]]);
    }

    #[test]
    fn xrange_has_a_column_per_field() {
        let e = |id: &str, kv: &[&str]| Value::Array(vec![b(id), Value::Array(kv.iter().map(|s| b(s)).collect())]);
        let t = shape(&args("XRANGE s - +"), Value::Array(vec![e("1-0", &["a", "1"]), e("2-0", &["b", "2", "a", "3"])]));
        assert_eq!(t.column_names(), ["id", "a", "b"]);
        assert_eq!(t.rows[0], vec![J::from("1-0"), J::from("1"), J::Null]);
        assert_eq!(t.rows[1], vec![J::from("2-0"), J::from("3"), J::from("2")]);
    }

    #[test]
    fn scan_reports_the_cursor() {
        let t = shape(&args("SCAN 0"), Value::Array(vec![b("17"), Value::Array(vec![b("k1"), b("k2")])]));
        assert_eq!(t.rows.len(), 2);
        assert!(t.message.unwrap().contains("17"));
        let t = shape(&args("HSCAN h 0"), Value::Array(vec![b("0"), Value::Array(vec![b("f"), b("v")])]));
        assert_eq!(t.column_names(), ["field", "value"]);
    }

    #[test]
    fn info_is_section_field_value() {
        let t = shape(&args("INFO"), b("# Server\r\nredis_version:7.2.0\r\n\r\n# Clients\r\nconnected_clients:1\r\n"));
        assert_eq!(t.rows[0], vec![J::from("Server"), J::from("redis_version"), J::from("7.2.0")]);
        assert_eq!(t.rows[1][0], J::from("Clients"));
    }

    #[test]
    fn binary_is_hex() {
        assert_eq!(cell(&Value::BulkString(vec![0xff, 0])), J::from("0xFF00"));
    }
}

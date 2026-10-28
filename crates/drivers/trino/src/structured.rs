//! Structured values (`row`, `array`, `map`) with their field names.
//!
//! Trino's client protocol sends a `row` as a positional JSON array; the names
//! of its fields are only in the column's type string, for example
//! `array(row(customer_id bigint, "a b" boolean, items array(row(n varchar))))`.
//! This module parses that type and rewrites the value so each named field is
//! shown under its name, recursively in nested rows, arrays of rows and maps
//! whose values are rows.
//!
//! A row whose fields are all unnamed (`row(integer, varchar)`) stays a JSON
//! array: Trino's own `CAST(... AS JSON)` gives every unnamed field the key
//! `""`, which repeats and would lose values in a JSON viewer. An unnamed field
//! in a row that also has named ones gets the key `field<i>` (0-based).
//!
//! PrestoDB sends each structured value as JSON text instead, with the nested
//! ones as JSON text inside it; where the type says a structure is expected,
//! that text is parsed and rewritten the same way.

use dbine_driver::{json_bytes, json_i64, json_u64};
use serde_json::Value;

/// The parts of a Trino type that change how a value is shown.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    Row(Vec<(Option<String>, Ty)>),
    Array(Box<Ty>),
    Map(Box<Ty>),
    Varbinary,
    Other,
}

impl Ty {
    pub(crate) fn is_structured(&self) -> bool {
        matches!(self, Ty::Row(_) | Ty::Array(_) | Ty::Map(_))
    }
}

/// Deeper nesting (or a longer type string) than any real schema is shown
/// as it came: the type comes from the server, and parsing it recursively
/// without a limit could overflow the stack.
const MAX_DEPTH: usize = 32;
const MAX_TYPE_LEN: usize = 64 * 1024;

/// Parses a Trino type string. Anything not understood is `Ty::Other`, so the
/// value is shown as it came.
pub(crate) fn parse(ty: &str) -> Ty {
    if ty.len() > MAX_TYPE_LEN {
        return Ty::Other;
    }
    parse_at(ty, 0)
}

fn parse_at(ty: &str, depth: usize) -> Ty {
    if depth > MAX_DEPTH {
        return Ty::Other;
    }
    let t = ty.trim();
    if let Some(inner) = args(t, "row") {
        return Ty::Row(
            split_top(inner)
                .into_iter()
                .map(|f| field(f, depth + 1))
                .collect(),
        );
    }
    if let Some(inner) = args(t, "array") {
        return Ty::Array(Box::new(parse_at(inner, depth + 1)));
    }
    if let Some(inner) = args(t, "map") {
        let parts = split_top(inner);
        return match parts.as_slice() {
            [_, v] => Ty::Map(Box::new(parse_at(v, depth + 1))),
            _ => Ty::Other,
        };
    }
    if t.eq_ignore_ascii_case("varbinary") {
        return Ty::Varbinary;
    }
    Ty::Other
}

/// `name(inner)` → `inner`, case-insensitive on the name.
fn args<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let head = t.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    t[name.len()..]
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// Splits on the commas outside parentheses and double quotes.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// One row field: `name type`, `"quoted name" type` or just `type`.
fn field(f: &str, depth: usize) -> (Option<String>, Ty) {
    let f = f.trim();
    if let Some(rest) = f.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if matches!(chars.peek(), Some((_, '"'))) {
                    chars.next();
                    name.push('"');
                } else {
                    return (Some(name), parse_at(&rest[i + 1..], depth));
                }
            } else {
                name.push(c);
            }
        }
        return (None, Ty::Other);
    }
    let Some((head, rest)) = f.split_once(char::is_whitespace) else {
        return (None, parse_at(f, depth));
    };
    let rest = rest.trim_start();
    let low = rest.to_ascii_lowercase();
    // Types written with spaces: `timestamp(3) with time zone`,
    // `time without time zone`, `interval day to second`.
    let unnamed = head.contains('(')
        || low.starts_with("with ")
        || low.starts_with("without ")
        || low.starts_with("to ")
        || (head.eq_ignore_ascii_case("interval")
            && (low.starts_with("day ") || low.starts_with("year ")));
    if unnamed {
        (None, parse_at(f, depth))
    } else {
        (Some(head.to_string()), parse_at(rest, depth))
    }
}

/// A Trino value rendered as JSON text, with field names where the type has them.
pub(crate) fn render(v: &Value, ty: &Ty) -> String {
    let mut out = String::new();
    write(&mut out, v, ty);
    out
}

fn write(out: &mut String, v: &Value, ty: &Ty) {
    match (v, ty) {
        (Value::Array(items), Ty::Row(fields))
            if items.len() == fields.len() && fields.iter().any(|(n, _)| n.is_some()) =>
        {
            out.push('{');
            for (i, (item, (name, fty))) in items.iter().zip(fields).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let key = name.clone().unwrap_or_else(|| format!("field{i}"));
                out.push_str(&Value::String(key).to_string());
                out.push(':');
                write(out, item, fty);
            }
            out.push('}');
        }
        (Value::Array(items), _) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let ity = match ty {
                    Ty::Array(e) => e.as_ref(),
                    Ty::Row(fields) => fields.get(i).map_or(&Ty::Other, |(_, t)| t),
                    _ => &Ty::Other,
                };
                write(out, item, ity);
            }
            out.push(']');
        }
        (Value::Object(m), _) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write(
                    out,
                    item,
                    if let Ty::Map(vt) = ty { vt } else { &Ty::Other },
                );
            }
            out.push('}');
        }
        (Value::String(s), ty) if ty.is_structured() => match structure(s) {
            Some(parsed) => write(out, &parsed, ty),
            None => out.push_str(&v.to_string()),
        },
        (v, ty) => out.push_str(&scalar(v.clone(), ty).to_string()),
    }
}

/// PrestoDB's JSON text for a structured value, parsed.
pub(crate) fn structure(s: &str) -> Option<Value> {
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(|v| v.is_array() || v.is_object())
}

/// A scalar as the UI wants it: integers beyond JavaScript's safe range as
/// text, `varbinary` (base64 on the wire) as hex.
pub(crate) fn scalar(v: Value, ty: &Ty) -> Value {
    match v {
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json_i64(i),
            (None, Some(u)) => json_u64(u),
            _ => Value::Number(n),
        },
        Value::String(s) if *ty == Ty::Varbinary => {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(&s) {
                Ok(b) => json_bytes(&b),
                Err(_) => Value::String(s),
            }
        }
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_or_huge_types_stay_as_they_came() {
        // A server-supplied type nested far beyond any real schema.
        let deep = format!("{}integer{}", "array(".repeat(20_000), ")".repeat(20_000));
        assert!(matches!(parse(&deep), Ty::Other));
        let rows = format!("{}x integer{}", "row(a ".repeat(5_000), ")".repeat(5_000));
        // Rows stop at the depth cap: the top stays a row, the 33rd level is shown as it came.
        assert!(matches!(parse(&rows), Ty::Row(_)));
        // Real nesting still parses.
        assert!(matches!(
            parse("array(array(row(a integer)))"),
            Ty::Array(_)
        ));
    }
    use serde_json::json;

    fn r(v: Value, ty: &str) -> String {
        render(&v, &parse(ty))
    }

    #[test]
    fn issue_example_gets_field_names() {
        let ty = "array(row(customer_id bigint, active boolean, items array(row(product_name varchar, quantity integer)), note varchar))";
        assert_eq!(
            r(json!([[42, true, [["demo-product", 2]], null]]), ty),
            r#"[{"customer_id":42,"active":true,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]"#
        );
    }

    #[test]
    fn nulls_empty_strings_and_empty_arrays_stay_distinct() {
        let ty = "row(a varchar, b array(integer), c varchar, d row(x integer))";
        assert_eq!(
            r(json!(["", [], null, null]), ty),
            r#"{"a":"","b":[],"c":null,"d":null}"#
        );
        assert_eq!(r(json!([]), "array(row(a integer))"), "[]");
    }

    #[test]
    fn quoted_and_keyword_names() {
        let ty = r#"row("a b" boolean, "x""y" integer, "from" integer, Upper integer, interval integer)"#;
        assert_eq!(
            r(json!([true, 1, 2, 3, 4]), ty),
            r#"{"a b":true,"x\"y":1,"from":2,"Upper":3,"interval":4}"#
        );
    }

    #[test]
    fn unnamed_fields() {
        // All unnamed: kept positional.
        assert_eq!(r(json!([1, "x"]), "row(integer, varchar(1))"), r#"[1,"x"]"#);
        assert_eq!(
            r(
                json!(["1 00:00:00.000", "2020-01-01 00:00:00.000 UTC"]),
                "row(INTERVAL DAY TO SECOND, timestamp(3) with time zone)"
            ),
            r#"["1 00:00:00.000","2020-01-01 00:00:00.000 UTC"]"#
        );
        // Mixed: the unnamed one gets a positional key.
        assert_eq!(
            r(
                json!([1, "2020-01-01 00:00:00.000 UTC", 2]),
                "row(interval integer, timestamp with time zone, b integer)"
            ),
            r#"{"interval":1,"field1":"2020-01-01 00:00:00.000 UTC","b":2}"#
        );
    }

    #[test]
    fn maps_of_rows() {
        assert_eq!(
            r(
                json!({"k": [1, [["p", 2]]]}),
                "map(varchar(1), row(id integer, items array(row(n varchar, q integer))))"
            ),
            r#"{"k":{"id":1,"items":[{"n":"p","q":2}]}}"#
        );
        assert_eq!(
            r(json!({"k": [1]}), "map(varchar, row(integer))"),
            r#"{"k":[1]}"#
        );
    }

    #[test]
    fn scalars_inside_rows() {
        let ty = "row(bin varbinary, d decimal(5, 2), big bigint, u bigint, f double)";
        assert_eq!(
            r(
                json!([
                    "yv4=",
                    "1.50",
                    9007199254740993i64,
                    18446744073709551615u64,
                    1.5
                ]),
                ty
            ),
            r#"{"bin":"0xCAFE","d":"1.50","big":"9007199254740993","u":"18446744073709551615","f":1.5}"#
        );
    }

    #[test]
    fn presto_json_text_is_unwrapped() {
        // PrestoDB: each structured value is JSON text, nested ones again.
        let item = json!(["p", 2]).to_string();
        let items = json!([item]).to_string();
        let row = json!([42, items, null]).to_string();
        let v = Value::String(json!([row]).to_string());
        let ty = "array(row(customer_id bigint,items array(row(product_name varchar,quantity integer)),note varchar))";
        assert_eq!(
            r(v, ty),
            r#"[{"customer_id":42,"items":[{"product_name":"p","quantity":2}],"note":null}]"#
        );
        assert_eq!(
            r(
                json!("{\n  \"k\" : \"[ 1 ]\"\n}"),
                "map(varchar(1),row(id integer))"
            ),
            r#"{"k":{"id":1}}"#
        );
        // Text that isn't a structure is left alone, and so is varchar.
        assert_eq!(r(json!(["[1"]), "array(array(integer))"), r#"["[1"]"#);
        assert_eq!(r(json!(["[1]"]), "array(varchar)"), r#"["[1]"]"#);
    }

    #[test]
    fn unknown_or_mismatched_shapes_fall_back() {
        assert_eq!(r(json!([1, 2]), "row(a integer)"), "[1,2]");
        assert_eq!(r(json!([1, 2]), "array(integer)"), "[1,2]");
        assert_eq!(r(json!({"a": 1}), "json"), r#"{"a":1}"#);
        assert_eq!(
            parse("row(\"unterminated integer)"),
            Ty::Row(vec![(None, Ty::Other)])
        );
    }
}

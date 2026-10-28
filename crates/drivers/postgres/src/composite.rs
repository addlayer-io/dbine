//! Composite values with their field names.
//!
//! The simple query protocol returns a composite (a row type, a table's row
//! type, `CREATE TYPE … AS (…)`) as its positional text, `(42,t,"{""(p,2)""}",)`.
//! The statement's description has each field's name and type, so the text
//! of every column that holds a composite (directly, in an array, a domain or
//! another composite) is rewritten as JSON: a composite as an object with its
//! field names in declared order, an array as an array, `NULL` as `null`, an
//! empty string as `""`. Numbers and booleans become JSON numbers and
//! booleans (integers beyond JavaScript's safe range as text), `json` and
//! `jsonb` stay JSON, and everything else keeps PostgreSQL's text.
//!
//! An anonymous `record` (`SELECT ROW(1, 'x')`) has no field names in the
//! description, so it keeps its positional text.

use dbine_driver::{json_i64, StatementResult};
use serde_json::Value;
use tokio_postgres::types::{Kind, Type};

/// Rewrites the cells of `r` whose column holds a composite.
pub(crate) fn name_fields(r: &mut StatementResult, cols: &[tokio_postgres::Column]) {
    let targets: Vec<(usize, &Type)> = cols
        .iter()
        .enumerate()
        .filter(|(_, c)| has_composite(c.type_(), 0))
        .map(|(i, c)| (i, c.type_()))
        .collect();
    if targets.is_empty() || cols.len() != r.columns.len() {
        return;
    }
    for row in &mut r.rows {
        for &(i, ty) in &targets {
            if let Some(Value::String(s)) = row.get(i) {
                if let Some(json) = render(s, ty) {
                    row[i] = Value::String(json);
                }
            }
        }
    }
}

fn has_composite(ty: &Type, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match ty.kind() {
        Kind::Composite(_) => true,
        Kind::Array(t) | Kind::Domain(t) => has_composite(t, depth + 1),
        _ => false,
    }
}

/// PostgreSQL's text for a value of type `ty`, as JSON text; `None` when the
/// text doesn't parse as that type.
pub(crate) fn render(text: &str, ty: &Type) -> Option<String> {
    let mut out = String::new();
    write(&mut out, text, ty, 0)?;
    Some(out)
}

fn write(out: &mut String, text: &str, ty: &Type, depth: usize) -> Option<()> {
    if depth > 32 {
        return None;
    }
    match ty.kind() {
        Kind::Domain(base) => write(out, text, base, depth + 1),
        Kind::Composite(fields) => {
            let items = record_items(text)?;
            if items.len() != fields.len() {
                return None;
            }
            out.push('{');
            for (i, (f, item)) in fields.iter().zip(items).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(f.name().to_string()).to_string());
                out.push(':');
                match item {
                    None => out.push_str("null"),
                    Some(s) => write(out, &s, f.type_(), depth + 1)?,
                }
            }
            out.push('}');
            Some(())
        }
        Kind::Array(elem) => {
            // `[1:2]={…}`: a lower bound other than 1.
            let body = match text.strip_prefix('[') {
                Some(_) => text.split_once('=')?.1,
                None => text,
            };
            let mut p = ArrayParser {
                s: body.as_bytes(),
                text: body,
                i: 0,
            };
            p.array(out, elem, depth + 1)?;
            (p.i == body.len()).then_some(())
        }
        _ => {
            out.push_str(&scalar(text, ty).to_string());
            Some(())
        }
    }
}

/// A scalar's text as JSON.
fn scalar(text: &str, ty: &Type) -> Value {
    match *ty {
        Type::BOOL => match text {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => text.into(),
        },
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => {
            text.parse::<i64>().map_or_else(|_| text.into(), json_i64)
        }
        Type::FLOAT4 | Type::FLOAT8 => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| text.into(), Value::Number),
        Type::JSON | Type::JSONB => serde_json::from_str(text).unwrap_or_else(|_| text.into()),
        _ => text.into(),
    }
}

/// The fields of a record's text `(a,"b c",,"")`: `None` for NULL.
fn record_items(text: &str) -> Option<Vec<Option<String>>> {
    let inner = text.strip_prefix('(')?.strip_suffix(')')?;
    let mut items = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        // One field, up to the next comma outside quotes.
        let (mut cur, mut quoted, mut in_q, mut more) = (String::new(), false, false, false);
        while let Some(c) = chars.next() {
            match c {
                '"' if in_q && chars.peek() == Some(&'"') => {
                    chars.next();
                    cur.push('"');
                }
                '"' => {
                    in_q = !in_q;
                    quoted = true;
                }
                '\\' => cur.push(chars.next()?),
                ',' if !in_q => {
                    more = true;
                    break;
                }
                c => cur.push(c),
            }
        }
        if in_q {
            return None;
        }
        items.push((quoted || !cur.is_empty()).then_some(cur));
        if !more {
            break;
        }
    }
    Some(items)
}

struct ArrayParser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl ArrayParser<'_> {
    fn array(&mut self, out: &mut String, elem: &Type, depth: usize) -> Option<()> {
        if depth > 32 || self.s.get(self.i) != Some(&b'{') {
            return None;
        }
        self.i += 1;
        out.push('[');
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            out.push(']');
            return Some(());
        }
        let mut first = true;
        loop {
            if !first {
                out.push(',');
            }
            first = false;
            match self.s.get(self.i)? {
                b'{' => self.array(out, elem, depth + 1)?,
                b'"' => {
                    let item = self.quoted()?;
                    write(out, &item, elem, depth + 1)?;
                }
                _ => {
                    let start = self.i;
                    while !matches!(self.s.get(self.i)?, b',' | b'}') {
                        self.i += 1;
                    }
                    let item = self.text[start..self.i].trim();
                    if item.eq_ignore_ascii_case("NULL") {
                        out.push_str("null");
                    } else {
                        write(out, item, elem, depth + 1)?;
                    }
                }
            }
            match self.s.get(self.i)? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    out.push(']');
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// A quoted element, `\` escapes undone.
    fn quoted(&mut self) -> Option<String> {
        self.i += 1;
        let mut buf = Vec::new();
        loop {
            match *self.s.get(self.i)? {
                b'"' => {
                    self.i += 1;
                    return String::from_utf8(buf).ok();
                }
                b'\\' => {
                    buf.push(*self.s.get(self.i + 1)?);
                    self.i += 2;
                }
                b => {
                    buf.push(b);
                    self.i += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::types::Field;

    fn composite(name: &str, fields: Vec<(&str, Type)>) -> Type {
        Type::new(
            name.into(),
            100_000,
            Kind::Composite(
                fields
                    .into_iter()
                    .map(|(n, t)| Field::new(n.into(), t))
                    .collect(),
            ),
            "public".into(),
        )
    }

    fn array_of(t: Type) -> Type {
        Type::new(
            format!("_{}", t.name()),
            100_001,
            Kind::Array(t),
            "public".into(),
        )
    }

    #[test]
    fn issue_example() {
        let item = composite(
            "item",
            vec![("product_name", Type::TEXT), ("quantity", Type::INT4)],
        );
        let order = composite(
            "order_t",
            vec![
                ("customer_id", Type::INT8),
                ("active", Type::BOOL),
                ("items", array_of(item)),
                ("note", Type::TEXT),
            ],
        );
        assert_eq!(
            render(
                r#"{"(42,t,\"{\"\"(demo-product,2)\"\"}\",)"}"#,
                &array_of(order)
            )
            .as_deref(),
            Some(
                r#"[{"customer_id":42,"active":true,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]"#
            )
        );
    }

    #[test]
    fn nulls_empty_strings_and_empty_arrays() {
        let t = composite(
            "t",
            vec![
                ("a", Type::TEXT),
                ("b", Type::INT4_ARRAY),
                ("c", Type::TEXT),
                ("d", Type::TEXT_ARRAY),
            ],
        );
        assert_eq!(
            render(r#"("",{},,"{NULL,"""",x}")"#, &t).as_deref(),
            Some(r#"{"a":"","b":[],"c":null,"d":[null,"","x"]}"#)
        );
    }

    #[test]
    fn quoting_numbers_and_json() {
        let t = composite(
            "t",
            vec![
                ("a b", Type::TEXT),
                ("big", Type::INT8),
                ("d", Type::NUMERIC),
                ("j", Type::JSONB),
                ("bin", Type::BYTEA),
                ("f", Type::FLOAT8),
            ],
        );
        assert_eq!(
            render(
                r#"("x,""y"")",9007199254740993,1.50,"{""k"": [1]}","\\xcafe",NaN)"#,
                &t
            )
            .as_deref(),
            Some(
                r#"{"a b":"x,\"y\")","big":"9007199254740993","d":"1.50","j":{"k":[1]},"bin":"\\xcafe","f":"NaN"}"#
            )
        );
    }

    #[test]
    fn nested_composites_and_bounds() {
        let inner = composite("inner_t", vec![("x", Type::INT4)]);
        let outer = composite("outer_t", vec![("i", inner.clone()), ("n", Type::INT4)]);
        assert_eq!(
            render(r#"("(1)",2)"#, &outer).as_deref(),
            Some(r#"{"i":{"x":1},"n":2}"#)
        );
        assert_eq!(
            render(r#"(,2)"#, &outer).as_deref(),
            Some(r#"{"i":null,"n":2}"#)
        );
        assert_eq!(
            render(r#"[0:1]={"(1)","(2)"}"#, &array_of(inner.clone())).as_deref(),
            Some(r#"[{"x":1},{"x":2}]"#)
        );
        // Multi-dimensional.
        assert_eq!(
            render(r#"{{"(1)"},{NULL}}"#, &array_of(inner)).as_deref(),
            Some(r#"[[{"x":1}],[null]]"#)
        );
    }

    #[test]
    fn text_that_does_not_fit_is_left_alone() {
        let t = composite("t", vec![("a", Type::INT4), ("b", Type::INT4)]);
        assert_eq!(render("(1)", &t), None);
        assert_eq!(render("1,2", &t), None);
        assert_eq!(render(r#"("1,2)"#, &t), None);
        assert_eq!(render("{(1,2)", &array_of(t)), None);
    }
}

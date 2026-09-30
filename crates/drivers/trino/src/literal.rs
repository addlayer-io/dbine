//! INSERT scripts for the Trino engine (Athena includes this file too).
//! Trino never coerces `varchar` to `date`, `timestamp` or `varbinary` on
//! INSERT, so strings shaped like those values go as typed literals.

use dbine_driver::ddl::{sql_literal, SqlFlavor};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use serde_json::Value;

/// A value as a Trino literal.
pub fn literal(v: &Value) -> String {
    if let Value::String(s) = v {
        if let Some(kw) = temporal(s) {
            // `2024-01-31T10:00` → `2024-01-31 10:00`.
            let text = if s.len() > 10 { format!("{} {}", &s[..10], &s[11..]) } else { s.clone() };
            return format!("{kw} '{text}'");
        }
        if let Some(hex) = s.strip_prefix("0x").filter(|h| !h.is_empty() && h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            return format!("X'{hex}'");
        }
    }
    sql_literal(&SqlFlavor::ansi(), v)
}

/// `DATE` for `YYYY-MM-DD`, `TIMESTAMP` for `YYYY-MM-DD[ T]HH:MM[:SS[.fff]]`.
fn temporal(s: &str) -> Option<&'static str> {
    let b = s.as_bytes();
    let digits = |from: usize, to: usize| b.get(from..to).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    if !(b.len() >= 10 && digits(0, 4) && b[4] == b'-' && digits(5, 7) && b[7] == b'-' && digits(8, 10)) {
        return None;
    }
    if b.len() == 10 {
        return Some("DATE");
    }
    if !(matches!(b[10], b' ' | b'T') && digits(11, 13) && b.get(13) == Some(&b':') && digits(14, 16)) {
        return None;
    }
    let ok = match &b[16..] {
        [] => true,
        [b':', s1, s2, rest @ ..] if s1.is_ascii_digit() && s2.is_ascii_digit() => match rest {
            [] => true,
            [b'.', f @ ..] => !f.is_empty() && f.iter().all(u8::is_ascii_digit),
            _ => false,
        },
        _ => false,
    };
    ok.then_some("TIMESTAMP")
}

/// Multi-row `INSERT … VALUES` with double-quoted identifiers.
pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let name = qualified_name(Quote::Double, schema.filter(|s| !s.is_empty()), table);
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(Quote::Double, c)).collect();
    rows.chunks(100)
        .map(|chunk| {
            let tuples: Vec<String> =
                chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
            format!("INSERT INTO {name} ({}) VALUES\n  {};", cols.join(", "), tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … SET … WHERE …;` per edited row, with the same typed literals
/// (the connector decides whether UPDATE is supported, e.g. Iceberg).
pub fn update_script(schema: Option<&str>, table: &str, changes: &[dbine_driver::RowChange]) -> String {
    dbine_driver::ddl::update_script_with(Quote::Double, schema, table, changes, &literal)
}

/// `DELETE … WHERE <key>` per row key, with the same typed literals (the
/// connector decides whether DELETE is supported, e.g. Iceberg).
pub fn delete_script(schema: Option<&str>, table: &str, keys: &[Vec<(String, Value)>]) -> String {
    dbine_driver::ddl::delete_script_with(Quote::Double, schema, table, keys, &literal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_script_per_row() {
        let c = dbine_driver::RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("alta".into(), json!("2024-01-31")), ("baja".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("ventas"), "clientes", &[c, dbine_driver::RowChange::default()]),
            "UPDATE \"ventas\".\"clientes\" SET \"nombre\" = 'O''Brien', \"alta\" = DATE '2024-01-31', \"baja\" = NULL \
             WHERE \"id\" = 7 AND \"region\" IS NULL;"
        );
    }

    #[test]
    fn delete_script_per_row() {
        let keys = vec![
            vec![("nombre".into(), json!("O'Brien")), ("alta".into(), json!("2024-01-31")), ("region".into(), Value::Null)],
            vec![],
        ];
        assert_eq!(
            delete_script(Some("ventas"), "clientes", &keys),
            "DELETE FROM \"ventas\".\"clientes\" WHERE \"nombre\" = 'O''Brien' AND \"alta\" = DATE '2024-01-31' AND \"region\" IS NULL;"
        );
    }

    #[test]
    fn typed_literals() {
        assert_eq!(literal(&json!("2024-01-31")), "DATE '2024-01-31'");
        assert_eq!(literal(&json!("2024-01-31T10:00")), "TIMESTAMP '2024-01-31 10:00'");
        assert_eq!(literal(&json!("2024-01-31 10:00:05.123")), "TIMESTAMP '2024-01-31 10:00:05.123'");
        assert_eq!(literal(&json!("2024-01-31 x")), "'2024-01-31 x'");
        assert_eq!(literal(&json!("2024-01-31 10:00:05+01:00")), "'2024-01-31 10:00:05+01:00'");
        assert_eq!(literal(&json!("0xCAFE")), "X'CAFE'");
        assert_eq!(literal(&json!("0xCAF")), "'0xCAF'");
        assert_eq!(literal(&json!("O'Brien")), "'O''Brien'");
        assert_eq!(literal(&json!(true)), "TRUE");
        assert_eq!(literal(&Value::Null), "NULL");
    }

    #[test]
    fn insert_batches() {
        let rows = vec![vec![json!(1), json!("2024-01-31")], vec![json!(2), Value::Null]];
        let s = insert_script(Some("s"), "t", &["id".into(), "d".into()], &rows);
        assert_eq!(s, "INSERT INTO \"s\".\"t\" (\"id\", \"d\") VALUES\n  (1, DATE '2024-01-31'),\n  (2, NULL);");
    }
}

//! Names and literals in DAX (spec §4.6 `browse_query`). Owned by part B;
//! part C writes definitions and object names with these.

use dbine_driver::params::{literal as param_literal, ParamValue};
use dbine_driver::{Error, Language, Result};
use serde_json::Value;

/// `'name'`, a quote doubled (`'Sales d''Or'`). Always quoted: correct for
/// any name, reserved words included.
pub fn quote_table(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// `[name]`, a closing bracket doubled (`[Total [neto]]]`).
pub fn quote_column(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `'Table'[Column]`.
pub fn column_ref(table: &str, column: &str) -> String {
    format!("{}{}", quote_table(table), quote_column(column))
}

/// `"text"`, a quote doubled.
pub fn string(s: &str) -> String {
    dbine_driver::params::dax_string(s)
}

/// A result column's name as the service writes it (`Sales[Amount]`,
/// `[Total]`, or a plain `Amount` of a stripped table browse) as a DAX
/// reference; a plain name belongs to `table` when given.
pub fn result_column_ref(name: &str, table: Option<&str>) -> String {
    if let Some(open) = name.find('[') {
        if name.ends_with(']') && open + 1 < name.len() {
            let column = &name[open + 1..name.len() - 1];
            let t = &name[..open];
            return if t.is_empty() {
                quote_column(column)
            } else {
                column_ref(t, column)
            };
        }
    }
    match table {
        Some(t) => column_ref(t, name),
        None => quote_column(name),
    }
}

fn bad(v: &Value) -> Error {
    let shown = match v {
        Value::String(s) => s.chars().take(40).collect(),
        other => other.to_string().chars().take(40).collect::<String>(),
    };
    Error::Query(format!("«{shown}» no es un valor que DAX pueda comparar"))
}

/// A grid value (filters, keyset paging) as a DAX literal of the column's
/// model type: `"…"` text, numbers as they are, `TRUE()`/`FALSE()`,
/// `BLANK()`, dates as `DATE(…) + TIME(…)` with milliseconds kept (G1).
/// The same rules as `dbine_driver::params::literal` for `Language::Dax`.
pub fn literal(value: &Value, data_type: Option<&str>) -> Result<String> {
    let ty = data_type.unwrap_or("").to_ascii_lowercase();
    let numeric = matches!(ty.as_str(), "int64" | "decimal" | "double");
    let pv = match value {
        Value::Null => ParamValue::Null,
        Value::Bool(b) => ParamValue::Bool(*b),
        Value::Number(n) => ParamValue::number(&n.to_string()).ok_or_else(|| bad(value))?,
        Value::String(s) if ty == "datetime" => {
            let s = s.trim();
            ParamValue::datetime(s)
                .or_else(|| ParamValue::date(s))
                .ok_or_else(|| bad(value))?
        }
        Value::String(s) if numeric => ParamValue::number(s).ok_or_else(|| bad(value))?,
        Value::String(s) if ty == "boolean" => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => ParamValue::Bool(true),
            "false" | "0" => ParamValue::Bool(false),
            _ => return Err(bad(value)),
        },
        Value::String(s) => ParamValue::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => return Err(bad(value)),
    };
    param_literal(Language::Dax, "powerbi", "powerbi", &pv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_double_their_quote() {
        assert_eq!(quote_table("Sales d'Or"), "'Sales d''Or'");
        assert_eq!(quote_column("Total [neto]"), "[Total [neto]]]");
        assert_eq!(column_ref("T", "c"), "'T'[c]");
        assert_eq!(result_column_ref("Date[Year]", None), "'Date'[Year]");
        assert_eq!(result_column_ref("[Ventas]", Some("T")), "[Ventas]");
        assert_eq!(
            result_column_ref("Amount", Some("Sales")),
            "'Sales'[Amount]"
        );
        assert_eq!(result_column_ref("Amount", None), "[Amount]");
        assert_eq!(result_column_ref("Sales d'Or[x]", None), "'Sales d''Or'[x]");
    }

    #[test]
    fn literals_by_type() {
        assert_eq!(literal(&json!("O\"Brien"), None).unwrap(), "\"O\"\"Brien\"");
        assert_eq!(literal(&json!(42), Some("Int64")).unwrap(), "42");
        assert_eq!(literal(&json!(-2.5), None).unwrap(), "-2.5");
        assert_eq!(
            literal(&json!("12.3400"), Some("Decimal")).unwrap(),
            "12.3400"
        );
        assert_eq!(literal(&json!(true), None).unwrap(), "TRUE()");
        assert_eq!(
            literal(&json!("false"), Some("Boolean")).unwrap(),
            "FALSE()"
        );
        assert_eq!(literal(&Value::Null, Some("String")).unwrap(), "BLANK()");
        assert_eq!(
            literal(&json!("2024-01-31 13:45:00.123"), Some("DateTime")).unwrap(),
            "DATE(2024, 1, 31) + TIME(13, 45, 0) + 0.123 / 86400"
        );
        assert_eq!(
            literal(&json!("2024-01-31"), Some("DateTime")).unwrap(),
            "DATE(2024, 1, 31)"
        );
        // Text for a number column, or a date that isn't one: refused, never
        // written into the query.
        for (v, t) in [
            (json!("1) EVALUATE 'Secret'"), "Int64"),
            (json!("2024-01-31\") EVALUATE x"), "DateTime"),
            (json!([1]), "String"),
        ] {
            assert!(literal(&v, Some(t)).is_err(), "{v}");
        }
    }
}

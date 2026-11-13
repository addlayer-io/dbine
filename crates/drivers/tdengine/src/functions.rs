//! The function catalog (issue #97): TDengine lists only the user's UDFs
//! (`information_schema.ins_functions`), so the built-ins come from a static
//! catalog filtered by the server's version, and the UDFs are added to it.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value;

const CATALOG: &str = include_str!("functions/tdengine.txt");

/// The UDFs, without `func_body` (the library itself). `comment` and
/// `aggregate` are keywords.
pub const UDFS: &str = "SELECT `name`, `comment`, `aggregate`, `output_type` \
                        FROM information_schema.ins_functions";

/// The built-ins of a server of `version` (`TDengine 3.3.6.13`).
pub fn builtins(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, version)
}

/// `UDFS`' rows (by column name) as catalog entries: a UDF takes whatever
/// columns it's given, so it has no arguments to show; its result type is
/// `output_type`, and its comment, when there is one, the description.
pub fn udfs(columns: &[String], rows: &[Vec<Value>]) -> Vec<FunctionInfo> {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let Some(name_at) = col("name") else {
        return Vec::new();
    };
    let (comment_at, agg_at, type_at) = (col("comment"), col("aggregate"), col("output_type"));
    let text = |row: &Vec<Value>, at: Option<usize>| match at.and_then(|i| row.get(i)) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    rows.iter()
        .filter_map(|row| {
            let name = text(row, Some(name_at));
            if name.is_empty() {
                return None;
            }
            let aggregate = matches!(
                agg_at.and_then(|i| row.get(i)),
                Some(Value::Number(n)) if n.as_i64() == Some(1)
            ) || matches!(agg_at.and_then(|i| row.get(i)), Some(Value::Bool(true)));
            let kind = if aggregate {
                FunctionKind::Aggregate
            } else {
                FunctionKind::Scalar
            };
            Some(
                FunctionInfo::new(name, kind)
                    .returns(text(row, type_at).to_ascii_uppercase())
                    .description(text(row, comment_at)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_static_catalog_parses() {
        for line in CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn the_builtins_follow_the_version() {
        let has = |v: &str, name: &str| builtins(Some(v)).iter().any(|f| f.name == name);
        assert!(has("TDengine 3.0.7.1", "TWA"));
        assert!(!has("TDengine 3.0.7.1", "PI"));
        assert!(has("TDengine 3.3.3.0", "PI"));
        // Not in the reference's version notes: the release tags'
        // builtins.c registers them from these versions on.
        assert!(!has("TDengine 3.2.3.0", "MD5"));
        assert!(has("TDengine 3.3.0.0", "MD5"));
        assert!(!has("TDengine 3.3.6.5", "CRC32"));
        assert!(has("TDengine 3.3.6.6", "CRC32"));
        assert!(!has("TDengine 3.3.6.12", "TO_BASE64"));
        assert!(has("TDengine 3.3.6.13", "TO_BASE64"));
        assert!(!has("TDengine 3.3.5.0", "TO_BASE64"));
        assert!(!has("TDengine 3.3.6.13", "GROUP_CONCAT"));
        assert!(has("TDengine 3.3.8.0", "GROUP_CONCAT"));
        assert!(!has("TDengine 3.4.1.0", "ROW_NUMBER"));
        assert!(has("TDengine 3.4.3.9.community", "ROW_NUMBER"));
        let rn = builtins(None)
            .into_iter()
            .find(|f| f.name == "ROW_NUMBER")
            .unwrap();
        assert_eq!(rn.kind, FunctionKind::Window);
    }

    #[test]
    fn udf_rows_map_to_entries() {
        let cols: Vec<String> = ["name", "comment", "aggregate", "output_type"]
            .map(String::from)
            .to_vec();
        let rows = vec![
            vec![json!("my_fn"), json!(""), json!(0), json!("INT")],
            vec![
                json!("my_agg"),
                json!("weighted sum"),
                json!(1),
                json!("double"),
            ],
            vec![json!(""), json!(null), json!(0), json!(null)],
        ];
        let c = udfs(&cols, &rows);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "my_fn");
        assert_eq!(c[0].kind, FunctionKind::Scalar);
        assert_eq!(c[0].returns, "INT");
        assert_eq!(c[0].args, "");
        assert_eq!(c[1].kind, FunctionKind::Aggregate);
        assert_eq!(c[1].returns, "DOUBLE");
        assert_eq!(c[1].description, "weighted sum");
        assert!(udfs(&["x".into()], &rows).is_empty());
    }
}

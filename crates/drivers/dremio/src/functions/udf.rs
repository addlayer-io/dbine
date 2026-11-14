//! The user's functions in Dremio's `sys.user_defined_functions`. Shared
//! with the Flight SQL driver, which reaches Dremio over Arrow Flight: it
//! depends on `serde_json` and the driver contract only.

use dbine_driver::functions::{FunctionInfo, FunctionKind};
use serde_json::{Map, Value};

/// `sql` is a reserved word.
pub(crate) const UDF_SQL: &str =
    r#"SELECT "name", "argList", "sql" FROM sys.user_defined_functions"#;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A row of `sys.user_defined_functions`: `name` is the function's path
/// (`"@dbine".twice`, `sales.reports.twice`) and `argList` Dremio's own
/// rendering of the parameters (`[FunctionArg{name=x, …}, …]`), from which
/// only the names are readable.
pub(crate) fn from_udf(r: &Map<String, Value>) -> Option<FunctionInfo> {
    let path = r.get("name").map(text).unwrap_or_default();
    let (schema, name) = split_path(&path);
    if name.is_empty() {
        return None;
    }
    let args = r.get("argList").map(text).unwrap_or_default();
    let names: Vec<&str> = args
        .split("FunctionArg{name=")
        .skip(1)
        .filter_map(|a| a.split([',', '}']).next())
        .map(str::trim)
        .collect();
    Some(
        FunctionInfo::new(name, FunctionKind::Scalar)
            .args(format!("({})", names.join(", ")))
            .description(r.get("sql").map(text).unwrap_or_default())
            .schema(schema),
    )
}

/// `"@dbine".twice` → (`"@dbine"`, `twice`): the last dot outside double
/// quotes; a quoted name loses its quotes.
fn split_path(path: &str) -> (String, String) {
    let mut quoted = false;
    let mut last = None;
    for (i, c) in path.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => last = Some(i),
            _ => {}
        }
    }
    let (schema, name) = match last {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let name = name
        .strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .map(|n| n.replace("\"\"", "\""))
        .unwrap_or_else(|| name.to_string());
    (schema.to_string(), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_functions_keep_their_path() {
        let Value::Object(r) = json!({
            "name": "\"@dbine\".dbine_twice",
            "argList": "[FunctionArg{name=x, rawDataType=<ByteString@f35834a size=120>, defaultExpression=null}, FunctionArg{name=k, rawDataType=<ByteString@1 size=1>, defaultExpression=null}]",
            "sql": "SELECT x * k",
        }) else {
            unreachable!()
        };
        let f = from_udf(&r).unwrap();
        assert_eq!(f.name, "dbine_twice");
        assert_eq!(f.schema, "\"@dbine\"");
        assert_eq!(f.args, "(x, k)");
        assert_eq!(f.description, "SELECT x * k");
        assert_eq!(
            split_path("sales.\"a.b\""),
            ("sales".to_string(), "a.b".to_string())
        );
    }
}

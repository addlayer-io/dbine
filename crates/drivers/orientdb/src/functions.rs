//! The function catalog (issue #97): OrientDB doesn't list its SQL
//! functions, so the bundled ones come from a static catalog filtered by
//! the server's version; the database's own functions (the `OFunction`
//! class, called from SQL by their name) are added from the server.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value;

const CATALOG: &str = include_str!("functions/orientdb.txt");

/// The database's functions and their parameters.
pub const DATABASE_FUNCTIONS: &str = "SELECT name, parameters FROM OFunction";

/// The bundled functions of a server of `version` (`OrientDB 3.2.56`).
pub fn bundled(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, version)
}

/// `DATABASE_FUNCTIONS`' records as catalog entries: each a scalar with its
/// parameters' names.
pub fn database_functions(records: &[Vec<(String, Value)>]) -> Vec<FunctionInfo> {
    records
        .iter()
        .filter_map(|r| {
            let get = |k: &str| r.iter().find(|(n, _)| n == k).map(|(_, v)| v);
            let name = get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let params: Vec<&str> = get("parameters")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Some(
                FunctionInfo::new(name, FunctionKind::Scalar)
                    .args(format!("({})", params.join(", "))),
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
    fn the_bundled_functions_follow_the_version() {
        let has = |v: &str, name: &str| bundled(Some(v)).iter().any(|f| f.name == name);
        assert!(has("OrientDB 3.2.56", "strcmpci"));
        assert!(!has("OrientDB 3.0.44", "strcmpci"));
        assert!(has("OrientDB 3.0.44", "search_class"));
        assert!(!has("OrientDB 2.2.37", "search_class"));
        assert!(has("OrientDB 2.2.37", "ST_Within"));
        let count = bundled(None)
            .into_iter()
            .find(|f| f.name == "count")
            .unwrap();
        assert_eq!(count.kind, FunctionKind::Aggregate);
        assert_eq!(count.args, "(field)");
    }

    #[test]
    fn database_functions_map_to_entries() {
        let records = vec![
            vec![
                ("name".to_string(), json!("discount")),
                ("parameters".to_string(), json!(["price", "rate"])),
            ],
            vec![
                ("name".to_string(), json!("now_utc")),
                ("parameters".to_string(), json!(null)),
            ],
            vec![("parameters".to_string(), json!(["x"]))],
        ];
        let c = database_functions(&records);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "discount");
        assert_eq!(c[0].args, "(price, rate)");
        assert_eq!(c[0].kind, FunctionKind::Scalar);
        assert_eq!(c[0].schema, "");
        assert_eq!(c[1].args, "()");
    }
}

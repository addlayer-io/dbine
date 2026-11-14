//! The editor's completion (#97) for SQL++ (N1QL): the built-in functions
//! from a static catalog (Couchbase doesn't list them), filtered by the
//! server's version, plus the user's functions from `system:functions`
//! (Couchbase 7.0+), with their scope (`bucket.scope`) or namespace
//! (`default`, for global functions) as the schema.

use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use serde_json::Value;

const N1QL: &str = include_str!("functions/n1ql.txt");

/// One statement for every user function the user can see.
pub const USER_FUNCTIONS: &str = "SELECT RAW functions FROM system:functions";

/// The built-ins of a Couchbase `version` (all of them when unknown).
pub fn builtins(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(N1QL, version)
}

/// `system:functions` rows: `{identity: {name, namespace, type, bucket,
/// scope}, definition: {parameters, #language…}}` (also wrapped in
/// `{"functions": …}`, as `SELECT *` returns them).
pub fn user_functions(rows: &[Value]) -> Vec<FunctionInfo> {
    rows.iter()
        .filter_map(|row| {
            let f = row.get("functions").unwrap_or(row);
            let id = f.get("identity")?;
            let name = id.get("name")?.as_str()?;
            let field = |k: &str| id.get(k).and_then(Value::as_str).unwrap_or("");
            let schema = match field("type") {
                "scope" => format!("{}.{}", field("bucket"), field("scope")),
                _ => field("namespace").to_string(),
            };
            // No `parameters` at all: declared with `...` (any arguments).
            let args = match f
                .pointer("/definition/parameters")
                .and_then(Value::as_array)
            {
                Some(p) => format!(
                    "({})",
                    p.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => "(...)".into(),
            };
            Some(
                FunctionInfo::new(name, FunctionKind::Scalar)
                    .args(args)
                    .schema(schema),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in N1QL.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let all = builtins(None);
        let find = |n: &str| all.iter().find(|f| f.name == n).unwrap();
        assert_eq!(find("COUNT").kind, FunctionKind::Aggregate);
        assert_eq!(find("ROW_NUMBER").kind, FunctionKind::Window);
        assert_eq!(find("ARRAY_CONTAINS").kind, FunctionKind::Scalar);
        assert_eq!(find("ARRAY_CONTAINS").args, "(expr, val)");
        assert!(all.iter().all(|f| f.description.is_empty()));
    }

    #[test]
    fn the_servers_version_filters_builtins() {
        let has = |v, n: &str| builtins(Some(v)).iter().any(|f| f.name == n);
        assert!(has(
            "Couchbase Server 8.0.0-3777-enterprise",
            "APPROX_VECTOR_DISTANCE"
        ));
        assert!(!has(
            "Couchbase Server 7.6.2-3505-enterprise",
            "APPROX_VECTOR_DISTANCE"
        ));
        assert!(has(
            "Couchbase Server 7.6.2-3505-enterprise",
            "VECTOR_DISTANCE"
        ));
        assert!(!has(
            "Couchbase Server 7.2.4-7070-enterprise",
            "VECTOR_DISTANCE"
        ));
        assert!(has(
            "Couchbase Server 7.2.4-7070-enterprise",
            "ARRAY_CONTAINS"
        ));
        assert!(!has("Couchbase Server 7.0.5-7658-community", "LPAD"));
        assert!(has("Couchbase Query 7.1.0-N1QL", "LPAD"));
    }

    #[test]
    fn user_functions_map_with_their_scope() {
        let rows = vec![
            json!({"definition": {"#language": "inline", "parameters": ["a", "b"]},
                   "identity": {"name": "add2", "namespace": "default", "type": "global"}}),
            json!({"functions": {"definition": {"#language": "javascript"},
                   "identity": {"bucket": "sales", "name": "anyargs", "namespace": "default",
                                "scope": "eu", "type": "scope"}}}),
            json!({"definition": {"parameters": []},
                   "identity": {"name": "none", "namespace": "default", "type": "global"}}),
            json!({"unexpected": true}),
        ];
        let f = user_functions(&rows);
        assert_eq!(f.len(), 3);
        assert_eq!(
            (f[0].name.as_str(), f[0].args.as_str(), f[0].schema.as_str()),
            ("add2", "(a, b)", "default")
        );
        assert_eq!(
            (f[1].name.as_str(), f[1].args.as_str(), f[1].schema.as_str()),
            ("anyargs", "(...)", "sales.eu")
        );
        assert_eq!(f[2].args, "()");
        assert!(f.iter().all(|f| f.kind == FunctionKind::Scalar));
    }
}

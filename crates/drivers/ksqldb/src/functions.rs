//! The function catalog (issue #97): `SHOW FUNCTIONS`, which lists the
//! built-ins and the user's UDFs with their type (`SCALAR`, `AGGREGATE`,
//! `TABLE`) in one request. Their arguments come from the static catalog
//! where it knows the function (`DESCRIBE FUNCTION` would cost a request
//! per function); the static catalog also stands in when the list can't be
//! read.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const CATALOG: &str = include_str!("functions/ksqldb.txt");

pub const SHOW_FUNCTIONS: &str = "SHOW FUNCTIONS";

/// Test UDFs that Confluent's ksqlDB image carries on its classpath next to
/// the built-ins: undocumented, and some throw on purpose.
const FIXTURES: &[&str] = &[
    "BAD_TEST_UDAF",
    "BAD_TEST_UDF",
    "BAD_TEST_UDTF",
    "BAD_UDF",
    "COMPLEXFUNCTION",
    "CONFIGURABLEUDF",
    "DECIMALSTRUCT",
    "E2ECONFIGURABLEUDF",
    "FIVE_ARG",
    "FOUR_ARG",
    "GENERIC_VAR_ARG",
    "KSQLSTRUCTUDF",
    "MID_VAR_ARG",
    "MULTI_ARG",
    "OBJ_COL_ARG",
    "RETURNDECIMAL",
    "RETURNINCOMPATIBLE",
    "SOMEFUNCTION",
    "TEST_UDAF",
    "TEST_UDF",
    "TEST_UDTF",
    "THROWING_UDTF",
    "VAR_ARG",
    "WHENCONDITION",
    "WHENRESULT",
];

/// Syntax written as a call, which `SHOW FUNCTIONS` doesn't list.
const SYNTAX: &[&str] = &["CAST"];

/// The static catalog.
pub fn static_catalog() -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, None)
}

/// `SHOW FUNCTIONS`' answer (`[{"@type": "function_names", "functions":
/// [{"name", "type", "category"}]}]`) as the catalog: one entry per
/// argument list the static catalog knows for the name, or one without
/// arguments for a function it doesn't (the user's UDFs, newer built-ins).
/// ksqlDB calls a table function in the SELECT list, so it's offered as a
/// scalar. `None` when the answer lists nothing.
pub fn from_show_functions(entities: &[Value]) -> Option<Vec<FunctionInfo>> {
    let funcs = entities
        .iter()
        .find_map(|e| e.get("functions").and_then(Value::as_array))?;
    let mut known: HashMap<String, Vec<FunctionInfo>> = HashMap::new();
    for f in static_catalog() {
        known
            .entry(f.name.to_ascii_uppercase())
            .or_default()
            .push(f);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for f in funcs {
        let Some(name) = f.get("name").and_then(Value::as_str) else {
            continue;
        };
        let upper = name.to_ascii_uppercase();
        if FIXTURES.contains(&upper.as_str()) || !seen.insert(upper.clone()) {
            continue;
        }
        let kind = match f.get("type").and_then(Value::as_str) {
            Some(t) if t.eq_ignore_ascii_case("AGGREGATE") => FunctionKind::Aggregate,
            _ => FunctionKind::Scalar,
        };
        match known.get(&upper) {
            Some(overloads) => {
                out.extend(overloads.iter().map(|o| FunctionInfo { kind, ..o.clone() }))
            }
            None => out.push(FunctionInfo::new(name, kind)),
        }
    }
    if out.is_empty() {
        return None;
    }
    for name in SYNTAX {
        if seen.insert(name.to_string()) {
            out.extend(known.get(*name).into_iter().flatten().cloned());
        }
    }
    Some(out)
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
        let c = static_catalog();
        assert!(c.iter().any(|f| f.name == "CAST"));
        assert!(!c.iter().any(|f| FIXTURES.contains(&f.name.as_str())));
    }

    #[test]
    fn show_functions_maps_to_entries() {
        let answer = vec![json!({
            "@type": "function_names",
            "statementText": "SHOW FUNCTIONS;",
            "functions": [
                {"name": "UCASE", "type": "SCALAR", "category": "STRING"},
                {"name": "SUBSTRING", "type": "SCALAR", "category": "STRING"},
                {"name": "TOPK", "type": "AGGREGATE", "category": "AGGREGATE"},
                {"name": "EXPLODE", "type": "TABLE", "category": "TABLE"},
                {"name": "MY_UDF", "type": "SCALAR", "category": "OTHER"},
                {"name": "BAD_UDF", "type": "SCALAR", "category": "OTHER"},
                {"name": "UCASE", "type": "SCALAR", "category": "STRING"},
            ]
        })];
        let c = from_show_functions(&answer).unwrap();
        let get = |n: &str| c.iter().filter(|f| f.name == n).collect::<Vec<_>>();
        let ucase = get("UCASE");
        assert_eq!(ucase.len(), 1);
        assert_eq!(ucase[0].args, "(input)");
        assert_eq!(ucase[0].returns, "VARCHAR");
        assert!(get("SUBSTRING").len() > 1, "its overloads");
        assert_eq!(get("TOPK")[0].kind, FunctionKind::Aggregate);
        assert_eq!(get("EXPLODE")[0].kind, FunctionKind::Scalar);
        let udf = get("MY_UDF");
        assert_eq!(
            (udf[0].kind, udf[0].args.as_str()),
            (FunctionKind::Scalar, "")
        );
        assert!(get("BAD_UDF").is_empty());
        assert_eq!(get("CAST").len(), 1);
        assert!(
            from_show_functions(&[json!({"@type": "function_names", "functions": []})]).is_none()
        );
        assert!(from_show_functions(&[json!({"@type": "currentStatus"})]).is_none());
    }
}

//! The function catalog (issue #97).
//!
//! - Phoenix: static, from its language reference (Phoenix lists none: its
//!   JDBC metadata returns empty function lists).
//! - A generic Avatica server: the function lists of its JDBC metadata
//!   (`getNumericFunctions`, `getStringFunctions`, `getSystemFunctions`,
//!   `getTimeDateFunctions`, read with one `databaseProperties` call), plus
//!   the SQL standard's catalog (`shared::ANSI`), which also gives the listed
//!   names their arguments.

use dbine_driver::functions::{parse_catalog, shared};
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value;
use std::collections::HashSet;

const PHOENIX: &str = include_str!("functions/phoenix.txt");

/// The database properties that list function names.
const LISTS: &[&str] = &[
    "GET_NUMERIC_FUNCTIONS",
    "GET_STRING_FUNCTIONS",
    "GET_SYSTEM_FUNCTIONS",
    "GET_TIME_DATE_FUNCTIONS",
];

/// Phoenix's built-ins.
pub fn phoenix() -> Vec<FunctionInfo> {
    parse_catalog(PHOENIX, None)
}

/// A generic Avatica server's catalog from its database properties (name →
/// value): the SQL standard's functions, then every listed name the standard
/// catalog doesn't have, as a scalar without arguments.
pub fn avatica(props: &[(String, Value)]) -> Vec<FunctionInfo> {
    let mut out = parse_catalog(shared::ANSI, None);
    let mut seen: HashSet<String> = out.iter().map(|f| f.name.to_ascii_uppercase()).collect();
    for (key, value) in props {
        if !LISTS.iter().any(|l| key.eq_ignore_ascii_case(l)) {
            continue;
        }
        let Some(list) = value.as_str() else {
            continue;
        };
        for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if seen.insert(name.to_ascii_uppercase()) {
                out.push(FunctionInfo::new(name, FunctionKind::Scalar));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_phoenix_catalog_parses() {
        for line in PHOENIX.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let c = phoenix();
        let pc = c.iter().find(|f| f.name == "PERCENTILE_CONT").unwrap();
        assert_eq!(pc.kind, FunctionKind::Aggregate);
        assert_eq!(
            pc.args,
            "(numeric) WITHIN GROUP (ORDER BY numericTerm [ASC | DESC])"
        );
        assert_eq!(c.iter().find(|f| f.name == "NOW").unwrap().args, "()");
    }

    #[test]
    fn avatica_lists_map_to_entries() {
        let props = vec![
            (
                "GET_NUMERIC_FUNCTIONS".to_string(),
                json!("ABS,ACOS, MOD ,ABS"),
            ),
            ("GET_STRING_FUNCTIONS".to_string(), json!("CONCAT,LOWER")),
            ("GET_TIME_DATE_FUNCTIONS".to_string(), json!("")),
            ("GET_S_Q_L_KEYWORDS".to_string(), json!("ABSOLUTE,ACTION")),
            ("GET_SYSTEM_FUNCTIONS".to_string(), json!(null)),
        ];
        let c = avatica(&props);
        let listed: Vec<_> = c
            .iter()
            .filter(|f| ["ABS", "ACOS", "MOD", "CONCAT", "LOWER"].contains(&f.name.as_str()))
            .collect();
        assert_eq!(listed.len(), 5, "each listed name once: {listed:?}");
        assert!(
            !c.iter().any(|f| f.name == "ABSOLUTE"),
            "keywords aren't functions"
        );
        assert!(c.iter().all(|f| f.description.is_empty()));
    }
}

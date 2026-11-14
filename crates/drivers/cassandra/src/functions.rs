//! The editor's completion (#97) for CQL: the built-in functions from a
//! static catalog per engine (CQL servers don't list them), filtered by
//! Cassandra's version, plus the user's functions and aggregates from
//! `system_schema.functions` / `system_schema.aggregates`, with their
//! keyspace as the schema. ScyllaDB has its own catalog, filtered by its
//! own versions (they don't follow Cassandra's); Amazon Keyspaces has the
//! scalar functions its guide lists and no user functions.

use crate::{text, texts};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use scylla::value::Row;

const CASSANDRA: &str = include_str!("functions/cassandra.txt");
const SCYLLA: &str = include_str!("functions/scylla.txt");
const KEYSPACES: &str = include_str!("functions/keyspaces.txt");

pub const USER_FUNCTIONS: &str = "SELECT keyspace_name, function_name, argument_names, argument_types, return_type FROM system_schema.functions";
pub const USER_AGGREGATES: &str = "SELECT keyspace_name, aggregate_name, argument_types, return_type FROM system_schema.aggregates";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Cassandra,
    Scylla,
    Keyspaces,
}

/// The built-ins of `engine` at its `version` (Cassandra's or ScyllaDB's
/// own, as `server_version` gives it).
pub fn builtins(engine: Engine, version: Option<&str>) -> Vec<FunctionInfo> {
    match engine {
        Engine::Cassandra => parse_catalog(CASSANDRA, version),
        Engine::Scylla => parse_catalog(SCYLLA, version),
        Engine::Keyspaces => parse_catalog(KEYSPACES, None),
    }
}

/// A `system_schema.functions` row ([`USER_FUNCTIONS`]).
pub fn user_function(r: &Row) -> FunctionInfo {
    let names = texts(r, 2);
    let types = texts(r, 3);
    let args: Vec<String> = types
        .iter()
        .enumerate()
        .map(|(i, t)| match names.get(i) {
            Some(n) => format!("{n} {t}"),
            None => t.clone(),
        })
        .collect();
    FunctionInfo::new(text(r, 1), FunctionKind::Scalar)
        .args(format!("({})", args.join(", ")))
        .returns(text(r, 4))
        .schema(text(r, 0))
}

/// A `system_schema.aggregates` row ([`USER_AGGREGATES`]).
pub fn user_aggregate(r: &Row) -> FunctionInfo {
    FunctionInfo::new(text(r, 1), FunctionKind::Aggregate)
        .args(format!("({})", texts(r, 2).join(", ")))
        .returns(text(r, 3))
        .schema(text(r, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::CqlValue;

    #[test]
    fn the_catalogs_parse_line_by_line() {
        for text in [CASSANDRA, SCYLLA, KEYSPACES] {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            }
        }
        let all = builtins(Engine::Cassandra, None);
        let cast = all.iter().find(|f| f.name == "cast").unwrap();
        assert_eq!(cast.args, "(value AS type)");
        assert!(all
            .iter()
            .any(|f| f.name == "avg" && f.kind == FunctionKind::Aggregate));
        assert!(all.iter().all(|f| f.description.is_empty()));
    }

    #[test]
    fn each_engine_has_its_own() {
        let has = |e, v, n: &str| builtins(e, v).iter().any(|f| f.name == n);
        // Cassandra 5.0 added math, masking and vector functions, and
        // dropped dateOf.
        assert!(has(
            Engine::Cassandra,
            Some("Apache Cassandra 5.0.4"),
            "similarity_cosine"
        ));
        assert!(!has(
            Engine::Cassandra,
            Some("Apache Cassandra 4.1.7"),
            "similarity_cosine"
        ));
        assert!(has(
            Engine::Cassandra,
            Some("Apache Cassandra 4.1.7"),
            "dateOf"
        ));
        assert!(!has(
            Engine::Cassandra,
            Some("Apache Cassandra 5.0.4"),
            "dateOf"
        ));
        assert!(has(
            Engine::Cassandra,
            Some("Apache Cassandra 4.1.7"),
            "toJson"
        ));
        assert!(!has(
            Engine::Cassandra,
            Some("Apache Cassandra 4.1.7"),
            "to_json"
        ));
        // The current* functions are 4.0's (CASSANDRA-13132), cast is
        // 3.2's (CASSANDRA-10310).
        assert!(!has(
            Engine::Cassandra,
            Some("Apache Cassandra 3.11.17"),
            "currentTimestamp"
        ));
        assert!(has(
            Engine::Cassandra,
            Some("Apache Cassandra 4.0.0"),
            "currentTimestamp"
        ));
        assert!(has(
            Engine::Cassandra,
            Some("Apache Cassandra 3.11.17"),
            "cast"
        ));
        assert!(!has(
            Engine::Cassandra,
            Some("Apache Cassandra 3.0.30"),
            "cast"
        ));
        assert!(has(Engine::Scylla, None, "dateOf"));
        assert!(!has(Engine::Scylla, None, "mask_inner"));
        assert!(has(
            Engine::Scylla,
            Some("ScyllaDB 2026.3.1-0.20260904"),
            "similarity_cosine"
        ));
        assert!(!has(
            Engine::Scylla,
            Some("ScyllaDB 6.2.3-0.20250119"),
            "similarity_cosine"
        ));
        assert!(has(Engine::Keyspaces, None, "toTimestamp"));
        assert!(!has(Engine::Keyspaces, None, "avg"));
    }

    fn t(s: &str) -> Option<CqlValue> {
        Some(CqlValue::Text(s.into()))
    }

    fn list(v: &[&str]) -> Option<CqlValue> {
        Some(CqlValue::List(
            v.iter().map(|s| CqlValue::Text(s.to_string())).collect(),
        ))
    }

    #[test]
    fn user_functions_map_with_their_keyspace() {
        let f = user_function(&Row {
            columns: vec![
                t("shop"),
                t("fahrenheit"),
                list(&["c", "digits"]),
                list(&["double", "int"]),
                t("double"),
            ],
        });
        assert_eq!(f.name, "fahrenheit");
        assert_eq!(f.args, "(c double, digits int)");
        assert_eq!(f.returns, "double");
        assert_eq!(f.schema, "shop");
        assert_eq!(f.kind, FunctionKind::Scalar);
        let a = user_aggregate(&Row {
            columns: vec![t("shop"), t("average"), list(&["int"]), t("double")],
        });
        assert_eq!(
            (
                a.name.as_str(),
                a.args.as_str(),
                a.returns.as_str(),
                a.schema.as_str()
            ),
            ("average", "(int)", "double", "shop")
        );
        assert_eq!(a.kind, FunctionKind::Aggregate);
    }
}

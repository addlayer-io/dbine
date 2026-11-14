//! The editor's completion (#97) for Cypher, per engine:
//! - Neo4j: `SHOW FUNCTIONS` (4.3+), or `dbms.functions()` before it: the
//!   built-ins and the installed plugins' functions (APOC, GDS…), with
//!   their signatures and the server's descriptions. When the server
//!   refuses both, a static catalog filtered by its version.
//! - Memgraph: its documented built-ins (static: the server doesn't list
//!   them), filtered by its version, plus `mg.functions()`, the query
//!   modules' functions.
//! - Amazon Neptune: the openCypher functions its guide lists (static),
//!   filtered by the engine release.
//!
//! A namespaced name (`date.truncate`, `apoc.coll.sum`) is split: the
//! namespace goes in the schema.

use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use serde_json::{Map, Value};

const NEO4J: &str = include_str!("functions/neo4j.txt");
const MEMGRAPH: &str = include_str!("functions/memgraph.txt");
const NEPTUNE: &str = include_str!("functions/neptune.txt");

pub const SHOW_FUNCTIONS: &str = "SHOW FUNCTIONS YIELD *";
pub const DBMS_FUNCTIONS: &str = "CALL dbms.functions()";
pub const MG_FUNCTIONS: &str = "CALL mg.functions() YIELD *";

/// `date.truncate` → `truncate` in `date`.
fn namespaced(mut f: FunctionInfo) -> FunctionInfo {
    if let Some((schema, name)) = f.name.rsplit_once('.') {
        f.schema = schema.to_string();
        f.name = name.to_string();
    }
    f
}

fn parse(text: &str, version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(text, version)
        .into_iter()
        .map(namespaced)
        .collect()
}

/// Neo4j's built-ins for a `version` (`server_version`'s `Neo4j 5.26.0
/// enterprise`), when the server lists none.
pub fn neo4j_static(version: Option<&str>) -> Vec<FunctionInfo> {
    // The 4 in "Neo4j" is not the version.
    let version = version.map(|v| v.trim_start_matches("Neo4j").trim_start());
    parse(NEO4J, version)
}

/// Memgraph's built-ins for a `version` (`server_version`'s `Memgraph
/// 3.13.1`).
pub fn memgraph_static(version: Option<&str>) -> Vec<FunctionInfo> {
    let version = version.map(|v| v.trim_start_matches("Memgraph").trim_start());
    parse(MEMGRAPH, version)
}

pub fn neptune_static(version: Option<&str>) -> Vec<FunctionInfo> {
    parse(NEPTUNE, version)
}

fn str_of<'a>(r: &'a Map<String, Value>, k: &str) -> &'a str {
    r.get(k).and_then(Value::as_str).unwrap_or("")
}

/// `name(a :: INTEGER, b :: STRING) :: FLOAT` → `(a :: INTEGER, b ::
/// STRING)` and `FLOAT` (4.x writes `:: (FLOAT?)`).
fn split_signature(signature: &str) -> (String, String) {
    let Some(open) = signature.find('(') else {
        return (String::new(), String::new());
    };
    let mut depth = 0;
    for (i, c) in signature[open..].char_indices() {
        match c {
            '(' | '[' | '<' | '{' => depth += 1,
            ')' | ']' | '>' | '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = open + i + 1;
                    let rest = signature[end..].trim_start();
                    let ret = rest.strip_prefix("::").unwrap_or("").trim();
                    let ret = ret
                        .strip_prefix('(')
                        .and_then(|r| r.strip_suffix(')'))
                        .unwrap_or(ret);
                    return (signature[open..end].to_string(), ret.trim().to_string());
                }
            }
            _ => {}
        }
    }
    (signature[open..].to_string(), String::new())
}

/// `SHOW FUNCTIONS YIELD *` / `dbms.functions()` rows: `name`,
/// `signature`, `description`, `aggregating` and (5.x)
/// `returnDescription`.
pub fn from_neo4j(rows: &[Map<String, Value>]) -> Vec<FunctionInfo> {
    rows.iter()
        .filter_map(|r| {
            let name = str_of(r, "name");
            if name.is_empty() {
                return None;
            }
            let (args, from_sig) = split_signature(str_of(r, "signature"));
            let returns = match str_of(r, "returnDescription") {
                "" => from_sig,
                d => d.to_string(),
            };
            let kind = if r.get("aggregating").and_then(Value::as_bool) == Some(true) {
                FunctionKind::Aggregate
            } else {
                FunctionKind::Scalar
            };
            Some(namespaced(
                FunctionInfo::new(name, kind)
                    .args(args)
                    .returns(returns)
                    .description(str_of(r, "description")),
            ))
        })
        .collect()
}

/// `mg.functions()` rows: `name` (`module.function`), `signature`
/// (`module.function(x :: INTEGER) :: STRING`), `is_editable`, `path`.
pub fn from_memgraph(rows: &[Map<String, Value>]) -> Vec<FunctionInfo> {
    rows.iter()
        .filter_map(|r| {
            let name = str_of(r, "name");
            if name.is_empty() {
                return None;
            }
            let (args, returns) = split_signature(str_of(r, "signature"));
            Some(namespaced(
                FunctionInfo::new(name, FunctionKind::Scalar)
                    .args(args)
                    .returns(returns),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(v: Value) -> Vec<Map<String, Value>> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_object().unwrap().clone())
            .collect()
    }

    #[test]
    fn the_catalogs_parse_line_by_line() {
        for text in [NEO4J, MEMGRAPH, NEPTUNE] {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            }
        }
        let n = neo4j_static(None);
        let t = n
            .iter()
            .find(|f| f.name == "truncate" && f.schema == "date")
            .unwrap();
        assert!(t.args.starts_with("(unit :: STRING"));
        assert_eq!(t.returns, "DATE");
        assert!(n
            .iter()
            .any(|f| f.name == "count" && f.kind == FunctionKind::Aggregate));
        let m = memgraph_static(None);
        assert!(m
            .iter()
            .any(|f| f.name == "distance" && f.schema == "point"));
        assert!(m
            .iter()
            .any(|f| f.name == "sum" && f.kind == FunctionKind::Aggregate));
        // project(path) and derive aggregate paths; project(nodes, edges)
        // doesn't.
        let kinds = |n: &str| {
            m.iter()
                .filter(|f| f.name == n)
                .map(|f| (f.args.starts_with("(row: path"), f.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds("project"),
            [
                (true, FunctionKind::Aggregate),
                (false, FunctionKind::Scalar)
            ]
        );
        assert_eq!(kinds("derive"), [(true, FunctionKind::Aggregate)]);
        assert!(neptune_static(None).iter().any(|f| f.name == "join"));
        for c in [&n, &m] {
            assert!(c.iter().all(|f| f.description.is_empty()));
        }
    }

    #[test]
    fn versions_filter_the_static_catalogs() {
        let has = |c: Vec<FunctionInfo>, n: &str| c.iter().any(|f| f.name == n);
        assert!(has(neo4j_static(Some("Neo4j 5.26.31 community")), "cosine"));
        assert!(!has(
            neo4j_static(Some("Neo4j 5.12.0 enterprise")),
            "cosine"
        ));
        assert!(!has(
            neo4j_static(Some("Neo4j 4.4.40 enterprise")),
            "elementId"
        ));
        assert!(has(
            neo4j_static(Some("Neo4j 4.4.40 enterprise")),
            "toLower"
        ));
        // Memgraph's release notes date them: nullIf 3.14, elementId 3.11,
        // username 3.8.
        let mg = |v| memgraph_static(Some(v));
        assert!(!has(mg("Memgraph 3.13.1"), "nullIf"));
        assert!(has(mg("Memgraph 3.14.0"), "nullIf"));
        assert!(has(mg("Memgraph 3.13.1"), "elementId"));
        assert!(!has(mg("Memgraph 3.10.1"), "elementId"));
        assert!(!has(mg("Memgraph 3.7.2"), "username"));
        assert!(has(mg("Memgraph 2.11.0"), "toUpper"));
        assert!(!has(mg("Memgraph 2.11.0"), "str2object"));
        assert!(has(
            neptune_static(Some("Amazon Neptune 1.4.5.0.R1")),
            "year"
        ));
        assert!(!has(
            neptune_static(Some("Amazon Neptune 1.3.2.1.R1")),
            "year"
        ));
        assert!(!has(
            neptune_static(Some("Amazon Neptune 1.3.2.1.R1")),
            "collToSet"
        ));
        assert!(has(
            neptune_static(Some("Amazon Neptune 1.3.2.1.R1")),
            "toUpper"
        ));
    }

    #[test]
    fn signatures_split_into_arguments_and_result() {
        assert_eq!(
            split_signature("abs(input :: INTEGER | FLOAT) :: INTEGER | FLOAT"),
            (
                "(input :: INTEGER | FLOAT)".into(),
                "INTEGER | FLOAT".into()
            )
        );
        assert_eq!(
            split_signature("apoc.coll.sum(coll :: LIST? OF NUMBER?) :: (FLOAT?)"),
            ("(coll :: LIST? OF NUMBER?)".into(), "FLOAT?".into())
        );
        assert_eq!(
            split_signature("vector.similarity.cosine(a :: LIST<INTEGER | FLOAT>, b :: LIST<INTEGER | FLOAT>) :: FLOAT"),
            ("(a :: LIST<INTEGER | FLOAT>, b :: LIST<INTEGER | FLOAT>)".into(), "FLOAT".into())
        );
    }

    #[test]
    fn server_rows_map() {
        let f = from_neo4j(&rows(json!([
            {"name": "abs", "category": "Numeric", "description": "Returns the absolute value of an `INTEGER` or `FLOAT`.",
             "signature": "abs(input :: INTEGER | FLOAT) :: INTEGER | FLOAT", "isBuiltIn": true,
             "returnDescription": "INTEGER | FLOAT", "aggregating": false},
            {"name": "apoc.coll.sum", "description": "Returns the sum of all the values.",
             "signature": "apoc.coll.sum(coll :: LIST? OF NUMBER?) :: (FLOAT?)", "aggregating": false},
            {"name": "apoc.agg.median", "signature": "apoc.agg.median(value :: ANY?) :: (ANY?)", "aggregating": true},
            {"category": "no name"}
        ])));
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].name.as_str(), f[0].schema.as_str()), ("abs", ""));
        assert_eq!(f[0].returns, "INTEGER | FLOAT");
        assert_eq!(
            f[0].description,
            "Returns the absolute value of an `INTEGER` or `FLOAT`."
        );
        assert_eq!(
            (f[1].name.as_str(), f[1].schema.as_str()),
            ("sum", "apoc.coll")
        );
        assert_eq!(f[1].args, "(coll :: LIST? OF NUMBER?)");
        assert_eq!(f[1].returns, "FLOAT?");
        assert_eq!(f[2].kind, FunctionKind::Aggregate);
        let m = from_memgraph(&rows(json!([
            {"name": "json_util.to_json", "signature": "json_util.to_json(value :: ANY) :: STRING", "is_editable": false}
        ])));
        assert_eq!(
            (m[0].name.as_str(), m[0].schema.as_str()),
            ("to_json", "json_util")
        );
        assert_eq!(
            (m[0].args.as_str(), m[0].returns.as_str()),
            ("(value :: ANY)", "STRING")
        );
    }
}

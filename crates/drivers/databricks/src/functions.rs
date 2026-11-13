//! The function catalog for the editor's completion (#97): `SHOW
//! FUNCTIONS`, one statement, which lists the built-ins and the user's
//! functions in the current schema (`catalog.schema.name`) by name only.
//! Kinds and arguments come from the static catalog of Databricks'
//! documentation; `DESCRIBE FUNCTION` would cost one statement per
//! function. A name the static catalog lacks is offered as a scalar
//! function. If `SHOW FUNCTIONS` fails, the static catalog alone.

use crate::DatabricksSession;
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

/// Databricks SQL's built-ins, from its documentation (signatures and
/// categories).
const DATABRICKS: &str = include_str!("functions/databricks.txt");

/// Operators `SHOW FUNCTIONS` lists by keyword: not called as `name(…)`.
const KEYWORDS: &[&str] = &[
    "and", "or", "not", "case", "when", "between", "in", "is", "like", "ilike", "rlike", "regexp",
];

impl DatabricksSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let fixed = parse_catalog(DATABRICKS, None);
        match self.run("SHOW FUNCTIONS", 100_000, None).await {
            Ok(st) => {
                let names: Vec<String> = st
                    .rows
                    .iter()
                    .filter_map(|r| r.first())
                    .map(|v| match v {
                        Json::String(s) => s.clone(),
                        v => v.to_string(),
                    })
                    .collect();
                let list = from_names(&names, &fixed);
                if !list.is_empty() {
                    return Ok(list);
                }
            }
            Err(e) => tracing::debug!("databricks: SHOW FUNCTIONS failed: {e}"),
        }
        Ok(fixed)
    }
}

/// `SHOW FUNCTIONS`' names as catalog entries: a built-in takes its
/// overloads from `fixed`; a qualified name is the user's, with its
/// `catalog.schema` (`system.builtin` is the built-ins' own).
pub(crate) fn from_names(names: &[String], fixed: &[FunctionInfo]) -> Vec<FunctionInfo> {
    let mut by_name: HashMap<String, Vec<&FunctionInfo>> = HashMap::new();
    for f in fixed {
        by_name
            .entry(f.name.to_ascii_lowercase())
            .or_default()
            .push(f);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for full in names {
        let (schema, name) = match full.rsplit_once('.') {
            Some((s, n)) if !s.eq_ignore_ascii_case("system.builtin") => (s, n),
            Some((_, n)) => ("", n),
            None => ("", full.as_str()),
        };
        let identifier = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let low = name.to_ascii_lowercase();
        if !identifier || KEYWORDS.contains(&low.as_str()) {
            continue;
        }
        if !seen.insert((schema.to_string(), low.clone())) {
            continue;
        }
        match by_name.get(&low).filter(|_| schema.is_empty()) {
            Some(known) => out.extend(known.iter().map(|f| (*f).clone())),
            None => out.push(FunctionInfo::new(name, FunctionKind::Scalar).schema(schema)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in DATABRICKS.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(n > 400, "{n}");
        let list = parse_catalog(DATABRICKS, None);
        let kind = |name: &str| list.iter().find(|f| f.name == name).map(|f| f.kind);
        assert_eq!(kind("approx_count_distinct"), Some(FunctionKind::Aggregate));
        assert_eq!(kind("row_number"), Some(FunctionKind::Window));
        assert_eq!(kind("read_files"), Some(FunctionKind::Table));
        assert_eq!(kind("abs"), Some(FunctionKind::Scalar));
        assert_eq!(kind("approx_distinct"), None);
        let kinds = |name: &str| {
            list.iter()
                .filter(|f| f.name == name)
                .map(|f| f.kind)
                .collect::<Vec<_>>()
        };
        // Generators go in the select list and in FROM.
        for g in [
            "explode",
            "posexplode_outer",
            "inline",
            "json_tuple",
            "stack",
        ] {
            assert_eq!(kinds(g), [FunctionKind::Scalar, FunctionKind::Table], "{g}");
        }
        // Table-valued only.
        for t in ["variant_explode", "table_changes", "range", "vector_search"] {
            assert!(kinds(t).iter().all(|k| *k == FunctionKind::Table), "{t}");
        }
        assert_eq!(kinds("vector_search").len(), 2);
    }

    #[test]
    fn server_names_map_into_entries() {
        let fixed = parse_catalog(DATABRICKS, None);
        let names: Vec<String> = [
            "!",
            "%",
            "abs",
            "approx_count_distinct",
            "and",
            "range",
            "h3_newfuture",
            "system.builtin.lower",
            "main.sales.twice",
            "main.sales.twice",
            "my_temp_fn",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let list = from_names(&names, &fixed);
        let find = |n: &str| list.iter().filter(|f| f.name == n).collect::<Vec<_>>();
        assert_eq!(find("abs")[0].args, "(expr)");
        assert_eq!(
            find("approx_count_distinct")[0].kind,
            FunctionKind::Aggregate
        );
        assert_eq!(find("range").len(), 2);
        assert!(find("range").iter().all(|f| f.kind == FunctionKind::Table));
        assert_eq!(find("h3_newfuture")[0].kind, FunctionKind::Scalar);
        assert!(find("lower")[0].schema.is_empty());
        assert_eq!(find("twice").len(), 1);
        assert_eq!(find("twice")[0].schema, "main.sales");
        assert!(find("my_temp_fn")[0].schema.is_empty());
        assert!(find("and").is_empty() && find("!").is_empty() && find("%").is_empty());
    }
}

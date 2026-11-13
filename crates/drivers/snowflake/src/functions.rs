//! The function catalog for the editor's completion (#97): `SHOW
//! FUNCTIONS`, which lists the built-ins and the user's functions in scope
//! (the current database's, or the account's), one row per overload with
//! its signature (`ABS(NUMBER) RETURN NUMBER`), whether it is an aggregate
//! or a table function, and the server's description. If it fails, the
//! static catalog from Snowflake's documentation (names and kinds only).

use crate::{monitor, SnowflakeSession};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use std::collections::HashSet;

/// Snowflake's built-ins, from its documentation's list of all functions.
const SNOWFLAKE: &str = include_str!("functions/snowflake.txt");

/// The tag on DBine's own `SHOW FUNCTIONS` (QUERY_TAG), as the monitor and
/// profiler tag theirs.
const TAG: &str = "dbine-functions";

/// Window functions, which `SHOW FUNCTIONS` doesn't mark.
const WINDOW: &[&str] = &[
    "ROW_NUMBER",
    "RANK",
    "DENSE_RANK",
    "PERCENT_RANK",
    "CUME_DIST",
    "NTILE",
    "LAG",
    "LEAD",
    "FIRST_VALUE",
    "LAST_VALUE",
    "NTH_VALUE",
    "RATIO_TO_REPORT",
    "CONDITIONAL_CHANGE_EVENT",
    "CONDITIONAL_TRUE_EVENT",
];

impl SnowflakeSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        match self.tagged_rows("SHOW FUNCTIONS", TAG).await {
            Ok(set) => {
                let list = from_show_functions(&set);
                if !list.is_empty() {
                    return Ok(list);
                }
            }
            Err(e) => tracing::debug!("snowflake: SHOW FUNCTIONS failed: {e}"),
        }
        Ok(built_ins())
    }
}

/// The static catalog, with namespaced names split into schema and name.
pub(crate) fn built_ins() -> Vec<FunctionInfo> {
    parse_catalog(SNOWFLAKE, None)
        .into_iter()
        .map(|f| match f.name.rsplit_once('.') {
            Some((ns, name)) => {
                let (ns, name) = (ns.to_string(), name.to_string());
                FunctionInfo { name, ..f }.schema(ns)
            }
            None => f,
        })
        .collect()
}

/// `SHOW FUNCTIONS`' rows (lowercase column names). A user's function
/// (`is_builtin` = N) gets `database.schema` as its schema.
pub(crate) fn from_show_functions(set: &monitor::Set) -> Vec<FunctionInfo> {
    let idx = |name: &str| set.cols.iter().position(|c| c == name);
    let cols = [
        "name",
        "schema_name",
        "is_builtin",
        "is_aggregate",
        "arguments",
        "description",
        "catalog_name",
        "is_table_function",
    ]
    .map(idx);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in &set.rows {
        let get = |i: usize| {
            cols[i]
                .and_then(|c| row.get(c))
                .cloned()
                .flatten()
                .unwrap_or_default()
        };
        let name = get(0);
        if name.is_empty() {
            continue;
        }
        let kind = if get(7) == "Y" {
            FunctionKind::Table
        } else if WINDOW.contains(&name.to_ascii_uppercase().as_str()) {
            FunctionKind::Window
        } else if get(3) == "Y" {
            FunctionKind::Aggregate
        } else {
            FunctionKind::Scalar
        };
        let (args, returns) = signature(&name, &get(4));
        let schema = if get(2) == "N" {
            [get(6), get(1)]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(".")
        } else {
            String::new()
        };
        if !seen.insert((name.clone(), args.clone(), schema.clone())) {
            continue;
        }
        out.push(
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(returns)
                .description(get(5))
                .schema(schema),
        );
    }
    out
}

/// `ABS(NUMBER) RETURN NUMBER` → (`(NUMBER)`, `NUMBER`); `MY_TF(NUMBER)
/// RETURN TABLE (A NUMBER)` keeps `TABLE (A NUMBER)` as the result.
pub(crate) fn signature(name: &str, arguments: &str) -> (String, String) {
    let text = arguments.trim();
    let rest = text
        .get(..name.len())
        .filter(|head| head.eq_ignore_ascii_case(name))
        .map(|_| &text[name.len()..])
        .unwrap_or(text);
    let Some(open) = rest.find('(') else {
        return (String::new(), String::new());
    };
    let mut depth = 0;
    let mut close = None;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return (rest[open..].to_string(), String::new());
    };
    let args = rest[open..=close].to_string();
    let after = rest[close + 1..].trim();
    let returns = after
        .strip_prefix("RETURN ")
        .or_else(|| after.strip_prefix("RETURNS "))
        .unwrap_or("")
        .trim()
        .to_string();
    (args, returns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(rows: Vec<[&str; 8]>) -> monitor::Set {
        monitor::Set {
            cols: [
                "created_on",
                "name",
                "schema_name",
                "is_builtin",
                "is_aggregate",
                "arguments",
                "description",
                "catalog_name",
                "is_table_function",
            ]
            .iter()
            .map(|c| c.to_string())
            .collect(),
            rows: rows
                .into_iter()
                .map(|r| {
                    std::iter::once(None)
                        .chain(r.iter().map(|c| Some(c.to_string())))
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in SNOWFLAKE.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(n > 500, "{n}");
        let list = built_ins();
        let find = |name: &str| list.iter().find(|f| f.name == name);
        assert_eq!(
            find("APPROX_COUNT_DISTINCT").unwrap().kind,
            FunctionKind::Aggregate
        );
        assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
        assert_eq!(find("FLATTEN").unwrap().kind, FunctionKind::Table);
        assert!(find("approx_distinct").is_none());
        // Rows naming several functions, and families, give each one.
        for name in [
            "SUBSTR",
            "SUBSTRING",
            "LENGTH",
            "LEN",
            "CAST",
            "TO_DATE",
            "TO_TIMESTAMP_NTZ",
            "TRY_TO_NUMBER",
            "HOUR",
            "DAYOFWEEKISO",
            "SEQ8",
            "IS_TIMESTAMP_TZ",
            "MD5",
        ] {
            assert_eq!(
                find(name).map(|f| f.kind),
                Some(FunctionKind::Scalar),
                "{name}"
            );
        }
        for name in ["STDDEV", "STDDEV_SAMP", "VARIANCE_SAMP", "ST_UNION_AGG"] {
            assert_eq!(find(name).unwrap().kind, FunctionKind::Aggregate, "{name}");
        }
        // Readers of an aggregate's state are scalar.
        for name in ["HLL_ESTIMATE", "BITMAP_COUNT", "APPROX_TOP_K_ESTIMATE"] {
            assert_eq!(find(name).unwrap().kind, FunctionKind::Scalar, "{name}");
        }
        // Namespaced: Cortex's and the data metric functions.
        let complete = find("COMPLETE").unwrap();
        assert_eq!(complete.schema, "SNOWFLAKE.CORTEX");
        assert!(list
            .iter()
            .any(|f| f.name == "NULL_COUNT" && f.schema == "SNOWFLAKE.CORE"));
        // No leftovers of the list's labels.
        assert!(list.iter().all(|f| f
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')));
    }

    #[test]
    fn server_rows_map_into_entries() {
        let list = from_show_functions(&set(vec![
            [
                "ABS",
                "",
                "Y",
                "N",
                "ABS(NUMBER) RETURN NUMBER",
                "Returns the absolute value of a numeric expression.",
                "",
                "N",
            ],
            [
                "APPROX_COUNT_DISTINCT",
                "",
                "Y",
                "Y",
                "APPROX_COUNT_DISTINCT(ANY) RETURN NUMBER",
                "",
                "",
                "N",
            ],
            [
                "ROW_NUMBER",
                "",
                "Y",
                "N",
                "ROW_NUMBER() RETURN NUMBER",
                "",
                "",
                "N",
            ],
            [
                "FLATTEN",
                "",
                "Y",
                "N",
                "FLATTEN(VARIANT) RETURN TABLE (SEQ NUMBER, VALUE VARIANT)",
                "",
                "",
                "Y",
            ],
            [
                "TWICE",
                "PUBLIC",
                "N",
                "N",
                "TWICE(NUMBER, VARCHAR DEFAULT 'x') RETURN NUMBER",
                "user-defined function",
                "SALES",
                "N",
            ],
            [
                "TWICE",
                "PUBLIC",
                "N",
                "N",
                "TWICE(NUMBER, VARCHAR DEFAULT 'x') RETURN NUMBER",
                "user-defined function",
                "SALES",
                "N",
            ],
        ]));
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].args, "(NUMBER)");
        assert_eq!(list[0].returns, "NUMBER");
        assert_eq!(
            list[0].description,
            "Returns the absolute value of a numeric expression."
        );
        assert!(list[0].schema.is_empty());
        assert_eq!(list[1].kind, FunctionKind::Aggregate);
        assert_eq!(list[2].kind, FunctionKind::Window);
        assert_eq!(list[2].args, "()");
        assert_eq!(list[3].kind, FunctionKind::Table);
        assert_eq!(list[3].returns, "TABLE (SEQ NUMBER, VALUE VARIANT)");
        assert_eq!(list[4].schema, "SALES.PUBLIC");
        assert_eq!(list[4].args, "(NUMBER, VARCHAR DEFAULT 'x')");
        assert_eq!(signature("X", "nonsense"), (String::new(), String::new()));
    }
}

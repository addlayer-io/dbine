//! The function catalog for the editor's completion (#97): Athena's engine
//! version 3 runs Trino, so `SHOW FUNCTIONS` gives each overload's types,
//! kind and description. If the workgroup refuses it, the static catalog of
//! Trino's built-ins, which are the functions Athena documents for engine
//! version 3.

use crate::{AthenaSession, Execution};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use std::collections::{HashMap, HashSet};

/// Trino's built-ins (scalar, aggregate and window), generated from `SHOW
/// FUNCTIONS` on Trino 483.
const ATHENA: &str = include_str!("functions/athena.txt");

impl AthenaSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        match self.run("SHOW FUNCTIONS", 100_000).await {
            Ok(ex) => {
                let list = from_show_functions(&ex);
                if !list.is_empty() {
                    return Ok(list);
                }
            }
            Err(e) => tracing::debug!("athena: SHOW FUNCTIONS failed: {e}"),
        }
        Ok(parse_catalog(ATHENA, None))
    }
}

/// `SHOW FUNCTIONS`' rows, by column name: one entry per overload, exact
/// repeats dropped. A qualified name (`catalog.schema.name`) gets its
/// prefix as the schema.
pub(crate) fn from_show_functions(ex: &Execution) -> Vec<FunctionInfo> {
    let columns: HashMap<&str, usize> = ex
        .columns
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (name.as_str(), i))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in &ex.rows {
        let get = |name: &str| {
            columns
                .get(name)
                .and_then(|&i| row.get(i))
                .and_then(|c| c.clone())
                .unwrap_or_default()
        };
        let full = get("Function");
        let (schema, name) = match full.rsplit_once('.') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), full.clone()),
        };
        if name.is_empty() || name.starts_with('$') {
            continue;
        }
        let kind = match get("Function Type").as_str() {
            "aggregate" => FunctionKind::Aggregate,
            "window" => FunctionKind::Window,
            "table" => FunctionKind::Table,
            _ => FunctionKind::Scalar,
        };
        let args = format!("({})", get("Argument Types"));
        let returns = get("Return Type");
        let returns = if kind == FunctionKind::Table && returns == "unknown" {
            String::new()
        } else {
            returns
        };
        if !seen.insert((
            name.clone(),
            kind,
            args.clone(),
            returns.clone(),
            schema.clone(),
        )) {
            continue;
        }
        out.push(
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(returns)
                .description(get("Description"))
                .schema(schema),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(columns: &[&str], rows: Vec<Vec<&str>>) -> Execution {
        Execution {
            columns: columns
                .iter()
                .map(|c| (c.to_string(), "varchar".to_string()))
                .collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(|c| Some(c.to_string())).collect())
                .collect(),
            update_count: None,
            more: false,
            has_result_set: true,
        }
    }

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in ATHENA.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(n > 300, "{n}");
        let list = parse_catalog(ATHENA, None);
        assert!(list
            .iter()
            .any(|f| f.name == "approx_distinct" && f.kind == FunctionKind::Aggregate));
        assert!(list
            .iter()
            .any(|f| f.name == "row_number" && f.kind == FunctionKind::Window));
        assert!(!list.iter().any(|f| f.name == "uniq"));
    }

    #[test]
    fn server_rows_map_into_entries() {
        let list = from_show_functions(&ex(
            &[
                "Function",
                "Return Type",
                "Argument Types",
                "Function Type",
                "Deterministic",
                "Description",
            ],
            vec![
                vec![
                    "approx_distinct",
                    "bigint",
                    "T, double",
                    "aggregate",
                    "true",
                    "",
                ],
                vec![
                    "approx_distinct",
                    "bigint",
                    "T, double",
                    "aggregate",
                    "true",
                    "",
                ],
                vec![
                    "date_diff",
                    "bigint",
                    "varchar(x), date, date",
                    "scalar",
                    "true",
                    "Difference of the given dates in the given unit",
                ],
                vec!["rank", "bigint", "", "window", "true", ""],
                vec![
                    "awsdatacatalog.sales.twice",
                    "bigint",
                    "bigint",
                    "scalar",
                    "true",
                    "",
                ],
            ],
        ));
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].kind, FunctionKind::Aggregate);
        assert_eq!(list[0].args, "(T, double)");
        assert_eq!(list[0].returns, "bigint");
        assert_eq!(
            list[1].description,
            "Difference of the given dates in the given unit"
        );
        assert_eq!(list[2].kind, FunctionKind::Window);
        assert_eq!(list[3].name, "twice");
        assert_eq!(list[3].schema, "awsdatacatalog.sales");

        // A one-column answer (the DDL engine's) maps to nothing: the
        // static catalog is used instead.
        assert!(from_show_functions(&ex(&["tab_name"], vec![vec!["abs"]])).is_empty());
    }
}

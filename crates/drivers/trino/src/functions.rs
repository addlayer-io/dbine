//! The function catalog for the editor's completion (#97): `SHOW FUNCTIONS`,
//! which gives each overload's argument and return types, its kind and the
//! server's description, plus `SHOW FUNCTIONS FROM catalog.schema` for the
//! user's SQL routines in the session's schema (Trino lists those only
//! there). If the read fails, the static catalog of the flavor.

use crate::{text, Flavor, TrinoSession};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{QueryOutcome, Result, Session, StatementResult};
use std::collections::{HashMap, HashSet};

/// Trino's built-ins, generated from `SHOW FUNCTIONS` on Trino 483
/// (Starburst runs Trino).
const TRINO: &str = include_str!("functions/trino.txt");
/// Presto's built-ins, generated from `SHOW FUNCTIONS` on Presto.
const PRESTO: &str = include_str!("functions/presto.txt");

impl TrinoSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let mut list = match self.result_set("SHOW FUNCTIONS").await {
            Ok(set) => from_show_functions(&set, ""),
            Err(e) => {
                tracing::debug!("trino: SHOW FUNCTIONS failed: {e}");
                Vec::new()
            }
        };
        if list.is_empty() {
            let version = self.server_version().await.ok();
            let text = match self.flavor {
                Flavor::Presto => PRESTO,
                Flavor::Trino | Flavor::Starburst => TRINO,
            };
            list = parse_catalog(text, version.as_deref());
        }
        // Inside an open transaction a refused statement would abort it.
        let in_transaction = self.state.transaction.is_some();
        if let (Some(catalog), Some(schema), false) = (
            self.state.catalog.clone(),
            self.state.schema.clone(),
            in_transaction,
        ) {
            let sql = format!(
                "SHOW FUNCTIONS FROM {}.{}",
                quote_ident(Quote::Double, &catalog),
                quote_ident(Quote::Double, &schema)
            );
            // Catalogs without routines refuse it; there is nothing to add.
            match self.result_set(&sql).await {
                Ok(set) => list.extend(from_show_functions(&set, &format!("{catalog}.{schema}"))),
                Err(e) => tracing::debug!("trino: {sql} failed: {e}"),
            }
        }
        Ok(list)
    }

    /// One catalog query's result set, with its column names.
    async fn result_set(&mut self, sql: &str) -> Result<StatementResult> {
        let mut out = QueryOutcome::default();
        self.run(sql, 100_000, &mut out).await?;
        Ok(out.results.pop().unwrap_or_default())
    }
}

/// `SHOW FUNCTIONS`' rows, by column name (Presto adds columns after
/// Trino's six): one entry per overload, exact repeats dropped. `schema` is
/// where they live when they aren't built-ins; a name Presto qualifies
/// (`catalog.schema.name`) gets its prefix as the schema.
pub(crate) fn from_show_functions(set: &StatementResult, schema: &str) -> Vec<FunctionInfo> {
    let columns: HashMap<&str, usize> = set
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in &set.rows {
        let get = |name: &str| {
            columns
                .get(name)
                .and_then(|&i| row.get(i))
                .map(text)
                .unwrap_or_default()
        };
        let full = get("Function");
        let (prefix, name) = match full.rsplit_once('.') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (schema.to_string(), full.clone()),
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
            prefix.clone(),
        )) {
            continue;
        }
        out.push(
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(returns)
                .description(get("Description"))
                .schema(prefix),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ResultColumn;
    use serde_json::{json, Value};

    fn set(columns: &[&str], rows: Vec<Vec<Value>>) -> StatementResult {
        StatementResult {
            columns: columns
                .iter()
                .map(|c| ResultColumn {
                    name: c.to_string(),
                    type_name: String::new(),
                })
                .collect(),
            rows,
            ..Default::default()
        }
    }

    const TRINO_COLUMNS: [&str; 6] = [
        "Function",
        "Return Type",
        "Argument Types",
        "Function Type",
        "Deterministic",
        "Description",
    ];

    #[test]
    fn the_static_catalogs_parse() {
        for text in [TRINO, PRESTO] {
            let mut n = 0;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                n += 1;
                assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            }
            assert!(n > 300, "{n}");
        }
        let trino = parse_catalog(TRINO, None);
        assert!(trino
            .iter()
            .any(|f| f.name == "approx_distinct" && f.kind == FunctionKind::Aggregate));
        assert!(trino
            .iter()
            .any(|f| f.name == "row_number" && f.kind == FunctionKind::Window));
    }

    #[test]
    fn server_rows_map_into_entries() {
        let rows = set(
            &TRINO_COLUMNS,
            vec![
                vec![
                    json!("approx_distinct"),
                    json!("bigint"),
                    json!("T, double"),
                    json!("aggregate"),
                    json!(true),
                    json!(""),
                ],
                vec![
                    json!("date_diff"),
                    json!("bigint"),
                    json!("varchar(x), date, date"),
                    json!("scalar"),
                    json!(true),
                    json!("Difference of the given dates in the given unit"),
                ],
                vec![
                    json!("row_number"),
                    json!("bigint"),
                    json!(""),
                    json!("window"),
                    json!(true),
                    json!(""),
                ],
                vec![
                    json!("exclude_columns"),
                    json!("unknown"),
                    json!("unknown, unknown"),
                    json!("table"),
                    json!(false),
                    json!(""),
                ],
                vec![
                    json!("exclude_columns"),
                    json!("unknown"),
                    json!("unknown, unknown"),
                    json!("table"),
                    json!(false),
                    json!(""),
                ],
            ],
        );
        let list = from_show_functions(&rows, "");
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].kind, FunctionKind::Aggregate);
        assert_eq!(list[0].args, "(T, double)");
        assert_eq!(list[0].returns, "bigint");
        assert_eq!(
            list[1].description,
            "Difference of the given dates in the given unit"
        );
        assert_eq!(list[2].kind, FunctionKind::Window);
        assert_eq!(list[2].args, "()");
        assert_eq!(list[3].kind, FunctionKind::Table);
        assert_eq!(list[3].returns, "");
        assert!(list.iter().all(|f| f.schema.is_empty()));

        let routines = set(
            &TRINO_COLUMNS,
            vec![vec![
                json!("twice"),
                json!("bigint"),
                json!("bigint"),
                json!("scalar"),
                json!(true),
                json!("twice x"),
            ]],
        );
        let list = from_show_functions(&routines, "memory.sales");
        assert_eq!(list[0].schema, "memory.sales");
        assert_eq!(list[0].name, "twice");

        // Presto qualifies SQL functions and adds columns.
        let presto = set(
            &[
                "Function",
                "Return Type",
                "Argument Types",
                "Function Type",
                "Deterministic",
                "Description",
                "Variable Arity",
                "Built In",
            ],
            vec![vec![
                json!("example.default.tan"),
                json!("double"),
                json!("double"),
                json!("scalar"),
                json!(true),
                json!("tangent"),
                json!(false),
                json!(false),
            ]],
        );
        let list = from_show_functions(&presto, "");
        assert_eq!(list[0].name, "tan");
        assert_eq!(list[0].schema, "example.default");
    }
}

//! The function catalog for the editor's completion (#97), read from
//! `system.functions` and `system.table_functions`: built-ins with the
//! server's own documentation (23.x and later), aliases, and the user's SQL
//! functions (`CREATE FUNCTION`, global, so without a schema). If the read
//! fails, the static catalog of the flavor, filtered by the server's version.

use crate::{read_rows, text, Body, ClickHouseSession, Flavor};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::{QueryOutcome, Result, Session, StatementResult};
use serde_json::Value;
use std::collections::HashMap;

/// ClickHouse's built-ins with the version that added them, generated from
/// a current server's `system.functions` (its `introduced_in`), which the
/// documentation is built from.
const CLICKHOUSE: &str = include_str!("functions/clickhouse.txt");
/// Timeplus Proton's built-ins, from its `system.functions`.
const PROTON: &str = include_str!("functions/proton.txt");

/// Only the columns this server has: `description`, `syntax`,
/// `returned_value` and `categories` came in 23.x, `origin` and
/// `create_query` in 21.x; the `COLUMNS` matcher skips the missing ones, so
/// one query serves every version.
const FUNCTIONS_SQL: &str = "SELECT COLUMNS('^(name|is_aggregate|alias_to|origin|create_query|syntax|returned_value|description|categories)$') FROM system.functions";
const TABLE_FUNCTIONS_SQL: &str =
    "SELECT COLUMNS('^(name|description)$') FROM system.table_functions";

/// Window functions, which `system.functions` lists as aggregates.
const WINDOW: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lagInFrame",
    "leadInFrame",
    // Proton's names.
    "lag_in_frame",
    "lead_in_frame",
    "nth_value",
    "lag",
    "lead",
];

impl ClickHouseSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        match self.server_functions().await {
            Ok(list) if !list.is_empty() => return Ok(list),
            Ok(_) => tracing::debug!("clickhouse: system.functions is empty"),
            Err(e) => tracing::debug!("clickhouse: system.functions unreadable: {e}"),
        }
        let version = self.server_version().await.ok();
        let text = match self.flavor {
            Flavor::ClickHouse => CLICKHOUSE,
            Flavor::Timeplus => PROTON,
        };
        Ok(parse_catalog(text, version.as_deref()))
    }

    async fn server_functions(&self) -> Result<Vec<FunctionInfo>> {
        let functions = self.result_set(FUNCTIONS_SQL).await?;
        let mut list = from_functions(&functions);
        // Older servers may refuse `system.table_functions`' matcher; the
        // functions alone are still worth offering.
        match self.result_set(TABLE_FUNCTIONS_SQL).await {
            Ok(tables) => list.extend(from_table_functions(&tables)),
            Err(e) => tracing::debug!("clickhouse: system.table_functions unreadable: {e}"),
        }
        Ok(list)
    }

    /// One catalog query's result set, with its column names.
    async fn result_set(&self, sql: &str) -> Result<StatementResult> {
        let mut out = QueryOutcome::default();
        let sent = self.send_with(sql, &[], false, &[], None).await;
        let read = match sent {
            Ok(Body::Rows(resp)) => read_rows(resp, &mut out, 100_000, false).await.map(|_| ()),
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        self.done();
        read?;
        Ok(out.results.pop().unwrap_or_default())
    }
}

/// A row's cells by column name.
struct Row<'a> {
    columns: &'a HashMap<&'a str, usize>,
    cells: &'a [Value],
}

impl Row<'_> {
    fn get(&self, column: &str) -> String {
        self.columns
            .get(column)
            .and_then(|&i| self.cells.get(i))
            .map(text)
            .unwrap_or_default()
    }

    fn flag(&self, column: &str) -> bool {
        matches!(self.get(column).as_str(), "1" | "true")
    }
}

fn column_index(set: &StatementResult) -> HashMap<&str, usize> {
    set.columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect()
}

/// `system.functions`' rows as catalog entries. Internal functions
/// (`_CAST`, `__getScalar`, the `Internal` category) are left out; an alias
/// with no documentation of its own takes its target's.
pub(crate) fn from_functions(set: &StatementResult) -> Vec<FunctionInfo> {
    let columns = column_index(set);
    let mut out: Vec<FunctionInfo> = Vec::new();
    let mut aliases: Vec<(usize, String)> = Vec::new();
    for cells in &set.rows {
        let row = Row {
            columns: &columns,
            cells,
        };
        let name = row.get("name");
        if name.is_empty() || name.starts_with('_') || row.get("categories") == "Internal" {
            continue;
        }
        let kind = if WINDOW.contains(&name.as_str()) {
            FunctionKind::Window
        } else if row.flag("is_aggregate") {
            FunctionKind::Aggregate
        } else {
            FunctionKind::Scalar
        };
        let mut f = FunctionInfo::new(name.clone(), kind);
        if row.get("origin") == "SQLUserDefined" {
            let query = row.get("create_query");
            f = f.args(udf_args(&query)).description(query);
        } else {
            f = f
                .args(syntax_args(&name, &row.get("syntax")))
                .returns(returned_types(&row.get("returned_value")))
                .description(first_paragraph(&row.get("description")));
        }
        let alias_to = row.get("alias_to");
        if !alias_to.is_empty() {
            aliases.push((out.len(), alias_to));
        }
        out.push(f);
    }
    let by_name: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.clone(), i))
        .collect();
    for (i, target) in aliases {
        let Some(&t) = by_name.get(&target) else {
            continue;
        };
        let (args, returns, description) = (
            out[t].args.clone(),
            out[t].returns.clone(),
            out[t].description.clone(),
        );
        let f = &mut out[i];
        if f.args.is_empty() {
            f.args = args;
        }
        if f.returns.is_empty() {
            f.returns = returns;
        }
        if f.description.is_empty() {
            f.description = description;
        }
    }
    out
}

/// `system.table_functions`' rows: `numbers`, `s3`, `remote`…
pub(crate) fn from_table_functions(set: &StatementResult) -> Vec<FunctionInfo> {
    let columns = column_index(set);
    set.rows
        .iter()
        .filter_map(|cells| {
            let row = Row {
                columns: &columns,
                cells,
            };
            let name = row.get("name");
            (!name.is_empty() && !name.starts_with('_')).then(|| {
                FunctionInfo::new(name, FunctionKind::Table)
                    .description(first_paragraph(&row.get("description")))
            })
        })
        .collect()
}

/// The arguments in `system.functions.syntax`: `dateDiff(unit, startdate,
/// enddate[, timezone])` → `(unit, startdate, enddate[, timezone])`,
/// `quantile(level)(expr)` → `(level)(expr)`. The first line that calls the
/// function (some show several modes, after `--` comments), without a
/// trailing `-- comment`; the call may use another of its names (`ceil`
/// documents `ceiling(x[, N])`).
pub(crate) fn syntax_args(name: &str, syntax: &str) -> String {
    let lines = || {
        syntax
            .lines()
            .map(|l| strip_comment(l).trim())
            .filter(|l| !l.is_empty())
    };
    let own = lines().find_map(|l| {
        let at = l.find(name)?;
        let rest = &l[at + name.len()..];
        (rest.starts_with('(') || rest.starts_with("[(")).then_some(rest)
    });
    let args = own.or_else(|| {
        lines().find_map(|l| {
            let open = l.find('(')?;
            let head = &l[..open];
            (!head.is_empty() && head.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .then_some(&l[open..])
        })
    });
    args.map(|a| a.trim().trim_end_matches(';').to_string())
        .unwrap_or_default()
}

/// `line` up to its `--` comment, if it has one outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut prev = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '-') if prev == Some('-') => return &line[..i - 1],
            _ => {}
        }
        prev = Some(c);
    }
    line
}

/// The types `returned_value` links to, as the documentation names them:
/// ``Returns … [`Int64`](/reference/…)`` → `Int64`; several are joined
/// with ` or `.
pub(crate) fn returned_types(returned: &str) -> String {
    let mut types: Vec<&str> = Vec::new();
    let mut rest = returned;
    while let Some(at) = rest.find("[`") {
        let after = &rest[at + 2..];
        let Some(end) = after.find("`]") else { break };
        let t = &after[..end];
        if !t.is_empty() && !types.contains(&t) {
            types.push(t);
        }
        rest = &after[end + 2..];
    }
    types.join(" or ")
}

/// The first paragraph of the server's description, its own words, on one
/// line. Documentation imports and markup lines (`import …`, `<Note>`) are
/// skipped.
pub(crate) fn first_paragraph(description: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in description.lines().map(str::trim) {
        if line.is_empty() {
            if out.is_empty() {
                continue;
            }
            break;
        }
        if line.starts_with("import ") || line.starts_with('<') {
            continue;
        }
        out.push(line);
    }
    out.join(" ")
}

/// The parameters of `CREATE FUNCTION name AS (x, k, b) -> …`, or of the
/// one-parameter form `AS x -> …`.
pub(crate) fn udf_args(create_query: &str) -> String {
    let upper = create_query.to_ascii_uppercase();
    let Some(at) = upper.find(" AS ") else {
        return String::new();
    };
    let rest = create_query[at + 4..].trim_start();
    if rest.starts_with('(') {
        let Some(end) = rest.find(')') else {
            return String::new();
        };
        rest[..=end].to_string()
    } else {
        let Some(arrow) = rest.find("->") else {
            return String::new();
        };
        format!("({})", rest[..arrow].trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ResultColumn;
    use serde_json::json;

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

    #[test]
    fn the_static_catalogs_parse() {
        for text in [CLICKHOUSE, PROTON] {
            let mut n = 0;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                n += 1;
                let parsed = parse_catalog(line, None);
                assert_eq!(parsed.len(), 1, "unreadable: {line}");
                assert!(
                    parsed[0]
                        .name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_'),
                    "bad name: {line}"
                );
                assert!(
                    !parsed[0].args.contains("--"),
                    "comment in the arguments: {line}"
                );
            }
            assert!(n > 100, "{n}");
        }
        let names = |v| -> Vec<String> {
            parse_catalog(CLICKHOUSE, v)
                .into_iter()
                .map(|f| f.name)
                .collect()
        };
        let has = |v, name: &str| names(v).iter().any(|n| n == name);
        assert!(has(Some("ClickHouse 22.8.21.38"), "toStartOfDay"));
        assert!(!has(Some("ClickHouse 22.8.21.38"), "mortonEncode"));
        assert!(has(Some("ClickHouse 24.6.1"), "mortonEncode"));
        assert!(has(None, "uniq"));
    }

    #[test]
    fn server_rows_map_into_entries() {
        let rows = set(
            &[
                "name",
                "is_aggregate",
                "alias_to",
                "origin",
                "create_query",
                "syntax",
                "returned_value",
                "description",
                "categories",
            ],
            vec![
                vec![
                    json!("dateDiff"),
                    json!(0),
                    json!(""),
                    json!("System"),
                    json!(""),
                    json!("dateDiff(unit, startdate, enddate[, timezone])"),
                    json!("Returns the difference. [`Int64`](/reference/data-types/int-uint)"),
                    json!(
                        "\nReturns the count of the specified `unit` boundaries\ncrossed.\n\nMore."
                    ),
                    json!("Dates and Times"),
                ],
                vec![
                    json!("date_diff"),
                    json!(0),
                    json!("dateDiff"),
                    json!("System"),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                ],
                vec![
                    json!("quantile"),
                    json!(1),
                    json!(""),
                    json!("System"),
                    json!(""),
                    json!("quantile(level)(expr)"),
                    json!("[`Float64`](/a) or [`Decimal*`](/b) or [`Float64`](/a)"),
                    json!(""),
                    json!("Aggregate Functions"),
                ],
                vec![
                    json!("row_number"),
                    json!(1),
                    json!(""),
                    json!("System"),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!("Numbers the current row.\n\n**Syntax**"),
                    json!("Aggregate Functions"),
                ],
                vec![
                    json!("linear_equation"),
                    json!(0),
                    json!(""),
                    json!("SQLUserDefined"),
                    json!("CREATE FUNCTION linear_equation AS (x, k, b) -> ((k * x) + b)"),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                ],
                vec![
                    json!("_CAST"),
                    json!(0),
                    json!(""),
                    json!("System"),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                ],
                vec![
                    json!("validateNestedArraySizes"),
                    json!(0),
                    json!(""),
                    json!("System"),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!(""),
                    json!("Internal"),
                ],
            ],
        );
        let list = from_functions(&rows);
        let names: Vec<&str> = list.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "dateDiff",
                "date_diff",
                "quantile",
                "row_number",
                "linear_equation"
            ]
        );
        assert_eq!(list[0].kind, FunctionKind::Scalar);
        assert_eq!(list[0].args, "(unit, startdate, enddate[, timezone])");
        assert_eq!(list[0].returns, "Int64");
        assert_eq!(
            list[0].description,
            "Returns the count of the specified `unit` boundaries crossed."
        );
        assert_eq!(list[1].args, list[0].args);
        assert_eq!(list[1].description, list[0].description);
        assert_eq!(list[2].kind, FunctionKind::Aggregate);
        assert_eq!(list[2].args, "(level)(expr)");
        assert_eq!(list[2].returns, "Float64 or Decimal*");
        assert_eq!(list[3].kind, FunctionKind::Window);
        assert_eq!(list[3].description, "Numbers the current row.");
        assert_eq!(list[4].args, "(x, k, b)");
        assert!(list[4].schema.is_empty());

        // 22.8 has only the first columns, as numbers.
        let old = set(
            &["name", "is_aggregate", "alias_to"],
            vec![vec![json!("uniq"), json!(1), json!("")]],
        );
        let list = from_functions(&old);
        assert_eq!(list[0].kind, FunctionKind::Aggregate);
        assert!(list[0].args.is_empty() && list[0].description.is_empty());

        let tables = set(
            &["name", "description"],
            vec![vec![
                json!("s3"),
                json!("import { X } from \"/x.jsx\";\n\n<X/>\nProvides a table-like interface.\n\nMore."),
            ]],
        );
        let list = from_table_functions(&tables);
        assert_eq!(list[0].kind, FunctionKind::Table);
        assert_eq!(list[0].description, "Provides a table-like interface.");
    }

    #[test]
    fn syntax_and_parameters_are_read() {
        assert_eq!(syntax_args("ceil", "ceiling(x[, N])"), "(x[, N])");
        assert_eq!(
            syntax_args(
                "mortonEncode",
                "-- Simplified mode\nmortonEncode(args)\n\n-- Expanded mode\nmortonEncode(range_mask, args)"
            ),
            "(args)"
        );
        assert_eq!(
            syntax_args(
                "groupConcat",
                "groupConcat[(delimiter [, limit])](expression)"
            ),
            "[(delimiter [, limit])](expression)"
        );
        assert_eq!(syntax_args("x", "SELECT 1"), "");
        // A trailing comment isn't part of the arguments; `--` in a string is.
        assert_eq!(
            syntax_args("tokens", "tokens(value) -- 'splitByNonAlpha' tokenizer"),
            "(value)"
        );
        assert_eq!(syntax_args("f", "f(x, '--') -- note"), "(x, '--')");
        assert_eq!(udf_args("CREATE FUNCTION f AS x -> x + 1"), "(x)");
        assert_eq!(udf_args("CREATE FUNCTION f AS (a, b) -> a + b"), "(a, b)");
    }
}

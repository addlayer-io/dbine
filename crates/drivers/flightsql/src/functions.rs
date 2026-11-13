//! The function catalog for the editor's completion (#97). Flight SQL is a
//! protocol, not an engine: where the server behind it lists its functions
//! in SQL, that list is read (DuckDB's `duckdb_functions()`, SQLite's
//! `pragma_function_list`, DataFusion's `information_schema.routines`),
//! with the user's macros and functions in their schema. Dremio's are its
//! static catalog (the Dremio driver's, filtered by the server's version)
//! plus the user's from `sys.user_defined_functions`. Otherwise, or if that
//! read fails, the functions `GetSqlInfo` names (numeric, string, system and
//! date-time) plus the SQL standard's (`shared::ANSI`); SQLite's static
//! catalog for a SQLite server without the pragma.

use crate::{cells, flight_error, text, Engine, FlightSession};
use arrow_flight::sql::SqlInfo;
use dbine_driver::functions::{parse_catalog, shared, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// SQLite's catalog code (the `pragma_function_list` query and its rows),
/// shared with the SQLite and libSQL drivers.
#[path = "../../sqlite/src/functions.rs"]
mod sqlite_catalog;

/// The user's functions in Dremio's `sys.user_defined_functions`, shared
/// with the Dremio driver.
#[path = "../../dremio/src/functions/udf.rs"]
mod dremio_udf;

/// Dremio's built-ins, the Dremio driver's static catalog.
const DREMIO: &str = include_str!("../../dremio/src/functions/dremio.txt");

const DUCKDB_SQL: &str =
    "SELECT function_name, function_type, parameters, parameter_types, varargs, \
     return_type, description, schema_name, internal \
     FROM duckdb_functions() \
     WHERE function_type IN ('scalar', 'aggregate', 'table', 'macro', 'table_macro')";

const DATAFUSION_SQL: &str = "SELECT routine_schema, routine_name, function_type, data_type, \
     description, syntax_example FROM information_schema.routines";

/// The `GetSqlInfo` lists of function names.
const LISTS: [SqlInfo; 4] = [
    SqlInfo::SqlNumericFunctions,
    SqlInfo::SqlStringFunctions,
    SqlInfo::SqlSystemFunctions,
    SqlInfo::SqlDatetimeFunctions,
];

impl FlightSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        if self.server.name.is_empty() {
            self.server = self.sql_info().await.unwrap_or_default();
        }
        if self.server.is_sqlite() {
            return Ok(match self.sqlite_functions().await {
                Ok(list) if !list.is_empty() => list,
                Ok(_) => sqlite_catalog::fallback(""),
                Err(e) => {
                    tracing::debug!("flightsql: pragma_function_list unreadable: {e}");
                    sqlite_catalog::fallback("")
                }
            });
        }
        let read = match self.server.engine() {
            Engine::DuckDb => match self.records(DUCKDB_SQL).await {
                Ok(r) => Some(Ok(from_duckdb(&r))),
                // GizmoSQL serves SQLite too, without always saying so.
                Err(e) => {
                    tracing::debug!("flightsql: duckdb_functions() unreadable: {e}");
                    Some(self.sqlite_functions().await)
                }
            },
            Engine::DataFusion => Some(
                self.records(DATAFUSION_SQL)
                    .await
                    .map(|r| from_datafusion(&r)),
            ),
            Engine::Dremio => return Ok(self.dremio_functions().await),
            Engine::Other => None,
        };
        match read {
            Some(Ok(list)) if !list.is_empty() => return Ok(list),
            Some(Err(e)) => tracing::debug!("flightsql: function list unreadable: {e}"),
            _ => {}
        }
        let names = match self.sql_info_functions().await {
            Ok(names) => names,
            Err(e) => {
                tracing::debug!("flightsql: GetSqlInfo function lists failed: {e}");
                Vec::new()
            }
        };
        Ok(with_standard(names))
    }

    /// SQLite's `pragma_function_list`, as the SQLite driver reads it.
    async fn sqlite_functions(&self) -> Result<Vec<FunctionInfo>> {
        let (_, rows) = self.rows(sqlite_catalog::LIST).await?;
        Ok(from_sqlite(&rows))
    }

    /// Dremio's static catalog for the server's version, plus the user's
    /// functions. `/apiv2/sql/functions`, the Dremio driver's first source,
    /// is Dremio's REST API, which Flight SQL doesn't reach.
    async fn dremio_functions(&self) -> Vec<FunctionInfo> {
        let mut list = parse_catalog(DREMIO, Some(&self.server.version));
        match self.records(dremio_udf::UDF_SQL).await {
            Ok(rows) => list.extend(rows.iter().filter_map(dremio_udf::from_udf)),
            Err(e) => tracing::debug!("flightsql: sys.user_defined_functions unreadable: {e}"),
        }
        list
    }

    /// The names in `GetSqlInfo`'s function lists.
    async fn sql_info_functions(&self) -> Result<Vec<String>> {
        let mut c = self.conn.client();
        let info = self
            .cancel
            .run(async { c.get_sql_info(LISTS.to_vec()).await.map_err(flight_error) })
            .await?;
        let mut out = Vec::new();
        for b in self.batches(info).await? {
            for (_, v) in cells::sql_info_rows(&b) {
                out.extend(names_in(&v));
            }
        }
        Ok(out)
    }
}

/// A `GetSqlInfo` string list, as the cells read it: a JSON array, or the
/// text of one.
fn names_in(v: &Value) -> Vec<String> {
    let items = match v {
        Value::Array(a) => a.clone(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            _ => s.split(',').map(|x| Value::String(x.to_string())).collect(),
        },
        _ => Vec::new(),
    };
    items
        .iter()
        .map(|x| text(x).trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// The standard catalog plus the names the server listed that it lacks,
/// as scalar functions without arguments (`GetSqlInfo` gives only names).
pub(crate) fn with_standard(names: Vec<String>) -> Vec<FunctionInfo> {
    let mut out = parse_catalog(shared::ANSI, None);
    let mut known: HashSet<String> = out.iter().map(|f| f.name.to_ascii_lowercase()).collect();
    for name in names {
        if known.insert(name.to_ascii_lowercase()) {
            out.push(FunctionInfo::new(name, FunctionKind::Scalar));
        }
    }
    out
}

/// The rows of SQLite's catalog query (name, kind, argument count,
/// version) as the SQLite driver maps them.
pub(crate) fn from_sqlite(rows: &[Vec<Value>]) -> Vec<FunctionInfo> {
    let cell = |r: &[Value], i: usize| r.get(i).map(text).unwrap_or_default();
    let version = rows.first().map(|r| cell(r, 3)).unwrap_or_default();
    let rows: Vec<(String, String, i64)> = rows
        .iter()
        .map(|r| {
            let narg = match r.get(2) {
                Some(Value::Number(n)) => n.as_i64(),
                Some(v) => text(v).trim().parse().ok(),
                None => None,
            };
            (cell(r, 0), cell(r, 1), narg.unwrap_or(-1))
        })
        .collect();
    sqlite_catalog::from_server(&rows, &version)
}

fn get(r: &Map<String, Value>, k: &str) -> String {
    r.get(k).map(text).unwrap_or_default()
}

fn list(r: &Map<String, Value>, k: &str) -> Vec<String> {
    match r.get(k) {
        Some(Value::Array(a)) => a.iter().map(text).collect(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a.iter().map(text).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn is_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !name.starts_with("__")
}

/// `duckdb_functions()`' rows: one entry per overload; built-ins have
/// `internal`, the user's macros and functions keep their schema.
pub(crate) fn from_duckdb(rows: &[Map<String, Value>]) -> Vec<FunctionInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let name = get(r, "function_name");
        if !is_identifier(&name) {
            continue;
        }
        let kind = match get(r, "function_type").as_str() {
            "aggregate" => FunctionKind::Aggregate,
            "table" | "table_macro" => FunctionKind::Table,
            _ => FunctionKind::Scalar,
        };
        let names = list(r, "parameters");
        let types = list(r, "parameter_types");
        let mut params: Vec<String> = names
            .iter()
            .enumerate()
            .map(
                |(i, n)| match types.get(i).filter(|t| !t.is_empty() && *t != "NULL") {
                    Some(t) => format!("{n} {t}"),
                    None => n.clone(),
                },
            )
            .collect();
        let varargs = get(r, "varargs");
        if !varargs.is_empty() {
            params.push(format!("{varargs}..."));
        }
        let args = format!("({})", params.join(", "));
        let schema = if r.get("internal").and_then(Value::as_bool) == Some(true) {
            String::new()
        } else {
            get(r, "schema_name")
        };
        if !seen.insert((name.clone(), args.clone(), schema.clone())) {
            continue;
        }
        let returns = get(r, "return_type");
        out.push(
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(if returns == "NULL" {
                    String::new()
                } else {
                    returns
                })
                .description(get(r, "description"))
                .schema(schema),
        );
    }
    // DuckDB's window functions aren't in `duckdb_functions()`.
    if !out.is_empty() {
        let have: HashSet<String> = out.iter().map(|f| f.name.to_ascii_lowercase()).collect();
        out.extend(parse_catalog(shared::ANSI, None).into_iter().filter(|f| {
            f.kind == FunctionKind::Window && !have.contains(&f.name.to_ascii_lowercase())
        }));
    }
    out
}

/// DataFusion's `information_schema.routines`: one row per signature;
/// the arguments come from its `syntax_example` (`abs(numeric_expression)`).
pub(crate) fn from_datafusion(rows: &[Map<String, Value>]) -> Vec<FunctionInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let name = get(r, "routine_name");
        if !is_identifier(&name) {
            continue;
        }
        let kind = match get(r, "function_type").to_ascii_uppercase().as_str() {
            "AGGREGATE" => FunctionKind::Aggregate,
            "WINDOW" => FunctionKind::Window,
            "TABLE" => FunctionKind::Table,
            _ => FunctionKind::Scalar,
        };
        let syntax = get(r, "syntax_example");
        let args = syntax
            .lines()
            .next()
            .and_then(|l| l.find('(').map(|i| l[i..].trim().to_string()))
            .unwrap_or_default();
        let returns = get(r, "data_type");
        if !seen.insert((name.clone(), args.clone(), returns.clone())) {
            continue;
        }
        out.push(
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(returns)
                .description(get(r, "description")),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Map<String, Value> {
        let Value::Object(m) = v else { unreachable!() };
        m
    }

    #[test]
    fn duckdb_rows_map_into_entries() {
        let rows = vec![
            row(json!({
                "function_name": "lower", "function_type": "scalar",
                "parameters": ["string"], "parameter_types": ["VARCHAR"], "varargs": null,
                "return_type": "VARCHAR", "description": "Converts string to lower case.",
                "schema_name": "main", "internal": true
            })),
            row(json!({
                "function_name": "concat", "function_type": "scalar",
                "parameters": ["value"], "parameter_types": ["ANY"], "varargs": "ANY",
                "return_type": "VARCHAR", "description": null,
                "schema_name": "main", "internal": true
            })),
            row(json!({
                "function_name": "approx_count_distinct", "function_type": "aggregate",
                "parameters": ["x"], "parameter_types": ["ANY"], "varargs": null,
                "return_type": "BIGINT", "description": null,
                "schema_name": "main", "internal": true
            })),
            row(json!({
                "function_name": "range", "function_type": "table",
                "parameters": ["start", "stop"], "parameter_types": ["BIGINT", "BIGINT"],
                "varargs": null, "return_type": null, "description": null,
                "schema_name": "main", "internal": true
            })),
            row(json!({
                "function_name": "twice", "function_type": "macro",
                "parameters": ["x"], "parameter_types": [null], "varargs": null,
                "return_type": null, "description": null,
                "schema_name": "main", "internal": false
            })),
            row(json!({
                "function_name": "~~", "function_type": "scalar",
                "parameters": [], "parameter_types": [], "varargs": null,
                "return_type": "BOOLEAN", "description": null,
                "schema_name": "main", "internal": true
            })),
        ];
        let list = from_duckdb(&rows);
        let get = |n: &str| list.iter().find(|f| f.name == n).unwrap();
        assert_eq!(get("lower").args, "(string VARCHAR)");
        assert_eq!(get("lower").returns, "VARCHAR");
        assert_eq!(get("lower").description, "Converts string to lower case.");
        assert!(get("lower").schema.is_empty());
        assert_eq!(get("concat").args, "(value ANY, ANY...)");
        assert_eq!(get("approx_count_distinct").kind, FunctionKind::Aggregate);
        assert_eq!(get("range").kind, FunctionKind::Table);
        assert_eq!(get("range").returns, "");
        assert_eq!(get("twice").args, "(x)");
        assert_eq!(get("twice").schema, "main");
        assert!(!list.iter().any(|f| f.name == "~~"));
    }

    #[test]
    fn datafusion_rows_map_into_entries() {
        let rows = vec![
            row(json!({
                "routine_schema": "public", "routine_name": "abs", "function_type": "SCALAR",
                "data_type": "Float64", "description": "Returns the absolute value of a number.",
                "syntax_example": "abs(numeric_expression)"
            })),
            row(json!({
                "routine_schema": "public", "routine_name": "abs", "function_type": "SCALAR",
                "data_type": "Float64", "description": "Returns the absolute value of a number.",
                "syntax_example": "abs(numeric_expression)"
            })),
            row(json!({
                "routine_schema": "public", "routine_name": "row_number", "function_type": "WINDOW",
                "data_type": "UInt64", "description": "Number of the current row.",
                "syntax_example": "row_number()"
            })),
            row(json!({
                "routine_schema": "public", "routine_name": "approx_distinct",
                "function_type": "AGGREGATE", "data_type": "UInt64", "description": null,
                "syntax_example": null
            })),
        ];
        let list = from_datafusion(&rows);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].args, "(numeric_expression)");
        assert_eq!(list[0].returns, "Float64");
        assert_eq!(list[1].kind, FunctionKind::Window);
        assert_eq!(list[2].kind, FunctionKind::Aggregate);
        assert_eq!(list[2].args, "");
    }

    #[test]
    fn sqlite_rows_map_into_entries() {
        let rows = vec![
            vec![json!("abs"), json!("s"), json!(1), json!("3.45.1")],
            vec![json!("count"), json!("w"), json!(0), json!("3.45.1")],
            vec![json!("count"), json!("w"), json!("1"), json!("3.45.1")],
            vec![json!("->>"), json!("s"), json!(2), json!("3.45.1")],
            vec![json!("json_each"), json!("m"), json!(-1), json!("3.45.1")],
        ];
        let list = from_sqlite(&rows);
        let find = |n: &str| list.iter().filter(|f| f.name == n).collect::<Vec<_>>();
        assert_eq!(find("abs")[0].args, "(x)");
        assert!(find("count")
            .iter()
            .all(|f| f.kind == FunctionKind::Aggregate));
        assert!(find("->>").is_empty());
        assert_eq!(find("json_each")[0].kind, FunctionKind::Table);
        // Without the pragma: SQLite's static catalog.
        assert!(sqlite_catalog::fallback("")
            .iter()
            .any(|f| f.name == "substr"));
    }

    #[test]
    fn dremio_gets_its_own_catalog() {
        let list = parse_catalog(DREMIO, Some("26.0.5"));
        for name in [
            "CONVERT_FROM",
            "FLATTEN",
            "ARRAY_CONTAINS",
            "APPROX_COUNT_DISTINCT",
        ] {
            assert!(list.iter().any(|f| f.name == name), "{name}");
        }
        let Value::Object(udf) = json!({
            "name": "sales.twice",
            "argList": "[FunctionArg{name=x, rawDataType=<ByteString@1 size=1>, defaultExpression=null}]",
            "sql": "SELECT x * 2",
        }) else {
            unreachable!()
        };
        let f = dremio_udf::from_udf(&udf).unwrap();
        assert_eq!(
            (f.name.as_str(), f.schema.as_str(), f.args.as_str()),
            ("twice", "sales", "(x)")
        );
    }

    #[test]
    fn sql_info_names_join_the_standard_catalog() {
        assert_eq!(
            names_in(&json!(["ABS", "ACOS"])),
            vec!["ABS".to_string(), "ACOS".to_string()]
        );
        assert_eq!(names_in(&json!("[\"PI\"]")), vec!["PI".to_string()]);
        let standard = parse_catalog(shared::ANSI, None);
        let list = with_standard(vec!["dbine_only_here".into(), "DBINE_ONLY_HERE".into()]);
        assert_eq!(list.len(), standard.len() + 1);
        let added = list.last().unwrap();
        assert_eq!(added.name, "dbine_only_here");
        assert_eq!(added.kind, FunctionKind::Scalar);
        // A name the standard catalog has isn't repeated.
        if let Some(f) = standard.first() {
            let list = with_standard(vec![f.name.to_ascii_uppercase()]);
            assert_eq!(list.len(), standard.len());
        }
    }
}

//! `executeQueries` (spec §4.2, JSON). Owned by part B.
//!
//! - Body: `{"queries":[{"query":…}],"serializerSettings":{"includeNulls":true}}`
//!   plus `impersonatedUserName` when the connection tests RLS.
//! - Rows parsed keeping key order (columns = union of keys in order of
//!   appearance) and each number's literal text (`serde_json`'s
//!   `raw_value`): integers through `json_i64`, fixed decimals as text,
//!   doubles through `json_f64`; DateTime strings normalized.
//! - An `error` at the response, `results[i]` or `tables[j]` level: with
//!   rows it's a truncation, without rows `Error::Statement`.
//! - Truncation also at 100,000 rows, when one more row wouldn't fit in
//!   1,000,000 values, or near 15 MB ([`CUT_BYTES`]): those cuts may come
//!   without an `error`, and a false positive only marks a whole result
//!   partial.
//! - `informationProtectionLabel` → `out.info`.

use super::Rowset;
use dbine_driver::{json_f64, json_i64, Error, Result, ScriptError};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use std::fmt;

/// Rows per query the API returns at most.
pub const MAX_ROWS: usize = 100_000;
/// Values (rows × columns) per query.
pub const MAX_VALUES: usize = 1_000_000;
/// Bytes of an answer at most, as the largest reading of Learn's "15MB of
/// data per query" (15 MiB); the service cuts after the current row.
pub const MAX_BYTES: usize = 15 * 1024 * 1024;
/// An answer this large is taken as cut by size: under 15,000,000 bytes
/// with a margin, since whether the service counts 15,000,000 bytes or
/// 15 MiB, the whole body or only the rows, is a live check.
pub const CUT_BYTES: usize = 14_500_000;

/// Whether an answer of `rows` rows of `columns` columns and `bytes` bytes
/// may have been cut by the service's caps: 100,000 rows, the next row
/// past 1,000,000 values (13 columns stop at 76,923 rows), or the size.
pub fn capped(rows: usize, columns: usize, bytes: usize) -> bool {
    bytes >= CUT_BYTES
        || rows >= MAX_ROWS
        || rows.saturating_add(1).saturating_mul(columns.max(1)) > MAX_VALUES
}

/// The warning of a cut result.
pub const CUT: &str = "La API de Power BI devuelve como máximo 100.000 filas o 1.000.000 de valores (15 MB) por consulta; el resultado está cortado.";

/// The request body for one query.
pub fn body(query: &str, impersonated_user: Option<&str>) -> Value {
    let mut b = json!({
        "queries": [{ "query": query }],
        "serializerSettings": { "includeNulls": true },
    });
    if let Some(u) = impersonated_user {
        b["impersonatedUserName"] = json!(u);
    }
    b
}

/// One row: its keys in order and each value's literal JSON.
struct Row<'a>(Vec<(String, &'a RawValue)>);

impl<'de: 'a, 'a> Deserialize<'de> for Row<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Row<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a row object")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Row<'a>, M::Error> {
                let mut cells = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, &'a RawValue>()? {
                    cells.push((k, v));
                }
                Ok(Row(cells))
            }
        }
        d.deserialize_map(V(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
struct Table<'a> {
    #[serde(default, borrow)]
    rows: Vec<Row<'a>>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct QueryResult<'a> {
    #[serde(default, borrow)]
    tables: Vec<Table<'a>>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Answer<'a> {
    #[serde(default, borrow)]
    results: Vec<QueryResult<'a>>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default, rename = "informationProtectionLabel")]
    label: Option<Value>,
}

/// One table of the answer, every row of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table2 {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// The API cut it (its `error` next to rows, or a cap reached).
    pub cut: bool,
}

/// A whole answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub tables: Vec<Table2>,
    /// The sensitivity label's name.
    pub label: Option<String>,
    /// The query failed: its error and, when the service says, its 1-based
    /// line and column in the query sent.
    pub failure: Option<(ScriptError, Option<(u32, u32)>)>,
}

/// The message and position of a DAX error object (`{code, message}` or
/// `pbi.error` with `DetailsMessage` "Query (L, C) …" and
/// `AnalysisServicesErrorCode`): the error and its 1-based line and column
/// in the query sent.
pub fn statement_error(e: &Value) -> (ScriptError, Option<(u32, u32)>) {
    let e = e.get("error").unwrap_or(e);
    let pbi = e.get("pbi.error").unwrap_or(e);
    let detail = |code: &str| {
        pbi.get("details")
            .and_then(Value::as_array)
            .and_then(|d| {
                d.iter()
                    .find(|x| x.get("code").and_then(Value::as_str) == Some(code))
            })
            .and_then(|x| x.get("detail").and_then(|d| d.get("value")))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let raw = detail("DetailsMessage")
        .or_else(|| e.get("message").and_then(Value::as_str).map(str::to_string))
        .or_else(|| e.get("code").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "Power BI rechazó la consulta".to_string());
    let (pos, message) = query_position(&raw);
    let mut se = ScriptError::new(crate::auth::short(&message, 2000));
    if let Some(code) = detail("AnalysisServicesErrorCode")
        .or_else(|| e.get("code").and_then(Value::as_str).map(str::to_string))
    {
        se = se.with_code(code);
    }
    (se, pos)
}

/// `Query (3, 12) The syntax…` → ((3, 12), "The syntax…").
fn query_position(msg: &str) -> (Option<(u32, u32)>, String) {
    let t = msg.trim_start();
    let parse = || {
        let rest = t.strip_prefix("Query (")?;
        let (inside, after) = rest.split_once(')')?;
        let (l, c) = inside.split_once(',')?;
        Some((
            (l.trim().parse().ok()?, c.trim().parse().ok()?),
            after.trim().to_string(),
        ))
    };
    match parse() {
        Some((pos, rest)) if !rest.is_empty() => (Some(pos), rest),
        _ => (None, msg.trim().to_string()),
    }
}

/// `2024-01-31T13:45:00[.fff]` (how the API writes a DAX DateTime).
fn is_dax_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .iter()
            .all(|r| b[r.clone()].iter().all(u8::is_ascii_digit))
        && (b.len() == 19
            || (b[19] == b'.' && b.len() > 20 && b[20..].iter().all(u8::is_ascii_digit)))
}

/// A cell from its literal JSON; `decimal` keeps a fraction's text.
fn cell(raw: &RawValue, decimal: bool) -> Value {
    let t = raw.get().trim();
    match t.as_bytes().first() {
        Some(b'"') => serde_json::from_str::<String>(t).map_or(Value::Null, Value::String),
        Some(b'-' | b'0'..=b'9') => {
            if let Ok(i) = t.parse::<i64>() {
                json_i64(i)
            } else if decimal {
                Value::String(t.to_string())
            } else {
                t.parse::<f64>()
                    .map_or_else(|_| Value::String(t.to_string()), json_f64)
            }
        }
        Some(b't') => Value::Bool(true),
        Some(b'f') => Value::Bool(false),
        Some(b'n') | None => Value::Null,
        Some(_) => Value::String(t.to_string()),
    }
}

/// Parses an answer. `type_of` gives a column's model type when the
/// metadata knows it (`Decimal` keeps the text, `DateTime` is normalized);
/// a column whose every value is a DAX DateTime is normalized too.
pub fn parse_typed(body: &[u8], type_of: &dyn Fn(&str) -> Option<String>) -> Result<Parsed> {
    let answer: Answer<'_> = serde_json::from_slice(body)
        .map_err(|e| Error::Query(format!("Power BI respondió algo que no se pudo leer: {e}")))?;
    let label = answer.label.as_ref().and_then(|l| match l {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => l.get("name").and_then(Value::as_str).map(str::to_string),
        _ => None,
    });
    let mut tables = Vec::new();
    let mut any_rows = false;
    for r in &answer.results {
        let result_error = r.error.as_ref();
        for t in &r.tables {
            let mut columns: Vec<String> = Vec::new();
            for row in &t.rows {
                for (k, _) in &row.0 {
                    if !columns.iter().any(|c| c == k) {
                        columns.push(k.clone());
                    }
                }
            }
            let types: Vec<Option<String>> = columns.iter().map(|c| type_of(c)).collect();
            let decimal: Vec<bool> = types
                .iter()
                .map(|t| {
                    t.as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("Decimal"))
                })
                .collect();
            let mut rows: Vec<Vec<Value>> = t
                .rows
                .iter()
                .map(|row| {
                    let mut out = vec![Value::Null; columns.len()];
                    for (k, v) in &row.0 {
                        if let Some(i) = columns.iter().position(|c| c == k) {
                            out[i] = cell(v, decimal[i]);
                        }
                    }
                    out
                })
                .collect();
            for (i, ty) in types.iter().enumerate() {
                let typed = ty
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("DateTime"));
                let strings = rows.iter().filter(|r| !r[i].is_null()).count();
                let all = strings > 0
                    && rows.iter().all(|r| match &r[i] {
                        Value::Null => true,
                        Value::String(s) => is_dax_datetime(s),
                        _ => false,
                    });
                if typed || all {
                    for r in &mut rows {
                        if let Value::String(s) = &mut r[i] {
                            if is_dax_datetime(s) {
                                s.replace_range(10..11, " ");
                            }
                        }
                    }
                }
            }
            let error = t.error.as_ref().or(result_error);
            if let Some(e) = error {
                if rows.is_empty() {
                    return Ok(Parsed {
                        tables,
                        label,
                        failure: Some(statement_error(e)),
                    });
                }
            }
            any_rows |= !rows.is_empty();
            let cut = error.is_some() || capped(rows.len(), columns.len(), body.len());
            tables.push(Table2 { columns, rows, cut });
        }
        if r.tables.is_empty() {
            if let Some(e) = result_error {
                return Ok(Parsed {
                    tables,
                    label,
                    failure: Some(statement_error(e)),
                });
            }
        }
    }
    if let Some(e) = &answer.error {
        if !any_rows {
            return Ok(Parsed {
                tables,
                label,
                failure: Some(statement_error(e)),
            });
        }
        for t in &mut tables {
            t.cut = true;
        }
    }
    Ok(Parsed {
        tables,
        label,
        failure: None,
    })
}

/// The answer's tables, in order (one per query here), with their
/// truncation and the sensitivity label, if any.
pub fn parse(body: &[u8], max_rows: usize) -> Result<(Vec<Rowset>, Option<String>)> {
    let p = parse_typed(body, &|_| None)?;
    if let Some((e, _)) = p.failure {
        return Err(Error::Statement(Box::new(e)));
    }
    let sets = p
        .tables
        .into_iter()
        .map(|mut t| {
            let over = t.rows.len() > max_rows;
            t.rows.truncate(max_rows);
            Rowset {
                columns: t.columns,
                types: Vec::new(),
                rows: t.rows,
                truncated: over || t.cut,
            }
        })
        .collect();
    Ok((sets, p.label))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Microsoft Learn's sample answer of Datasets - Execute Queries.
    const LEARN: &str = include_str!("../../tests/fixtures/execute_queries.json");

    #[test]
    fn the_learn_sample() {
        let (sets, label) = parse(LEARN.as_bytes(), 1000).unwrap();
        assert_eq!(label, None);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].columns, ["MyTable[Year]", "MyTable[Quarter]"]);
        assert_eq!(sets[0].rows[0], vec![json!(2010), json!("Q1")]);
        assert_eq!(sets[0].rows.len(), 3);
        assert!(!sets[0].truncated);
        let (sets, _) = parse(LEARN.as_bytes(), 2).unwrap();
        assert!(sets[0].truncated && sets[0].rows.len() == 2);
    }

    #[test]
    fn key_order_nulls_decimals_and_dates() {
        // Keys in the order the service writes them (not alphabetical), a
        // blank kept by includeNulls, a column missing from the first row.
        let body = r#"{"results":[{"tables":[{"rows":[
            {"Z[b]":1,"A[a]":12.3400,"[When]":"2024-01-31T13:45:00","[Big]":9007199254740993},
            {"Z[b]":null,"A[a]":0.5,"[When]":null,"[Big]":1.5,"[Late]":"x"}
        ]}]}],"informationProtectionLabel":{"id":"x","name":"Confidencial"}}"#;
        let p = parse_typed(body.as_bytes(), &|c| {
            (c == "A[a]").then(|| "Decimal".to_string())
        })
        .unwrap();
        assert_eq!(p.label.as_deref(), Some("Confidencial"));
        let t = &p.tables[0];
        assert_eq!(t.columns, ["Z[b]", "A[a]", "[When]", "[Big]", "[Late]"]);
        assert_eq!(
            t.rows[0],
            vec![
                json!(1),
                json!("12.3400"),
                json!("2024-01-31 13:45:00"),
                json!("9007199254740993"),
                Value::Null
            ]
        );
        assert_eq!(
            t.rows[1],
            vec![
                Value::Null,
                json!("0.5"),
                Value::Null,
                json!(1.5),
                json!("x")
            ]
        );
        // Without the type, a fraction is a double; a mixed column isn't a date.
        let body = r#"{"results":[{"tables":[{"rows":[{"[a]":12.34,"[d]":"2024-01-31T00:00:00"},{"[a]":1,"[d]":"no"}]}]}]}"#;
        let p = parse_typed(body.as_bytes(), &|_| None).unwrap();
        assert_eq!(
            p.tables[0].rows[0],
            vec![json!(12.34), json!("2024-01-31T00:00:00")]
        );
    }

    #[test]
    fn zero_rows_have_no_columns() {
        let (sets, _) = parse(br#"{"results":[{"tables":[{"rows":[]}]}]}"#, 10).unwrap();
        assert!(sets[0].columns.is_empty() && sets[0].rows.is_empty());
    }

    #[test]
    fn an_error_next_to_rows_is_a_cut_without_rows_a_failure() {
        let cut = r#"{"results":[{"tables":[{"rows":[{"[a]":1}],"error":{"code":"ResultTruncated","message":"More than 100000 rows in a query result"}}]}]}"#;
        let (sets, _) = parse(cut.as_bytes(), 10).unwrap();
        assert!(sets[0].truncated);
        let cut = r#"{"results":[{"tables":[{"rows":[{"[a]":1}]}],"error":{"code":"x","message":"More than one result table in a query"}}]}"#;
        assert!(parse(cut.as_bytes(), 10).unwrap().0[0].truncated);
        let failed = r#"{"results":[{"error":{"code":"DatasetExecuteQueriesError","message":"Query (1, 10) The syntax for 'x' is incorrect."}}]}"#;
        let Err(Error::Statement(e)) = parse(failed.as_bytes(), 10) else {
            panic!("expected a statement error");
        };
        assert_eq!(e.message, "The syntax for 'x' is incorrect.");
        assert_eq!(e.code.as_deref(), Some("DatasetExecuteQueriesError"));
        // Caps reached without an error object.
        let mut rows = String::from(r#"{"results":[{"tables":[{"rows":["#);
        for i in 0..MAX_ROWS {
            if i > 0 {
                rows.push(',');
            }
            rows.push_str(&format!(r#"{{"[a]":{i}}}"#));
        }
        rows.push_str("]}]}]}");
        let (sets, _) = parse(rows.as_bytes(), usize::MAX).unwrap();
        assert!(sets[0].truncated && sets[0].rows.len() == MAX_ROWS);
        let (sets, _) = parse(rows.as_bytes(), 10).unwrap();
        assert_eq!(sets[0].rows.len(), 10);
    }

    #[test]
    fn the_value_cap_is_the_row_that_wouldnt_fit() {
        // 13 columns: the service stops at 76,923 rows (999,999 values).
        assert!(capped(76_923, 13, 0));
        assert!(!capped(76_922, 13, 0));
        // 11 columns: 90,909 rows (999,999 values) is the cap.
        assert!(capped(90_909, 11, 0) && !capped(90_908, 11, 0));
        // 10 columns: the 100,000th row still fits in 1,000,000 values.
        assert!(!capped(99_999, 10, 0) && capped(100_000, 10, 0));
        // One column: the row cap comes first.
        assert!(capped(MAX_ROWS, 1, 0) && !capped(MAX_ROWS - 1, 1, 0));
        // An empty answer is never cut.
        assert!(!capped(0, 0, 100));
    }

    #[test]
    fn thirteen_columns_cut_without_an_error_object() {
        let cols: Vec<String> = (0..13).map(|c| format!("T[c{c}]")).collect();
        let row = format!(
            "{{{}}}",
            cols.iter()
                .map(|c| format!(r#""{c}":1"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let body = |rows: usize| {
            format!(
                r#"{{"results":[{{"tables":[{{"rows":[{}]}}]}}]}}"#,
                vec![row.as_str(); rows].join(",")
            )
        };
        let p = parse_typed(body(76_923).as_bytes(), &|_| None).unwrap();
        assert_eq!(p.tables[0].columns.len(), 13);
        assert!(p.tables[0].cut, "76,923 rows of 13 columns is the cap");
        let p = parse_typed(body(76_922).as_bytes(), &|_| None).unwrap();
        assert!(!p.tables[0].cut);
    }

    #[test]
    fn a_body_past_15_million_bytes_is_cut_without_an_error_object() {
        // Two rows of long text: a body just over 15,000,000 bytes (under
        // 15 MiB), no `error` anywhere.
        let text = "x".repeat(7_500_000);
        let body = format!(
            r#"{{"results":[{{"tables":[{{"rows":[{{"[t]":"{text}"}},{{"[t]":"{text}"}}]}}]}}]}}"#
        );
        assert!(body.len() > 15_000_000 && body.len() < MAX_BYTES);
        let p = parse_typed(body.as_bytes(), &|_| None).unwrap();
        assert_eq!(p.tables[0].rows.len(), 2);
        assert!(p.tables[0].cut);
        // Well under it: whole.
        let small = text[..1000].to_string();
        let body = format!(r#"{{"results":[{{"tables":[{{"rows":[{{"[t]":"{small}"}}]}}]}}]}}"#);
        assert!(!parse_typed(body.as_bytes(), &|_| None).unwrap().tables[0].cut);
    }

    #[test]
    fn the_error_of_a_400_carries_code_and_place() {
        let body: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/execute_queries_error.json"
        ))
        .unwrap();
        let (e, pos) = statement_error(&body);
        assert_eq!(pos, Some((3, 12)));
        assert_eq!(e.code.as_deref(), Some("3238002695"));
        assert!(e.message.starts_with("The syntax for"), "{}", e.message);
        assert_eq!(
            query_position("Query (x) y"),
            (None, "Query (x) y".to_string())
        );
    }

    #[test]
    fn bodies() {
        assert_eq!(
            body("EVALUATE {1}", None),
            json!({"queries":[{"query":"EVALUATE {1}"}],"serializerSettings":{"includeNulls":true}})
        );
        assert_eq!(
            body("q", Some("ana@contoso.com"))["impersonatedUserName"],
            "ana@contoso.com"
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for b in [
            &b""[..],
            b"{",
            b"{\"results\":[{\"tables\":[{\"rows\":[1]}]}]}",
            b"\xff",
        ] {
            assert!(parse(b, 10).is_err());
        }
    }
}

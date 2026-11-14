//! The driver's own browse queries (spec §4.5, §4.6). Owned by part B.
//!
//! - Table: `EVALUATE TOPN(<limit>, 'T')` (`EVALUATE 'T'` without a
//!   limit); calculation group: `EVALUATE 'G'`; measure: `EVALUATE { [M]
//!   }`; hierarchy: `EVALUATE TOPN(<limit>, SUMMARIZECOLUMNS('T'[L1],
//!   'T'[L2]…), …) ORDER BY …` (levels from the metadata cache).
//! - Filters (`filtered_browse`): recognizes its own browse and adds
//!   `FILTER('T', cond)`. SQL's NULL semantics (G11): equality is `==`
//!   (strict), inequality and ranges add `NOT ISBLANK('T'[c]) && …`;
//!   contains → `CONTAINSSTRING` (its wildcards escaped), starts with →
//!   `LEFT(…) = "x"`, is blank → `ISBLANK`, in → `IN { … }` (strict, like
//!   `CONTAINSROW`). Other text → `Unsupported`.
//! - Sort (`sorted_browse`): `TOPN(n, …, 'T'[c1], DESC, …)` + `ORDER BY`,
//!   always with ASC/DESC written (TOPN's default is descending).

use super::lexer::{code, Token, TokenKind};
use super::quote::{column_ref, literal, quote_column, quote_table, result_column_ref, string};
use crate::Ctx;
use dbine_driver::filter::FilterOp;
use dbine_driver::{ColumnFilter, Error, ObjectRef, Result, SortKey};
use serde_json::Value;

/// One of the driver's table browses, as [`parse_browse`] reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browse {
    /// The table's name, unquoted.
    pub table: String,
    /// `TOPN`'s row count; `None`: the whole table.
    pub limit: Option<u64>,
    /// The `FILTER` condition, as written.
    pub filter: Option<String>,
    /// It already sorts (`TOPN` order arguments or `ORDER BY`).
    pub sorted: bool,
}

/// The query that shows `obj`'s first `limit` rows.
pub fn browse_query(cx: &Ctx, obj: &ObjectRef, limit: u32) -> String {
    let whole = limit == u32::MAX;
    let topn = |inner: &str| {
        if whole {
            format!("EVALUATE {inner}")
        } else {
            format!("EVALUATE TOPN({limit}, {inner})")
        }
    };
    match obj.kind.as_str() {
        crate::CALCULATION_GROUP => format!("EVALUATE {}", quote_table(&obj.name)),
        crate::MEASURE => format!("EVALUATE {{ {} }}", quote_column(&obj.name)),
        crate::HIERARCHY => {
            let Some((table, name)) = split_hierarchy(&obj.name) else {
                return topn(&quote_table(&obj.name));
            };
            let levels: Vec<String> = cx
                .meta
                .cached
                .as_ref()
                .and_then(|m| {
                    m.hierarchies
                        .iter()
                        .find(|h| h.table == table && h.name == name)
                })
                .map(|h| {
                    h.levels
                        .iter()
                        .map(|l| column_ref(&table, &l.column))
                        .collect()
                })
                .unwrap_or_default();
            if levels.is_empty() {
                return topn(&quote_table(&table));
            }
            let order: Vec<String> = levels.iter().map(|l| format!("{l} ASC")).collect();
            let inner = format!("SUMMARIZECOLUMNS({})", levels.join(", "));
            if whole {
                format!("EVALUATE {inner}\nORDER BY {}", order.join(", "))
            } else {
                let args: Vec<String> = levels.iter().map(|l| format!("{l}, ASC")).collect();
                format!(
                    "EVALUATE TOPN({limit}, {inner}, {})\nORDER BY {}",
                    args.join(", "),
                    order.join(", ")
                )
            }
        }
        _ => topn(&quote_table(&obj.name)),
    }
}

/// `'Table'[Hierarchy]` → (Table, Hierarchy).
fn split_hierarchy(name: &str) -> Option<(String, String)> {
    let toks = code(name);
    match toks.as_slice() {
        [t, r] if t.kind == TokenKind::Table && r.kind == TokenKind::Ref => {
            Some((super::lexer::unquote(t.text), super::lexer::unquote(r.text)))
        }
        _ => None,
    }
}

/// The index of the `)` that closes the `(` at `open`.
fn closing(toks: &[Token<'_>], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct("(") {
            depth += 1;
        } else if t.is_punct(")") {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The table expression at `i`: `'T'` or `FILTER('T', cond)`; the table,
/// the condition and the index after it.
fn inner_at(q: &str, toks: &[Token<'_>], i: usize) -> Option<(String, Option<String>, usize)> {
    let t = toks.get(i)?;
    if t.kind == TokenKind::Table {
        return Some((super::lexer::unquote(t.text), None, i + 1));
    }
    if !t.is_word("filter") || !toks.get(i + 1)?.is_punct("(") {
        return None;
    }
    let close = closing(toks, i + 1)?;
    let table = toks.get(i + 2).filter(|t| t.kind == TokenKind::Table)?;
    if !toks.get(i + 3)?.is_punct(",") || i + 4 >= close {
        return None;
    }
    let cond = q[toks[i + 4].start..toks[close].start].trim_end();
    Some((
        super::lexer::unquote(table.text),
        Some(cond.to_string()),
        close + 1,
    ))
}

/// Reads back a table browse the driver wrote (`browse_query`, then
/// `filtered_browse`, then `sorted_browse`); `None` for anything else.
pub fn parse_browse(q: &str) -> Option<Browse> {
    let toks = code(q);
    if !toks.first()?.is_word("evaluate") {
        return None;
    }
    let (mut b, next) = if toks.get(1)?.is_word("topn") && toks.get(2)?.is_punct("(") {
        let close = closing(&toks, 2)?;
        let n = toks.get(3).filter(|t| t.kind == TokenKind::Number)?;
        let limit: u64 = n.text.parse().ok()?;
        if !toks.get(4)?.is_punct(",") {
            return None;
        }
        let (table, filter, after) = inner_at(q, &toks, 5)?;
        let sorted = if after == close {
            false
        } else if toks[after].is_punct(",") {
            true
        } else {
            return None;
        };
        (
            Browse {
                table,
                limit: Some(limit),
                filter,
                sorted,
            },
            close + 1,
        )
    } else {
        let (table, filter, after) = inner_at(q, &toks, 1)?;
        (
            Browse {
                table,
                limit: None,
                filter,
                sorted: false,
            },
            after,
        )
    };
    match toks.get(next) {
        None => {}
        Some(t) if t.is_word("order") && toks.get(next + 1).is_some_and(|t| t.is_word("by")) => {
            b.sorted = true;
        }
        Some(_) => return None,
    }
    Some(b)
}

fn rebuild(b: &Browse, cond: Option<&str>, order: &[(String, bool)]) -> String {
    let table = quote_table(&b.table);
    let inner = match cond {
        Some(c) => format!("FILTER({table}, {c})"),
        None => table,
    };
    let by: Vec<String> = order
        .iter()
        .map(|(c, desc)| format!("{c} {}", if *desc { "DESC" } else { "ASC" }))
        .collect();
    let body = match b.limit {
        Some(n) if order.is_empty() => format!("TOPN({n}, {inner})"),
        Some(n) => {
            let args: Vec<String> = order
                .iter()
                .map(|(c, desc)| format!("{c}, {}", if *desc { "DESC" } else { "ASC" }))
                .collect();
            format!("TOPN({n}, {inner}, {})", args.join(", "))
        }
        None => inner,
    };
    if by.is_empty() {
        format!("EVALUATE {body}")
    } else {
        format!("EVALUATE {body}\nORDER BY {}", by.join(", "))
    }
}

fn unsupported_browse() -> Error {
    Error::Unsupported(
        "esta consulta no es la vista de una tabla del modelo: el filtro se aplica a las filas cargadas"
            .into(),
    )
}

/// A grid value written for a comparison: dates as `DATE(…) + TIME(…)`,
/// the rest as their own literal.
fn value_literal(v: &Value) -> Result<String> {
    if let Value::String(s) = v {
        if looks_like_datetime(s) {
            return literal(v, Some("DateTime"));
        }
    }
    literal(v, None)
}

/// `YYYY-MM-DD` with an optional ` HH:MM[:SS[.fff]]` (or `T`).
pub(crate) fn looks_like_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    let date = b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    if !date {
        return false;
    }
    if b.len() == 10 {
        return true;
    }
    if !matches!(b[10], b' ' | b'T') || b.len() < 16 {
        return false;
    }
    let t = &b[11..];
    t[..2].iter().all(u8::is_ascii_digit)
        && t[2] == b':'
        && t[3..5].iter().all(u8::is_ascii_digit)
        && (t.len() == 5
            || (t.len() >= 8
                && t[5] == b':'
                && t[6..8].iter().all(u8::is_ascii_digit)
                && (t.len() == 8
                    || (t[8] == b'.' && t.len() > 9 && t[9..].iter().all(u8::is_ascii_digit)))))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `CONTAINSSTRING`'s wildcards (`*`, `?`) and its escape (`~`) taken
/// literally.
fn no_wildcards(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '~' | '*' | '?') {
            out.push('~');
        }
        out.push(c);
    }
    out
}

/// One column filter as a DAX condition on `table`'s rows (G11).
pub fn condition(f: &ColumnFilter, table: &str) -> Result<String> {
    let c = result_column_ref(&f.column, Some(table));
    let first = || {
        f.values
            .first()
            .ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)))
    };
    let not_blank = |cond: String| format!("(NOT ISBLANK({c}) && {cond})");
    Ok(match f.op {
        FilterOp::Eq => format!("{c} == {}", value_literal(first()?)?),
        FilterOp::Ne => not_blank(format!("NOT ({c} == {})", value_literal(first()?)?)),
        FilterOp::Gt => not_blank(format!("{c} > {}", value_literal(first()?)?)),
        FilterOp::Ge => not_blank(format!("{c} >= {}", value_literal(first()?)?)),
        FilterOp::Lt => not_blank(format!("{c} < {}", value_literal(first()?)?)),
        FilterOp::Le => not_blank(format!("{c} <= {}", value_literal(first()?)?)),
        FilterOp::Contains => format!(
            "CONTAINSSTRING({c}, {})",
            string(&no_wildcards(&text_of(first()?)))
        ),
        FilterOp::NotContains => not_blank(format!(
            "NOT CONTAINSSTRING({c}, {})",
            string(&no_wildcards(&text_of(first()?)))
        )),
        FilterOp::StartsWith => {
            let s = text_of(first()?);
            format!("LEFT({c}, {}) = {}", s.chars().count(), string(&s))
        }
        FilterOp::EndsWith => {
            let s = text_of(first()?);
            format!("RIGHT({c}, {}) = {}", s.chars().count(), string(&s))
        }
        FilterOp::IsNull => format!("ISBLANK({c})"),
        FilterOp::NotNull => format!("NOT ISBLANK({c})"),
        FilterOp::IsEmpty => format!("{c} == \"\""),
        FilterOp::NotEmpty => not_blank(format!("NOT ({c} == \"\")")),
        FilterOp::In | FilterOp::NotIn => {
            if f.values.is_empty() {
                return Err(Error::Query(format!(
                    "el filtro de «{}» necesita al menos un valor",
                    f.column
                )));
            }
            let list = f
                .values
                .iter()
                .map(value_literal)
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            if f.op == FilterOp::In {
                format!("{c} IN {{ {list} }}")
            } else {
                not_blank(format!("NOT ({c} IN {{ {list} }})"))
            }
        }
        FilterOp::IsTrue => format!("{c} == TRUE()"),
        FilterOp::IsFalse => format!("{c} == FALSE()"),
        FilterOp::TrueOrNull => format!("({c} == TRUE() || ISBLANK({c}))"),
        FilterOp::FalseOrNull => format!("({c} == FALSE() || ISBLANK({c}))"),
        FilterOp::Sql | FilterOp::SqlRight => {
            let text = f.sql.as_deref().unwrap_or("").trim();
            if text.is_empty() {
                return Err(Error::Query(format!(
                    "falta la condición DAX del filtro de «{}»",
                    f.column
                )));
            }
            if f.op == FilterOp::Sql {
                format!("({text})")
            } else {
                format!("({c} {text})")
            }
        }
    })
}

pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let b = parse_browse(browse).ok_or_else(unsupported_browse)?;
    if b.filter.is_some() || b.sorted {
        return Err(unsupported_browse());
    }
    let conds = filters
        .iter()
        .map(|f| condition(f, &b.table))
        .collect::<Result<Vec<_>>>()?;
    Ok(rebuild(&b, Some(&conds.join(" && ")), &[]))
}

pub fn sorted_browse(query: &str, sort: &[SortKey]) -> Result<String> {
    if sort.is_empty() {
        return Ok(query.to_string());
    }
    let b = parse_browse(query).ok_or_else(|| {
        Error::Unsupported(
            "esta consulta no es la vista de una tabla del modelo: se ordenan las filas cargadas"
                .into(),
        )
    })?;
    if b.sorted {
        return Err(Error::Unsupported(
            "la consulta ya ordena: se ordenan las filas cargadas".into(),
        ));
    }
    let order: Vec<(String, bool)> = sort
        .iter()
        .map(|k| (result_column_ref(&k.column, Some(&b.table)), k.descending))
        .collect();
    Ok(rebuild(&b, b.filter.as_deref(), &order))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Hierarchy, Level, ModelMeta};
    use serde_json::json;
    use std::sync::Arc;

    fn f(column: &str, op: FilterOp, values: Vec<Value>) -> ColumnFilter {
        ColumnFilter {
            column: column.into(),
            op,
            values,
            sql: None,
        }
    }

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    #[test]
    fn browses_by_kind() {
        let mut cx = crate::http::testing::ctx(&[]);
        assert_eq!(
            browse_query(&cx, &obj(crate::TABLE, "Sales d'Or"), 200),
            "EVALUATE TOPN(200, 'Sales d''Or')"
        );
        assert_eq!(
            browse_query(&cx, &obj(crate::TABLE, "T"), u32::MAX),
            "EVALUATE 'T'"
        );
        assert_eq!(
            browse_query(&cx, &obj(crate::CALCULATION_GROUP, "Time Intel"), 200),
            "EVALUATE 'Time Intel'"
        );
        assert_eq!(
            browse_query(&cx, &obj(crate::MEASURE, "Total [neto]"), 200),
            "EVALUATE { [Total [neto]]] }"
        );
        // A hierarchy without metadata yet: its table.
        assert_eq!(
            browse_query(&cx, &obj(crate::HIERARCHY, "'Date'[Calendar]"), 50),
            "EVALUATE TOPN(50, 'Date')"
        );
        cx.meta.cached = Some(Arc::new(ModelMeta {
            hierarchies: vec![Hierarchy {
                table: "Date".into(),
                name: "Calendar".into(),
                levels: vec![
                    Level {
                        name: "Año".into(),
                        column: "Year".into(),
                    },
                    Level {
                        name: "Mes".into(),
                        column: "Month".into(),
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }));
        assert_eq!(
            browse_query(&cx, &obj(crate::HIERARCHY, "'Date'[Calendar]"), 50),
            "EVALUATE TOPN(50, SUMMARIZECOLUMNS('Date'[Year], 'Date'[Month]), 'Date'[Year], ASC, 'Date'[Month], ASC)\nORDER BY 'Date'[Year] ASC, 'Date'[Month] ASC"
        );
    }

    #[test]
    fn filters_with_sql_null_semantics() {
        let q = filtered_browse(
            "EVALUATE TOPN(200, 'Sales')",
            &[
                f("Amount", FilterOp::Gt, vec![json!(0)]),
                f("Customer[Name]", FilterOp::Eq, vec![json!("O\"Brien")]),
                f("Note", FilterOp::IsNull, vec![]),
                f("Code", FilterOp::Contains, vec![json!("50%*?~")]),
                f("Id", FilterOp::In, vec![json!(1), json!(2)]),
                f("Day", FilterOp::Le, vec![json!("2024-01-31 13:45:00")]),
                f("Flag", FilterOp::TrueOrNull, vec![]),
            ],
        )
        .unwrap();
        assert_eq!(
            q,
            "EVALUATE TOPN(200, FILTER('Sales', (NOT ISBLANK('Sales'[Amount]) && 'Sales'[Amount] > 0) && 'Customer'[Name] == \"O\"\"Brien\" && ISBLANK('Sales'[Note]) && CONTAINSSTRING('Sales'[Code], \"50%~*~?~~\") && 'Sales'[Id] IN { 1, 2 } && (NOT ISBLANK('Sales'[Day]) && 'Sales'[Day] <= DATE(2024, 1, 31) + TIME(13, 45, 0)) && ('Sales'[Flag] == TRUE() || ISBLANK('Sales'[Flag]))))"
        );
        // BLANK and 0 in the same column: `==` and the ISBLANK guard keep
        // them apart (DAX's `=` and `<>` would match BLANK to 0).
        assert_eq!(
            condition(&f("x", FilterOp::Eq, vec![json!(0)]), "T").unwrap(),
            "'T'[x] == 0"
        );
        assert_eq!(
            condition(&f("x", FilterOp::Ne, vec![json!(0)]), "T").unwrap(),
            "(NOT ISBLANK('T'[x]) && NOT ('T'[x] == 0))"
        );
        assert_eq!(
            condition(&f("x", FilterOp::NotIn, vec![json!("a")]), "T").unwrap(),
            "(NOT ISBLANK('T'[x]) && NOT ('T'[x] IN { \"a\" }))"
        );
        assert_eq!(
            condition(&f("x", FilterOp::StartsWith, vec![json!("añ")]), "T").unwrap(),
            "LEFT('T'[x], 2) = \"añ\""
        );
        let typed = ColumnFilter {
            column: "x".into(),
            op: FilterOp::SqlRight,
            values: vec![],
            sql: Some("IN {1, 2}".into()),
        };
        assert_eq!(condition(&typed, "T").unwrap(), "('T'[x] IN {1, 2})");
        assert!(condition(&f("x", FilterOp::Eq, vec![]), "T").is_err());
        assert!(condition(&f("x", FilterOp::In, vec![]), "T").is_err());
        // The whole table.
        assert_eq!(
            filtered_browse("EVALUATE 'T'", &[f("a", FilterOp::NotNull, vec![])]).unwrap(),
            "EVALUATE FILTER('T', NOT ISBLANK('T'[a]))"
        );
        // Not one of its browses: the grid filters.
        for q in [
            "EVALUATE { [M] }",
            "EVALUATE SUMMARIZECOLUMNS('T'[a])",
            "EVALUATE TOPN(5, 'T') EVALUATE 'U'",
            "EVALUATE TOPN(5, 'T', 'T'[a], ASC)",
        ] {
            assert!(matches!(
                filtered_browse(q, &[f("a", FilterOp::NotNull, vec![])]),
                Err(Error::Unsupported(_))
            ));
        }
        // Text for a typed comparison isn't written into the query.
        assert!(condition(&f("x", FilterOp::Eq, vec![json!([1])]), "T").is_err());
    }

    #[test]
    fn sorts_after_filters() {
        let filtered = filtered_browse(
            "EVALUATE TOPN(200, 'T')",
            &[f("a", FilterOp::Eq, vec![json!("x, (y)")])],
        )
        .unwrap();
        let b = parse_browse(&filtered).unwrap();
        assert_eq!(b.filter.as_deref(), Some("'T'[a] == \"x, (y)\""));
        let sorted = sorted_browse(
            &filtered,
            &[
                SortKey {
                    column: "a".into(),
                    descending: true,
                },
                SortKey {
                    column: "b".into(),
                    descending: false,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            sorted,
            "EVALUATE TOPN(200, FILTER('T', 'T'[a] == \"x, (y)\"), 'T'[a], DESC, 'T'[b], ASC)\nORDER BY 'T'[a] DESC, 'T'[b] ASC"
        );
        assert!(parse_browse(&sorted).unwrap().sorted);
        // Sorting twice isn't stacked.
        assert!(matches!(
            sorted_browse(
                &sorted,
                &[SortKey {
                    column: "a".into(),
                    descending: false
                }]
            ),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(
            sorted_browse(
                "EVALUATE 'T'",
                &[SortKey {
                    column: "a".into(),
                    descending: false
                }]
            )
            .unwrap(),
            "EVALUATE 'T'\nORDER BY 'T'[a] ASC"
        );
        assert!(matches!(
            sorted_browse(
                "EVALUATE { 1 }",
                &[SortKey {
                    column: "a".into(),
                    descending: false
                }]
            ),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn datetimes_are_recognized() {
        for s in [
            "2024-01-31",
            "2024-01-31 13:45",
            "2024-01-31T13:45:00",
            "2024-01-31 13:45:00.123",
        ] {
            assert!(looks_like_datetime(s), "{s}");
        }
        for s in ["2024-1-31", "2024-01-31 13", "hola", "2024-01-31 13:45:00."] {
            assert!(!looks_like_datetime(s), "{s}");
        }
    }
}

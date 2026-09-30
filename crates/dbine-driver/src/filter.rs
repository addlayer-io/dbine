//! Column filters of the data grid (the filter row under the headers):
//! what the user asked for, and how SQL engines turn it into a WHERE added
//! to their own browse query ([`crate::Driver::filtered_browse`]).

use crate::sql::{quote_ident, Quote};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    IsNull,
    NotNull,
    /// `= ''`.
    IsEmpty,
    NotEmpty,
    /// Any of `values`.
    In,
    NotIn,
    IsTrue,
    IsFalse,
    TrueOrNull,
    FalseOrNull,
    /// `sql` is a whole condition, as typed ("SQL condition…").
    Sql,
    /// `sql` goes right after the column ("SQL condition – right side…").
    SqlRight,
}

/// One column's filter; the row's filters combine with AND.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnFilter {
    pub column: String,
    pub op: FilterOp,
    /// The value(s) compared with, typed (numbers stay numbers).
    #[serde(default)]
    pub values: Vec<Value>,
    /// For `Sql` / `SqlRight`.
    #[serde(default)]
    pub sql: Option<String>,
}

/// How an SQL engine writes a filter.
pub struct SqlFilterStyle<'a> {
    pub quote: Quote,
    /// A value as a literal of the engine.
    pub literal: &'a dyn Fn(&Value) -> String,
    /// Case-insensitive pattern match (`ILIKE` in PostgreSQL); `LIKE` otherwise.
    pub like: &'static str,
    /// What `IS TRUE` compares with (`TRUE`, or `1` where booleans are bits).
    pub true_literal: &'static str,
    pub false_literal: &'static str,
}

/// `%`, `_` and `\` inside a LIKE pattern, escaped with `\`.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The filters as one SQL condition (joined with AND).
pub fn sql_condition(filters: &[ColumnFilter], st: &SqlFilterStyle) -> Result<String> {
    let mut parts = Vec::new();
    for f in filters {
        let c = quote_ident(st.quote, &f.column);
        let lit = |v: &Value| (st.literal)(v);
        let first = || f.values.first().cloned().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let like = |pattern: String, not: bool| {
            let esc = if pattern.contains('\\') { " ESCAPE '\\'" } else { "" };
            format!("{c} {}{} {}{esc}", if not { "NOT " } else { "" }, st.like, lit(&Value::String(pattern)))
        };
        let cond = match f.op {
            FilterOp::Eq => format!("{c} = {}", lit(&first()?)),
            FilterOp::Ne => format!("{c} <> {}", lit(&first()?)),
            FilterOp::Gt => format!("{c} > {}", lit(&first()?)),
            FilterOp::Ge => format!("{c} >= {}", lit(&first()?)),
            FilterOp::Lt => format!("{c} < {}", lit(&first()?)),
            FilterOp::Le => format!("{c} <= {}", lit(&first()?)),
            FilterOp::Contains => like(format!("%{}%", like_escape(&text_of(&first()?))), false),
            FilterOp::NotContains => like(format!("%{}%", like_escape(&text_of(&first()?))), true),
            FilterOp::StartsWith => like(format!("{}%", like_escape(&text_of(&first()?))), false),
            FilterOp::EndsWith => like(format!("%{}", like_escape(&text_of(&first()?))), false),
            FilterOp::IsNull => format!("{c} IS NULL"),
            FilterOp::NotNull => format!("{c} IS NOT NULL"),
            FilterOp::IsEmpty => format!("{c} = ''"),
            FilterOp::NotEmpty => format!("({c} IS NOT NULL AND {c} <> '')"),
            FilterOp::In | FilterOp::NotIn => {
                if f.values.is_empty() {
                    return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
                }
                let list: Vec<String> = f.values.iter().map(lit).collect();
                format!("{c} {}IN ({})", if f.op == FilterOp::NotIn { "NOT " } else { "" }, list.join(", "))
            }
            FilterOp::IsTrue => format!("{c} = {}", st.true_literal),
            FilterOp::IsFalse => format!("{c} = {}", st.false_literal),
            FilterOp::TrueOrNull => format!("({c} = {} OR {c} IS NULL)", st.true_literal),
            FilterOp::FalseOrNull => format!("({c} = {} OR {c} IS NULL)", st.false_literal),
            FilterOp::Sql => format!("({})", f.sql.as_deref().unwrap_or("").trim()),
            FilterOp::SqlRight => format!("{c} {}", f.sql.as_deref().unwrap_or("").trim()),
        };
        parts.push(cond);
    }
    Ok(parts.join("\n  AND "))
}

/// Add `WHERE cond` to a browse query (`SELECT … FROM t [LIMIT n]`,
/// `SELECT TOP (n) * FROM t`, `… FETCH FIRST n ROWS ONLY`): right after the
/// table, before the LIMIT / FETCH / `;`. `None` when there's no FROM or the
/// query already filters.
pub fn insert_where(query: &str, cond: &str) -> Option<String> {
    let lower = query.to_ascii_lowercase();
    let from = lower.find("from ")?;
    if lower[from..].contains(" where ") || lower[from..].contains("\nwhere ") {
        return None;
    }
    let after_from = from + 5;
    // Where the table reference ends.
    let rest = &lower[after_from..];
    let mut end = rest.len();
    for stop in ["\n", " limit ", " fetch ", " order by ", ";"] {
        if let Some(i) = rest.find(stop) {
            end = end.min(i);
        }
    }
    let at = after_from + end;
    let (head, tail) = query.split_at(at);
    Some(format!("{}\nWHERE {cond}{}{}", head.trim_end(), if tail.starts_with('\n') || tail.is_empty() { "" } else { "\n" }, tail.trim_start_matches(' ')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn style() -> SqlFilterStyle<'static> {
        SqlFilterStyle {
            quote: Quote::Double,
            literal: &|v| match v {
                Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                Value::Null => "NULL".into(),
                other => other.to_string(),
            },
            like: "LIKE",
            true_literal: "TRUE",
            false_literal: "FALSE",
        }
    }

    fn f(column: &str, op: FilterOp, values: Vec<Value>) -> ColumnFilter {
        ColumnFilter { column: column.into(), op, values, sql: None }
    }

    #[test]
    fn conditions() {
        let c = sql_condition(
            &[
                f("nombre", FilterOp::Contains, vec![json!("O'Brien 50%")]),
                f("id", FilterOp::In, vec![json!(1), json!(2)]),
                f("activo", FilterOp::TrueOrNull, vec![]),
                f("baja", FilterOp::IsNull, vec![]),
                ColumnFilter { column: "total".into(), op: FilterOp::SqlRight, values: vec![], sql: Some("BETWEEN 1 AND 5".into()) },
            ],
            &style(),
        )
        .unwrap();
        assert_eq!(
            c,
            "\"nombre\" LIKE '%O''Brien 50\\%%' ESCAPE '\\'\n  AND \"id\" IN (1, 2)\n  AND (\"activo\" = TRUE OR \"activo\" IS NULL)\n  AND \"baja\" IS NULL\n  AND \"total\" BETWEEN 1 AND 5"
        );
        assert!(sql_condition(&[f("x", FilterOp::Eq, vec![])], &style()).is_err());
    }

    #[test]
    fn where_goes_after_the_table() {
        assert_eq!(insert_where("SELECT *\nFROM \"t\"\nLIMIT 200", "a = 1").unwrap(), "SELECT *\nFROM \"t\"\nWHERE a = 1\nLIMIT 200");
        assert_eq!(insert_where("SELECT TOP (200) *\nFROM [dbo].[t]", "a = 1").unwrap(), "SELECT TOP (200) *\nFROM [dbo].[t]\nWHERE a = 1");
        assert_eq!(insert_where("SELECT * FROM ks.t LIMIT 50;", "a = 1").unwrap(), "SELECT * FROM ks.t\nWHERE a = 1\nLIMIT 50;");
        assert_eq!(
            insert_where("SELECT *\nFROM \"S\".\"T\"\nFETCH FIRST 10 ROWS ONLY", "a = 1").unwrap(),
            "SELECT *\nFROM \"S\".\"T\"\nWHERE a = 1\nFETCH FIRST 10 ROWS ONLY"
        );
        assert!(insert_where("SHOW TABLES", "a = 1").is_none());
    }
}

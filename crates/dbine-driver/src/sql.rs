//! Helpers for SQL drivers: identifier quoting, `SELECT` of the first rows,
//! a `CREATE TABLE` from a column list.

use crate::model::ColumnInfo;

mod script;
pub use script::{
    expose_versioned, leading_keyword, split_script, strip_comments, unsafe_dml, unsafe_statements, BatchLine, ScriptDefaults, ScriptDialect, ScriptMode,
    ScriptStatement, StatementKind, UnsafeStatement, GO_COUNT_ERROR,
};

/// How a dialect quotes identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quote {
    /// `"name"`: ANSI (PostgreSQL, SQLite, Oracle, DB2, Trino, DuckDB…).
    Double,
    /// `` `name` ``: MySQL family, ClickHouse, BigQuery, Hive.
    Backtick,
    /// `[name]`: SQL Server, Sybase.
    Bracket,
}

/// How a dialect limits a `SELECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// `LIMIT n` at the end.
    Limit,
    /// `SELECT TOP (n)`.
    Top,
    /// `FETCH FIRST n ROWS ONLY` (Oracle 12c+, DB2).
    FetchFirst,
}

pub fn quote_ident(q: Quote, name: &str) -> String {
    match q {
        Quote::Double => format!("\"{}\"", name.replace('"', "\"\"")),
        Quote::Backtick => format!("`{}`", name.replace('`', "``")),
        Quote::Bracket => format!("[{}]", name.replace(']', "]]")),
    }
}

pub fn qualified_name(q: Quote, schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(q, s), quote_ident(q, name)),
        _ => quote_ident(q, name),
    }
}

/// `SELECT` of the first `limit` rows of a table or view.
pub fn select_top(q: Quote, l: Limit, schema: Option<&str>, name: &str, limit: u32) -> String {
    let t = qualified_name(q, schema, name);
    match l {
        Limit::Limit => format!("SELECT *\nFROM {t}\nLIMIT {limit}"),
        Limit::Top => format!("SELECT TOP ({limit}) *\nFROM {t}"),
        Limit::FetchFirst => format!("SELECT *\nFROM {t}\nFETCH FIRST {limit} ROWS ONLY"),
    }
}

/// A plain `CREATE TABLE` from the column list, for engines that don't
/// hand one out. Keys, defaults and nullability; no indexes or foreign keys.
pub fn create_table_from_columns(q: Quote, schema: Option<&str>, name: &str, cols: &[ColumnInfo]) -> String {
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote_ident(q, &c.name), c.data_type);
            if let Some(d) = &c.default_value {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            l.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            l
        })
        .collect();
    let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| quote_ident(q, &c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {} (\n{}\n);", qualified_name(q, schema, name), lines.join(",\n"))
}

/// Split a script on `;` outside quotes and comments, dropping empty
/// statements and comments. For drivers whose server takes one statement
/// per request (HTTP APIs, CQL…). The generic [`split_script`] underneath:
/// '…', "…", `…`, -- and /* */, trigger and routine bodies kept whole.
pub fn split_statements(sql: &str) -> Vec<String> {
    let d = ScriptDialect::generic();
    split_script(sql, &d)
        .into_iter()
        .filter(|s| s.kind != StatementKind::ClientCommand)
        .map(|s| strip_comments(&s.text, &d, false).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_per_dialect() {
        assert_eq!(quote_ident(Quote::Bracket, "a]b"), "[a]]b]");
        assert_eq!(quote_ident(Quote::Backtick, "a`b"), "`a``b`");
        assert_eq!(quote_ident(Quote::Double, "a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn select_top_per_dialect() {
        assert_eq!(select_top(Quote::Bracket, Limit::Top, Some("dbo"), "t", 10), "SELECT TOP (10) *\nFROM [dbo].[t]");
        assert_eq!(select_top(Quote::Double, Limit::Limit, None, "t", 10), "SELECT *\nFROM \"t\"\nLIMIT 10");
    }

    #[test]
    fn statements_split_outside_quotes_and_comments() {
        let s = split_statements("select ';' ; -- x;\nselect 2; /* ; */ ;");
        assert_eq!(s, vec!["select ';'", "select 2"]);
    }
}

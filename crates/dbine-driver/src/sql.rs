//! Helpers for SQL drivers: identifier quoting, `SELECT` of the first rows,
//! a `CREATE TABLE` from a column list.

use crate::model::ColumnInfo;

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
/// statements. For drivers whose server takes one statement per request
/// (HTTP APIs, CQL…). Handles '…', "…", `…`, -- and /* */.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                cur.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        cur.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                cur.push(' ');
            }
            ';' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
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

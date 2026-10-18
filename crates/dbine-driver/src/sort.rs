//! Column sorting of the data grid (clicking a header): what the user asked
//! for, and how SQL and CQL engines turn it into an `ORDER BY` in their own
//! browse query ([`crate::Driver::sorted_browse`]).

use crate::sql::{quote_ident, Quote};
use serde::{Deserialize, Serialize};

/// One sort key; the first is the primary one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

/// `"a" ASC, "b" DESC` with the engine's identifier quoting.
pub fn order_list(sort: &[SortKey], quote: Quote) -> String {
    sort.iter()
        .map(|k| format!("{} {}", quote_ident(quote, &k.column), if k.descending { "DESC" } else { "ASC" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where the clauses of a query start, at the top level (outside quotes,
/// comments and parentheses).
#[derive(Default)]
pub(crate) struct Clauses {
    pub(crate) from: Option<usize>,
    pub(crate) order_by: Option<usize>,
    /// The first clause that goes after `ORDER BY` (`LIMIT`, `OFFSET`,
    /// `FETCH`, CQL's `ALLOW FILTERING`, `;`), after the `FROM`.
    pub(crate) tail: Option<usize>,
}

pub(crate) fn clauses(query: &str, backslash: bool) -> Clauses {
    let b = query.as_bytes();
    let lower = query.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let word_at = |i: usize, w: &str| -> bool {
        lower[i..].starts_with(w)
            && (i == 0 || !(lb[i - 1].is_ascii_alphanumeric() || lb[i - 1] == b'_'))
            && lb.get(i + w.len()).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
    };
    let mut c = Clauses::default();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    // MySQL, ClickHouse, BigQuery…: `\'` inside a string.
                    if backslash && b[i] == b'\\' && q != b'`' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        // A doubled quote is an escaped one.
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth == 0 && c.from.is_some() => {
                c.tail.get_or_insert(i);
                break;
            }
            _ if depth == 0 => {
                if c.from.is_none() {
                    if word_at(i, "from") {
                        c.from = Some(i);
                    }
                } else if c.tail.is_none() {
                    if c.order_by.is_none() && word_at(i, "order") && lower[i + 5..].trim_start().starts_with("by") {
                        c.order_by = Some(i);
                    } else if ["limit", "offset", "fetch", "allow"].iter().any(|w| word_at(i, w)) {
                        c.tail = Some(i);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    c
}

/// Put `ORDER BY order` in a browse query (`SELECT … FROM t [WHERE …]
/// [ORDER BY …] [LIMIT n | FETCH FIRST n ROWS ONLY] [;]`, `SELECT TOP (n) *
/// FROM t`, `SELECT FIRST n * FROM t`): it replaces the query's own
/// `ORDER BY`, or goes after the `WHERE` and before the `LIMIT` / `OFFSET` /
/// `FETCH` / `ALLOW FILTERING` / `;`. `None` when there's no `FROM`.
pub fn set_order_by(query: &str, order: &str) -> Option<String> {
    set_order_by_in(query, order, false)
}

/// [`set_order_by`] for dialects whose strings take backslash escapes
/// (`'O\\'Brien'`) when `backslash` is set.
pub fn set_order_by_in(query: &str, order: &str, backslash: bool) -> Option<String> {
    let c = clauses(query, backslash);
    c.from?;
    let tail_at = c.tail.unwrap_or(query.len());
    let head_end = c.order_by.unwrap_or(tail_at);
    let head = query[..head_end].trim_end();
    let tail = query[tail_at..].trim_start();
    Some(if tail.is_empty() || tail.starts_with(';') {
        format!("{head}\nORDER BY {order}{tail}")
    } else {
        format!("{head}\nORDER BY {order}\n{tail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::insert_where;
    use crate::sql::{select_top, Limit};

    fn keys(k: &[(&str, bool)]) -> Vec<SortKey> {
        k.iter().map(|(c, d)| SortKey { column: c.to_string(), descending: *d }).collect()
    }

    fn sorted(q: &str, quote: Quote, k: &[(&str, bool)]) -> String {
        set_order_by(q, &order_list(&keys(k), quote)).unwrap()
    }

    #[test]
    fn order_list_quotes_per_dialect() {
        let k = keys(&[("a b", false), ("c", true)]);
        assert_eq!(order_list(&k, Quote::Double), "\"a b\" ASC, \"c\" DESC");
        assert_eq!(order_list(&k, Quote::Bracket), "[a b] ASC, [c] DESC");
        assert_eq!(order_list(&k, Quote::Backtick), "`a b` ASC, `c` DESC");
    }

    #[test]
    fn limit_shape() {
        // postgres, sqlite, duckdb, mysql, snowflake, trino, hana… (select_top Limit)
        let q = select_top(Quote::Double, Limit::Limit, Some("s"), "t", 200);
        assert_eq!(sorted(&q, Quote::Double, &[("x", true)]), "SELECT *\nFROM \"s\".\"t\"\nORDER BY \"x\" DESC\nLIMIT 200");
        let q = select_top(Quote::Backtick, Limit::Limit, Some("db"), "t", 200);
        let w = insert_where(&q, "`a` = 1").unwrap();
        assert_eq!(
            sorted(&w, Quote::Backtick, &[("x", false), ("y", true)]),
            "SELECT *\nFROM `db`.`t`\nWHERE `a` = 1\nORDER BY `x` ASC, `y` DESC\nLIMIT 200"
        );
    }

    #[test]
    fn top_shape() {
        // sqlserver
        let q = select_top(Quote::Bracket, Limit::Top, Some("dbo"), "t", 200);
        assert_eq!(sorted(&q, Quote::Bracket, &[("x", false)]), "SELECT TOP (200) *\nFROM [dbo].[t]\nORDER BY [x] ASC");
        let w = insert_where(&q, "[a] = 1").unwrap();
        assert_eq!(sorted(&w, Quote::Bracket, &[("x", true)]), "SELECT TOP (200) *\nFROM [dbo].[t]\nWHERE [a] = 1\nORDER BY [x] DESC");
        // odbc presets: TOP n and FIRST n (sybase, access, teradata, informix, firebird)
        let q = "SELECT TOP 50 *\nFROM \"t\"";
        assert_eq!(sorted(q, Quote::Double, &[("x", false)]), "SELECT TOP 50 *\nFROM \"t\"\nORDER BY \"x\" ASC");
        let q = "SELECT FIRST 50 *\nFROM \"T\"";
        let w = insert_where(q, "\"A\" = 1").unwrap();
        assert_eq!(sorted(&w, Quote::Double, &[("X", true)]), "SELECT FIRST 50 *\nFROM \"T\"\nWHERE \"A\" = 1\nORDER BY \"X\" DESC");
    }

    #[test]
    fn fetch_first_shape() {
        // oracle, db2 and the odbc FetchFirst presets
        let q = select_top(Quote::Double, Limit::FetchFirst, Some("HR"), "EMP", 10);
        assert_eq!(sorted(&q, Quote::Double, &[("SAL", true)]), "SELECT *\nFROM \"HR\".\"EMP\"\nORDER BY \"SAL\" DESC\nFETCH FIRST 10 ROWS ONLY");
        let w = insert_where(&q, "\"DEPT\" = 10").unwrap();
        assert_eq!(
            sorted(&w, Quote::Double, &[("SAL", true), ("ENAME", false)]),
            "SELECT *\nFROM \"HR\".\"EMP\"\nWHERE \"DEPT\" = 10\nORDER BY \"SAL\" DESC, \"ENAME\" ASC\nFETCH FIRST 10 ROWS ONLY"
        );
    }

    #[test]
    fn cql_and_semicolon_shape() {
        // cassandra: one line, LIMIT and `;`
        let q = "SELECT * FROM ks.t LIMIT 50;";
        assert_eq!(sorted(q, Quote::Double, &[("ts", true)]), "SELECT * FROM ks.t\nORDER BY \"ts\" DESC\nLIMIT 50;");
        let w = "SELECT * FROM ks.t WHERE \"k\" = 1 LIMIT 50 ALLOW FILTERING;";
        assert_eq!(sorted(w, Quote::Double, &[("ts", false)]), "SELECT * FROM ks.t WHERE \"k\" = 1\nORDER BY \"ts\" ASC\nLIMIT 50 ALLOW FILTERING;");
        assert_eq!(sorted("SELECT * FROM t;", Quote::Double, &[("a", false)]), "SELECT * FROM t\nORDER BY \"a\" ASC;");
    }

    #[test]
    fn replaces_the_query_own_order() {
        // influxdb3, iotdb: ORDER BY time DESC LIMIT n
        let q = "SELECT *\nFROM \"cpu\"\nORDER BY time DESC\nLIMIT 100";
        assert_eq!(sorted(q, Quote::Double, &[("host", false)]), "SELECT *\nFROM \"cpu\"\nORDER BY \"host\" ASC\nLIMIT 100");
        let w = insert_where(q, "\"host\" = 'a'").unwrap();
        assert_eq!(sorted(&w, Quote::Double, &[("usage", true)]), "SELECT *\nFROM \"cpu\"\nWHERE \"host\" = 'a'\nORDER BY \"usage\" DESC\nLIMIT 100");
    }

    #[test]
    fn no_limit_shape() {
        // dynamodb (PartiQL), ODBC without a limit
        assert_eq!(sorted("SELECT * FROM \"t\"", Quote::Double, &[("a", false)]), "SELECT * FROM \"t\"\nORDER BY \"a\" ASC");
    }

    #[test]
    fn keywords_inside_names_dont_count() {
        let q = "SELECT *\nFROM \"limit order\".\"fetch;from\"\nWHERE \"x\" = 'LIMIT 3; ORDER BY y'\nLIMIT 5";
        assert_eq!(
            sorted(q, Quote::Double, &[("offset", true)]),
            "SELECT *\nFROM \"limit order\".\"fetch;from\"\nWHERE \"x\" = 'LIMIT 3; ORDER BY y'\nORDER BY \"offset\" DESC\nLIMIT 5"
        );
        let q = "SELECT TOP (5) *\nFROM [order].[limit]";
        assert_eq!(sorted(q, Quote::Bracket, &[("a", false)]), "SELECT TOP (5) *\nFROM [order].[limit]\nORDER BY [a] ASC");
        // A subquery's own LIMIT isn't the outer one.
        let q = "SELECT * FROM (SELECT * FROM t LIMIT 3) s LIMIT 5";
        assert_eq!(sorted(q, Quote::Double, &[("a", false)]), "SELECT * FROM (SELECT * FROM t LIMIT 3) s\nORDER BY \"a\" ASC\nLIMIT 5");
        // The table is a function call (Timeplus streams).
        let q = "SELECT *\nFROM table(`s`)\nLIMIT 5";
        assert_eq!(sorted(q, Quote::Backtick, &[("a", false)]), "SELECT *\nFROM table(`s`)\nORDER BY `a` ASC\nLIMIT 5");
    }

    #[test]
    fn backslash_escaped_quotes() {
        let q = "SELECT * FROM `t` WHERE `a` = 'O\\'Brien LIMIT 9' LIMIT 200";
        assert_eq!(
            set_order_by_in(q, "`b` ASC", true).unwrap(),
            "SELECT * FROM `t` WHERE `a` = 'O\\'Brien LIMIT 9'\nORDER BY `b` ASC\nLIMIT 200"
        );
        // Standard strings: the backslash is a plain character.
        let q = "SELECT * FROM \"t\" WHERE \"a\" = 'x\\' LIMIT 200";
        assert_eq!(set_order_by_in(q, "\"b\" ASC", false).unwrap(), "SELECT * FROM \"t\" WHERE \"a\" = 'x\\'\nORDER BY \"b\" ASC\nLIMIT 200");
    }

    #[test]
    fn needs_a_from() {
        assert!(set_order_by("SHOW TABLES", "a").is_none());
        assert!(set_order_by("MATCH (n) RETURN n LIMIT 5", "a").is_none());
    }
}

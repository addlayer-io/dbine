//! Values for a statement's parameters (notebook parameters,
//! docs/notebooks.md#parameters). A value reaches the engine one of two
//! ways, never as raw text:
//!
//! - **bound**: the driver sends it apart from the statement
//!   ([`crate::Session::execute_bound`], with the driver's own placeholders);
//! - **literal**: written into the statement by the engine's quoting
//!   ([`crate::Driver::param_literal`], by default [`literal`]).
//!
//! Every value is checked when it is made ([`ParamValue::number`],
//! [`ParamValue::date`]…): a number is only digits, a sign, a point and an
//! exponent; a date only digits and separators. Only text needs quoting.

use crate::{Error, Language, Result};
use serde::{Deserialize, Serialize};

/// A typed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ParamValue {
    Null,
    Text(String),
    /// A decimal number as written (`-12`, `3.50`, `1e3`); see [`ParamValue::number`].
    Number(String),
    Bool(bool),
    /// `YYYY-MM-DD`.
    Date(String),
    /// `YYYY-MM-DDTHH:MM:SS[.fff]`, no time zone.
    DateTime(String),
}

impl ParamValue {
    /// A number, or `None` when `s` isn't one (`1`, `-2.5`, `.5`, `1e-3`).
    pub fn number(s: &str) -> Option<Self> {
        let s = s.trim();
        is_number(s).then(|| ParamValue::Number(s.trim_start_matches('+').to_string()))
    }

    /// A date (`2026-10-18`), or `None`.
    pub fn date(s: &str) -> Option<Self> {
        let s = s.trim();
        is_date(s).then(|| ParamValue::Date(s.to_string()))
    }

    /// A date and time (`2026-10-18T14:30`, `2026-10-18 14:30:05.250`),
    /// or `None`. Kept as `YYYY-MM-DDTHH:MM:SS[.fff]`.
    pub fn datetime(s: &str) -> Option<Self> {
        let s = s.trim();
        // `get`: byte 10 may fall inside a character (`2026-10-1é…`). Past
        // the ASCII date and its ASCII separator, every slice below is
        // after ASCII bytes checked first.
        let Some(date) = s.get(..10) else { return None };
        if s.len() < 16 || !is_date(date) || !matches!(s.as_bytes()[10], b'T' | b' ') {
            return None;
        }
        let time = &s[11..];
        let b = time.as_bytes();
        let two = |i: usize| b.len() >= i + 2 && b[i].is_ascii_digit() && b[i + 1].is_ascii_digit();
        let hm = two(0) && b[2] == b':' && two(3);
        if !hm {
            return None;
        }
        let (h, m) = (num(&time[0..2]), num(&time[3..5]));
        if h > 23 || m > 59 {
            return None;
        }
        let rest = &time[5..];
        let full = match rest.as_bytes() {
            [] => format!("{}:00", &time[..5]),
            [b':', a, c, tail @ ..]
                if a.is_ascii_digit() && c.is_ascii_digit() && num(&rest[1..3]) <= 59 =>
            {
                let ok = match tail {
                    [] => true,
                    [b'.', frac @ ..] => {
                        !frac.is_empty() && frac.len() <= 9 && frac.iter().all(u8::is_ascii_digit)
                    }
                    _ => false,
                };
                if !ok {
                    return None;
                }
                time.to_string()
            }
            _ => return None,
        };
        Some(ParamValue::DateTime(format!("{date}T{full}")))
    }

    /// The date and the time apart (`("2026-10-18", Some("14:30:00"))`).
    fn date_time(&self) -> (&str, Option<&str>) {
        match self {
            ParamValue::Date(d) => (d.as_str(), None),
            // Made by `datetime`, but a deserialized one wasn't checked.
            ParamValue::DateTime(dt) => (dt.get(..10).unwrap_or(dt), dt.get(11..)),
            _ => ("", None),
        }
    }
}

fn num(s: &str) -> u32 {
    s.parse().unwrap_or(u32::MAX)
}

fn is_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'-' | b'+')) {
        i = 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i > int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits |= i > f;
    }
    if !digits {
        return false;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if matches!(b.get(i), Some(b'-' | b'+')) {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || ![0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
    {
        return false;
    }
    let (m, d) = (num(&s[5..7]), num(&s[8..10]));
    (1..=12).contains(&m) && (1..=31).contains(&d)
}

/// A statement with its parameters apart: what [`crate::Session::execute_bound`]
/// gets. Each [`BoundPart::Param`] is an index into `values` (a value used
/// twice is the same index twice).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BoundStatement {
    pub parts: Vec<BoundPart>,
    pub values: Vec<ParamValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BoundPart {
    /// The statement's own text, as written.
    Text(String),
    /// A parameter: `values[i]`.
    Param(usize),
}

impl BoundStatement {
    /// Whether it has any parameter.
    pub fn has_params(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, BoundPart::Param(_)))
    }

    /// The text with each parameter written by `marker` (its index in
    /// `values`): `$1` for PostgreSQL-like numbering, the literal for
    /// [`BoundStatement::literal_text`].
    pub fn render(&self, mut marker: impl FnMut(usize) -> String) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                BoundPart::Text(t) => out.push_str(t),
                BoundPart::Param(i) => {
                    let m = marker(*i);
                    // `a -{{n}}` with n = -5 is `a - -5`, never a `--` comment.
                    if out.ends_with('-') && m.starts_with('-') {
                        out.push(' ');
                    }
                    out.push_str(&m);
                }
            }
        }
        out
    }

    /// Numbered placeholders, one per value (`@P1`, `:1`, `?1`…): `prefix`
    /// plus the 1-based index.
    pub fn numbered(&self, prefix: &str) -> String {
        self.render(|i| format!("{prefix}{}", i + 1))
    }

    /// `?` per occurrence, and the values in that order (a value used
    /// twice is sent twice).
    pub fn positional(&self) -> (String, Vec<&ParamValue>) {
        let mut values = Vec::new();
        let text = self.render(|i| {
            if let Some(v) = self.values.get(i) {
                values.push(v);
            }
            "?".to_string()
        });
        (text, values)
    }

    /// The statement with every value as a literal (`lit`): what runs when
    /// the driver doesn't bind.
    pub fn literal_text(&self, lit: &dyn Fn(&ParamValue) -> Result<String>) -> Result<String> {
        let mut lits = Vec::with_capacity(self.values.len());
        for v in &self.values {
            lits.push(lit(v)?);
        }
        Ok(self.render(|i| lits.get(i).cloned().unwrap_or_default()))
    }
}

/// Dialects whose `'…'` strings read backslash escapes (`\'`, `\\`).
const BACKSLASH: &[&str] = &[
    "mysql",
    "mariadb",
    "snowflake",
    "clickhouse",
    "hive",
    "sparksql",
    "databricks",
    "tdengine",
    "orientdb",
];
/// Of those, the ones that don't take `''` for a quote (it would end the
/// string there, or be two strings): a quote goes as `\'`.
const BACKSLASH_QUOTE: &[&str] = &[
    "clickhouse",
    "hive",
    "sparksql",
    "databricks",
    "tdengine",
    "orientdb",
];
/// GoogleSQL, Cosmos DB and N1QL: C-like strings (`\'`, `\\`, `\n`…, no raw line breaks).
const C_STRINGS: &[&str] = &["bigquery", "spanner", "cosmos", "n1ql"];
/// Dialects where a date is just its text (no `DATE '…'`).
const PLAIN_DATES: &[&str] = &[
    "sqlite",
    "clickhouse",
    "informix",
    "altibase",
    "cosmos",
    "n1ql",
    "partiql",
    "iotdb",
    "tdengine",
    "influxql",
    "influxdb3",
    "ksql",
    "orientdb",
    "phoenix",
    "etcd",
];
/// Dialects without TRUE / FALSE (or where 1 / 0 is what a boolean column takes).
const NUMERIC_BOOLS: &[&str] = &[
    "mssql", "sybase", "oracle", "sqlite", "teradata", "altibase", "cubrid", "access", "iotdb",
];
const MONGO: &[&str] = &["mongodb", "ferretdb", "documentdb", "cosmosdb_mongo"];

/// `s` in single quotes, the quote doubled: standard SQL.
pub fn standard_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `s` as a C-like string in `q` quotes: `\\`, `\q`, `\n`, `\r`, `\t` and
/// other control characters as `\u00XX` (or `\xXX` with `hex_bytes`).
fn c_string(s: &str, q: char, hex_bytes: bool) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push(q);
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c == q => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                if hex_bytes {
                    o.push_str(&format!("\\x{:02x}", c as u32));
                } else {
                    o.push_str(&format!("\\u{:04x}", c as u32));
                }
            }
            c => o.push(c),
        }
    }
    o.push(q);
    o
}

/// A SQL string literal of `dialect` (also CQL's, with `cql`).
pub fn sql_string(dialect: &str, s: &str) -> String {
    if C_STRINGS.contains(&dialect) {
        return c_string(s, '\'', false);
    }
    if BACKSLASH.contains(&dialect) {
        let s = s.replace('\\', "\\\\");
        return if BACKSLASH_QUOTE.contains(&dialect) {
            format!("'{}'", s.replace('\'', "\\'"))
        } else {
            format!("'{}'", s.replace('\'', "''"))
        };
    }
    match dialect {
        "mssql" | "sybase" => format!("N{}", standard_string(s)),
        // Where `standard_conforming_strings` may be off, an `E'…'` reads
        // backslashes the same either way (drivers with variants override
        // this; see `Driver::param_literal`).
        "postgres" if s.contains('\\') => {
            format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
        }
        _ => standard_string(s),
    }
}

/// The value as a literal of the engine's language: what
/// [`crate::Driver::param_literal`] returns unless the driver has its own.
/// Errors only where the language has no way to write it (NULL in Redis
/// or Flux).
pub fn literal(
    language: Language,
    dialect: &str,
    driver_id: &str,
    v: &ParamValue,
) -> Result<String> {
    let no_null = |what: &str| {
        Error::Unsupported(format!(
            "{what} no tiene un valor nulo: el parámetro necesita un valor"
        ))
    };
    Ok(match language {
        Language::Sql | Language::Cql => {
            let cql = language == Language::Cql;
            match v {
                ParamValue::Null => "NULL".into(),
                ParamValue::Bool(b) if !cql && dialect == "informix" => {
                    if *b { "'t'" } else { "'f'" }.into()
                }
                ParamValue::Bool(b) if !cql && NUMERIC_BOOLS.contains(&dialect) => {
                    if *b { "1" } else { "0" }.into()
                }
                ParamValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
                ParamValue::Number(n) => n.clone(),
                ParamValue::Text(s) if cql => standard_string(s),
                ParamValue::Text(s) => sql_string(dialect, s),
                ParamValue::Date(_) | ParamValue::DateTime(_) => {
                    let (d, t) = v.date_time();
                    let text = match t {
                        Some(t) => format!("{d} {t}"),
                        None => d.to_string(),
                    };
                    if cql || PLAIN_DATES.contains(&dialect) {
                        format!("'{text}'")
                    } else {
                        match dialect {
                            "mssql" => match t {
                                Some(t) => format!("CAST('{d}T{t}' AS datetime2)"),
                                None => format!("CAST('{d}' AS date)"),
                            },
                            "sybase" => format!(
                                "'{}{}'",
                                d.replace('-', ""),
                                t.map(|t| format!(" {t}")).unwrap_or_default()
                            ),
                            "access" => format!("#{text}#"),
                            _ if t.is_some() => format!("TIMESTAMP '{text}'"),
                            _ => format!("DATE '{text}'"),
                        }
                    }
                }
            }
        }
        Language::Json => match v {
            ParamValue::Null => "null".into(),
            ParamValue::Bool(b) => b.to_string(),
            ParamValue::Number(n) => n.clone(),
            ParamValue::Text(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
            ParamValue::Date(_) | ParamValue::DateTime(_) => {
                let (d, t) = v.date_time();
                let iso = format!("{d}T{}", t.unwrap_or("00:00:00"));
                if MONGO.contains(&driver_id) {
                    format!("ISODate(\"{iso}Z\")")
                } else if t.is_some() {
                    format!("\"{iso}\"")
                } else {
                    format!("\"{d}\"")
                }
            }
        },
        Language::Redis => match v {
            ParamValue::Null => return Err(no_null("Redis")),
            ParamValue::Bool(b) => if *b { "1" } else { "0" }.into(),
            ParamValue::Number(n) => n.clone(),
            ParamValue::Text(s) => c_string(s, '"', true),
            ParamValue::Date(_) | ParamValue::DateTime(_) => {
                let (d, t) = v.date_time();
                format!("\"{d}{}\"", t.map(|t| format!("T{t}")).unwrap_or_default())
            }
        },
        Language::Flux => match v {
            ParamValue::Null => return Err(no_null("Flux")),
            ParamValue::Bool(b) => b.to_string(),
            ParamValue::Number(n) => n.clone(),
            // `${` would start an interpolation.
            ParamValue::Text(s) => c_string(s, '"', false).replace("${", "\\${"),
            ParamValue::Date(_) | ParamValue::DateTime(_) => {
                let (d, t) = v.date_time();
                format!("{d}T{}Z", t.unwrap_or("00:00:00"))
            }
        },
        Language::Cypher => match v {
            ParamValue::Null => "null".into(),
            ParamValue::Bool(b) => b.to_string(),
            ParamValue::Number(n) => n.clone(),
            ParamValue::Text(s) => c_string(s, '\'', false),
            ParamValue::Date(_) | ParamValue::DateTime(_) => {
                let (d, t) = v.date_time();
                match (dialect, t) {
                    ("neptune", Some(t)) => format!("datetime('{d}T{t}')"),
                    ("neptune", None) => format!("datetime('{d}T00:00:00')"),
                    (_, Some(t)) => format!("localdatetime('{d}T{t}')"),
                    (_, None) => format!("date('{d}')"),
                }
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(dialect: &str, v: &ParamValue) -> String {
        literal(Language::Sql, dialect, "x", v).unwrap()
    }
    fn text(s: &str) -> ParamValue {
        ParamValue::Text(s.into())
    }

    #[test]
    fn values_are_checked_when_made() {
        for ok in ["1", "-2.5", "+3", ".5", "1e3", "1.2E-3", "0"] {
            assert!(ParamValue::number(ok).is_some(), "{ok}");
        }
        for bad in [
            "", "-", "1;", "1 OR 1=1", "0x10", "1e", "--1", "1.2.3", "NaN", "١",
        ] {
            assert!(ParamValue::number(bad).is_none(), "{bad}");
        }
        assert_eq!(
            ParamValue::number("+3"),
            Some(ParamValue::Number("3".into()))
        );
        assert_eq!(
            ParamValue::date("2026-10-18"),
            Some(ParamValue::Date("2026-10-18".into()))
        );
        for bad in [
            "2026-13-01",
            "2026-1-01",
            "2026-10-18'",
            "18/10/2026",
            "2026-10-18 00:00",
        ] {
            assert!(ParamValue::date(bad).is_none(), "{bad}");
        }
        assert_eq!(
            ParamValue::datetime("2026-10-18 14:30"),
            Some(ParamValue::DateTime("2026-10-18T14:30:00".into()))
        );
        assert_eq!(
            ParamValue::datetime("2026-10-18T14:30:05.25"),
            Some(ParamValue::DateTime("2026-10-18T14:30:05.25".into()))
        );
        for bad in [
            "2026-10-18",
            "2026-10-18T25:00",
            "2026-10-18T14:30:05'",
            "2026-10-18T14:30Z",
            "2026-10-18T14:30:05.",
            "2026-10-18x14:30",
        ] {
            assert!(ParamValue::datetime(bad).is_none(), "{bad}");
        }
        // A character across byte 10, or after it: refused, not a panic.
        for bad in [
            "2026-10-1é 00:00:00",
            "2026-10-18é00:00:00",
            "2026-10-18 é0:00:00",
            "2026-10-18 00:0é:00",
            "2026-10-18 00:00:é0",
            "2026-10-18 00:00:00.é",
            "ééééééééééé",
        ] {
            assert!(ParamValue::datetime(bad).is_none(), "{bad}");
        }
        for bad in ["2026-10-é", "2026-1é-18", "é026-10-18"] {
            assert!(ParamValue::date(bad).is_none(), "{bad}");
        }
        // A deserialized value isn't checked: still no panic.
        for v in [
            ParamValue::DateTime("2026-10-1é 00:00".into()),
            ParamValue::DateTime("x".into()),
            ParamValue::Date("é".into()),
        ] {
            for lang in [
                Language::Sql,
                Language::Json,
                Language::Redis,
                Language::Flux,
                Language::Cypher,
            ] {
                let _ = literal(lang, "postgres", "x", &v);
            }
        }
    }

    #[test]
    fn text_is_quoted_per_dialect() {
        let evil = "'; DROP TABLE x; --";
        assert_eq!(sql("standard", &text(evil)), "'''; DROP TABLE x; --'");
        assert_eq!(sql("oracle", &text("O'Brien")), "'O''Brien'");
        assert_eq!(sql("mssql", &text("O'Brien")), "N'O''Brien'");
        assert_eq!(sql("mysql", &text("a\\'b")), "'a\\\\''b'");
        assert_eq!(sql("clickhouse", &text("a\\'b")), "'a\\\\\\'b'");
        assert_eq!(sql("bigquery", &text("a'b\nc")), "'a\\'b\\nc'");
        assert_eq!(sql("postgres", &text("a'b")), "'a''b'");
        assert_eq!(sql("postgres", &text("a\\'b")), "E'a\\\\''b'");
        assert_eq!(
            literal(Language::Cql, "", "cassandra", &text("O'Brien")).unwrap(),
            "'O''Brien'"
        );
    }

    /// Whatever the text, the literal is one string: read back with each
    /// dialect's rules, it ends at its last character and holds the text.
    #[test]
    fn a_literal_never_ends_early() {
        let nasty = [
            "'; DROP TABLE x; --",
            "\\'; DROP TABLE x; --",
            "\\",
            "'",
            "''",
            "\\\\'",
            "a\nb'\r--",
            "$$",
            "\u{0}'",
        ];
        for d in [
            "standard",
            "postgres",
            "mysql",
            "snowflake",
            "clickhouse",
            "databricks",
            "bigquery",
            "mssql",
            "oracle",
            "sqlite",
        ] {
            for s in nasty {
                let lit = sql(d, &text(s));
                let body = lit.trim_start_matches(['N', 'E']);
                let backslash = d != "standard"
                    && d != "mssql"
                    && d != "oracle"
                    && d != "sqlite"
                    && !(d == "postgres" && !lit.starts_with('E'));
                assert_eq!(
                    read_string(body, backslash),
                    Some((s.to_string(), body.len())),
                    "{d}: {s:?} -> {lit}"
                );
            }
        }
    }

    /// A quoted string at the start of `s` read with or without backslash
    /// escapes: its value and where it ends.
    fn read_string(s: &str, backslash: bool) -> Option<(String, usize)> {
        let b: Vec<char> = s.chars().collect();
        if b.first() != Some(&'\'') {
            return None;
        }
        let mut out = String::new();
        let mut i = 1;
        while i < b.len() {
            match b[i] {
                '\\' if backslash && i + 1 < b.len() => {
                    out.push(match b[i + 1] {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'u' => {
                            let hex: String = b[i + 2..i + 6].iter().collect();
                            i += 4;
                            char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
                        }
                        c => c,
                    });
                    i += 2;
                }
                '\'' if b.get(i + 1) == Some(&'\'') => {
                    out.push('\'');
                    i += 2;
                }
                '\'' => return Some((out, b[..=i].iter().collect::<String>().len())),
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        None
    }

    #[test]
    fn other_types_per_dialect() {
        let d = ParamValue::date("2026-10-18").unwrap();
        let dt = ParamValue::datetime("2026-10-18 14:30").unwrap();
        assert_eq!(sql("postgres", &d), "DATE '2026-10-18'");
        assert_eq!(sql("oracle", &dt), "TIMESTAMP '2026-10-18 14:30:00'");
        assert_eq!(sql("mssql", &d), "CAST('2026-10-18' AS date)");
        assert_eq!(
            sql("mssql", &dt),
            "CAST('2026-10-18T14:30:00' AS datetime2)"
        );
        assert_eq!(sql("sqlite", &d), "'2026-10-18'");
        assert_eq!(sql("mssql", &ParamValue::Bool(true)), "1");
        assert_eq!(sql("postgres", &ParamValue::Bool(false)), "FALSE");
        assert_eq!(sql("mysql", &ParamValue::Null), "NULL");
        assert_eq!(
            literal(Language::Json, "", "mongodb", &d).unwrap(),
            "ISODate(\"2026-10-18T00:00:00Z\")"
        );
        assert_eq!(
            literal(
                Language::Json,
                "",
                "mongodb",
                &text("a\"}); db.x.drop(); //")
            )
            .unwrap(),
            "\"a\\\"}); db.x.drop(); //\""
        );
        assert_eq!(
            literal(Language::Json, "", "elasticsearch", &d).unwrap(),
            "\"2026-10-18\""
        );
        assert_eq!(
            literal(Language::Redis, "", "redis", &text("a\" b\nFLUSHALL")).unwrap(),
            "\"a\\\" b\\nFLUSHALL\""
        );
        assert_eq!(
            literal(Language::Redis, "", "redis", &text("\u{1}")).unwrap(),
            "\"\\x01\""
        );
        assert!(literal(Language::Redis, "", "redis", &ParamValue::Null).is_err());
        assert_eq!(
            literal(Language::Flux, "", "influxdb", &text("${x}\"")).unwrap(),
            "\"\\${x}\\\"\""
        );
        assert_eq!(
            literal(Language::Cypher, "neo4j", "neo4j", &text("a'b")).unwrap(),
            "'a\\'b'"
        );
        assert_eq!(
            literal(Language::Cypher, "neo4j", "neo4j", &d).unwrap(),
            "date('2026-10-18')"
        );
    }

    #[test]
    fn bound_statements_render_each_way() {
        let s = BoundStatement {
            parts: vec![
                BoundPart::Text("SELECT * FROM t WHERE a = ".into()),
                BoundPart::Param(0),
                BoundPart::Text(" OR b = ".into()),
                BoundPart::Param(1),
                BoundPart::Text(" OR c = ".into()),
                BoundPart::Param(0),
            ],
            values: vec![text("x"), ParamValue::Number("2".into())],
        };
        assert!(s.has_params());
        assert_eq!(
            s.numbered("@P"),
            "SELECT * FROM t WHERE a = @P1 OR b = @P2 OR c = @P1"
        );
        let (q, vals) = s.positional();
        assert_eq!(q, "SELECT * FROM t WHERE a = ? OR b = ? OR c = ?");
        assert_eq!(
            vals,
            vec![&text("x"), &ParamValue::Number("2".into()), &text("x")]
        );
        let lit = s
            .literal_text(&|v| literal(Language::Sql, "standard", "x", v))
            .unwrap();
        assert_eq!(lit, "SELECT * FROM t WHERE a = 'x' OR b = 2 OR c = 'x'");
        // A negative number after a minus is never a comment.
        let m = BoundStatement {
            parts: vec![BoundPart::Text("SELECT 1 -".into()), BoundPart::Param(0)],
            values: vec![ParamValue::Number("-5".into())],
        };
        assert_eq!(
            m.literal_text(&|v| literal(Language::Sql, "standard", "x", v))
                .unwrap(),
            "SELECT 1 - -5"
        );
    }
}

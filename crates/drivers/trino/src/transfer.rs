//! Bulk transfer (see `dbine_driver::transfer`) for Trino, Presto and
//! Starburst, over the same client REST protocol as the rest of the driver.
//!
//! Reading: one `SELECT <columns> FROM t [WHERE filter]`, following its
//! `nextUri` pages, with the `PARAMETRIC_DATETIME` client capability so
//! TIME / TIMESTAMP keep their full precision (up to picoseconds) instead of
//! milliseconds. Values are typed by the table's column types: DECIMAL
//! comes as exact text; REAL / DOUBLE are read as text (`CAST … AS
//! VARCHAR`, the shortest exact form, parsed here bit for bit); TIMESTAMP
//! WITH TIME ZONE through `to_iso8601` (a numeric offset even for region
//! zones); VARBINARY as base64, decoded whole; ARRAY / MAP / ROW as JSON
//! (`json_format(CAST(… AS JSON))`, so a ROW keeps its field names) when
//! every type inside can be cast to JSON ([`json_readable`]), otherwise the
//! protocol's raw value (UUID, TIME, VARBINARY, CHAR… inside).
//!
//! Loading: Trino has no bulk API, so it's multi-row `INSERT … VALUES` with
//! each literal rendered for the target column's type (`DECIMAL '…'`,
//! `TIMESTAMP '…'`, `X'…'`, `CAST(ARRAY[DATE '…'] AS array(date))`…).
//! Nothing is rounded or coerced silently: a decimal with more fraction
//! digits than the column takes is an error, and so is a TIME / TIMESTAMP
//! with more fraction-of-second digits than the column's precision
//! (trailing zeros aside), and so is a timestamp with a time of day
//! (midnight UTC, for an instant) into a DATE; nested values are built
//! element by element with those same literals (a `CAST(JSON … AS …)`
//! rounds and can't build dates, times, UUIDs or binaries). Statements go
//! up to [`STMT_BYTES`] (the server's `query.max-length` is 1,000,000
//! characters by default), [`STMT_ROWS`] rows or [`STMT_VALUES`] values
//! (what the coordinator's memory to plan them depends on), and never past
//! the spec's commit window. Every INSERT is its own transaction (most connectors only
//! write in autocommit), so each statement is a committed window and
//! `progress` is called after each one; inside an explicit transaction
//! nothing would be committed, so the load refuses to run there. Connectors
//! that take concurrent INSERTs into a table ([`CONCURRENT`]) get
//! [`IN_FLIGHT`] statements at once; the rest (Iceberg and Delta Lake
//! commits conflict, unknown connectors, Presto whose catalog list doesn't
//! say the connector) one at a time. When the load fails or is cancelled,
//! every statement still running is deleted on the server and waited for
//! until it has ended, so none commits after `bulk_load` returns; one whose
//! future is dropped (the engine's cancel) is deleted on the way out.

use crate::{http_error, lit, query_error, Conn, InFlight, QueryResults, TrinoSession};
use std::sync::Arc;
use base64::Engine as _;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result, Session};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::sync::atomic::Ordering;
use tokio::task::JoinSet;

/// Bytes of one INSERT at most (below the default `query.max-length`)…
pub(crate) const STMT_BYTES: usize = 900_000;
/// …or this many rows…
pub(crate) const STMT_ROWS: usize = 1_000;
/// …or this many values (rows × columns): the coordinator's memory to
/// analyze and plan a VALUES list grows with its expressions. Measured on
/// Trino 483 with a 2.5 GiB container (2 GiB heap), 100,000 rows of 6
/// columns: 2,000-row statements two at a time got the server OOM-killed
/// (it also happened one at a time); 1,000 rows (6,000 values) passed,
/// at 3,000 to 9,000 rows/s (cold to warm server), 500 at about 6,700.
pub(crate) const STMT_VALUES: usize = 6_000;
/// INSERTs in flight at once on connectors that take them concurrently.
pub(crate) const IN_FLIGHT: usize = 2;
/// Connectors whose INSERTs into one table don't conflict: in-memory,
/// Hive (new files per write) and the JDBC ones (each write lands in its
/// own staging table and is then appended).
pub(crate) const CONCURRENT: &[&str] =
    &["memory", "blackhole", "hive", "postgresql", "mysql", "mariadb", "sqlserver", "oracle", "singlestore", "redshift", "clickhouse"];

/// The families of Trino types the transfer tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int,
    Real,
    Double,
    Decimal,
    Char,
    Binary,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    /// ARRAY / MAP / ROW.
    Nested,
    Other,
}

/// A type as `information_schema.columns.data_type` (or the protocol's
/// column type) spells it.
pub(crate) fn kind(ty: &str) -> Kind {
    let t = ty.trim().to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    let tz = t.ends_with("with time zone");
    match base {
        "boolean" => Kind::Bool,
        "tinyint" | "smallint" | "integer" | "int" | "bigint" => Kind::Int,
        "real" => Kind::Real,
        "double" => Kind::Double,
        "decimal" => Kind::Decimal,
        "varchar" | "char" => Kind::Char,
        "varbinary" => Kind::Binary,
        "date" => Kind::Date,
        "time" | "time with time zone" if tz => Kind::TimeTz,
        "time" => Kind::Time,
        "timestamp" | "timestamp with time zone" if tz => Kind::TimestampTz,
        "timestamp" => Kind::Timestamp,
        "uuid" => Kind::Uuid,
        "json" => Kind::Json,
        "array" | "map" | "row" => Kind::Nested,
        _ => Kind::Other,
    }
}

/// The SELECT expression that reads column `name` of type `ty` without loss.
pub(crate) fn read_expr(name: &str, ty: &str) -> String {
    let c = quote_ident(Quote::Double, name);
    match kind(ty) {
        Kind::Real | Kind::Double => format!("CAST({c} AS VARCHAR)"),
        Kind::TimestampTz => format!("to_iso8601({c})"),
        // Types JSON can't take (UUID, TIME, VARBINARY…) make the cast
        // fail: those go raw.
        Kind::Nested if json_readable(&parse_ty(ty)) => format!("json_format(CAST({c} AS JSON))"),
        _ => c,
    }
}

/// A type, split into its nested parts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    Array(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    /// Fields with their names (`None`: anonymous).
    Row(Vec<(Option<String>, Ty)>),
    Leaf(String),
}

/// `s` split at its top-level commas (outside parentheses and quotes).
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut quoted, mut start) = (Vec::new(), 0i32, false, 0);
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// A type as `information_schema` / `typeof` spell it: `array(T)`,
/// `map(K, V)`, `row("a" T, b U)` / `row(T, U)`, or a leaf.
pub(crate) fn parse_ty(ty: &str) -> Ty {
    let t = ty.trim();
    let lower = t.to_ascii_lowercase();
    let inner = |p: &str| (lower.starts_with(p) && t.ends_with(')')).then(|| &t[p.len()..t.len() - 1]);
    if let Some(e) = inner("array(") {
        return Ty::Array(Box::new(parse_ty(e)));
    }
    if let Some(kv) = inner("map(") {
        if let [k, v] = split_top(kv)[..] {
            return Ty::Map(Box::new(parse_ty(k)), Box::new(parse_ty(v)));
        }
    }
    if let Some(fs) = inner("row(") {
        return Ty::Row(split_top(fs).into_iter().map(row_field).collect());
    }
    Ty::Leaf(t.to_string())
}

/// `"name" type`, `name type` or just `type`.
fn row_field(f: &str) -> (Option<String>, Ty) {
    if let Some(rest) = f.strip_prefix('"') {
        let (mut name, mut chars) = (String::new(), rest.char_indices().peekable());
        while let Some((i, ch)) = chars.next() {
            if ch == '"' {
                if chars.peek().map(|&(_, c)| c) == Some('"') {
                    chars.next();
                    name.push('"');
                    continue;
                }
                return (Some(name), parse_ty(&rest[i + 1..]));
            }
            name.push(ch);
        }
    }
    // A bare word before the type, unless the whole thing is one type
    // (`timestamp(3) with time zone`, `array(integer)`, `ipaddress`).
    match f.split_once(char::is_whitespace) {
        Some((name, ty)) if kind(f) == Kind::Other && !name.contains('(') => (Some(name.to_string()), parse_ty(ty)),
        _ => (None, parse_ty(f)),
    }
}

/// Whether `CAST(x AS JSON)` takes a value of this type (checked on
/// Trino 483: DATE and zoneless TIMESTAMP do; UUID, TIME, zoned values,
/// CHAR, VARBINARY, IPADDRESS don't, and neither do map keys other than
/// numbers, booleans and VARCHAR).
pub(crate) fn json_readable(ty: &Ty) -> bool {
    let scalar = |t: &str| matches!(kind(t), Kind::Bool | Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) || t.trim().to_ascii_lowercase().starts_with("varchar");
    match ty {
        Ty::Leaf(t) => scalar(t) || matches!(kind(t), Kind::Json | Kind::Date | Kind::Timestamp),
        Ty::Array(e) => json_readable(e),
        Ty::Map(k, v) => matches!(&**k, Ty::Leaf(t) if scalar(t)) && json_readable(v),
        Ty::Row(fs) => fs.iter().all(|(_, t)| json_readable(t)),
    }
}

fn float(s: &str) -> f64 {
    match s.trim() {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        other => other.parse().unwrap_or(f64::NAN),
    }
}

/// `2024-01-02T03:04:05.123-05:00` / `…Z` as `2024-01-02 03:04:05.123-05:00`
/// (and `+12024-…`, a year past 9999 as `to_iso8601` writes it, as `12024-…`).
pub(crate) fn iso_to_tz(s: &str) -> String {
    let s = t_to_space(s.trim());
    let s = s.strip_prefix('+').map(str::to_string).unwrap_or(s);
    match s.strip_suffix('Z') {
        Some(l) => format!("{l}+00:00"),
        None => s,
    }
}

/// The ISO `T` between date and time as a space (only there: a zone name
/// like `America/Tijuana` keeps its letters).
fn t_to_space(s: &str) -> String {
    let b = s.as_bytes();
    match s.find('T') {
        Some(p) if p > 0 && b[p - 1].is_ascii_digit() && b.get(p + 1).is_some_and(u8::is_ascii_digit) => format!("{} {}", &s[..p], &s[p + 1..]),
        _ => s.to_string(),
    }
}

/// A date or timestamp without the `+` the protocol puts before a year
/// past 9999 (the cells' form is `YYYY-…`).
fn plain_year(s: String) -> String {
    match s.strip_prefix('+') {
        Some(r) => r.to_string(),
        None => s,
    }
}

/// Whether a time or timestamp text carries a zone (`±HH:MM`, `Z`, a
/// zone name) after its time.
fn has_zone(s: &str) -> bool {
    let t = s.trim();
    let Some(c) = t.find(':') else { return false };
    let rest = &t[c..];
    rest.contains(['+', '-']) || rest.bytes().any(|b| b.is_ascii_alphabetic())
}

/// A value of the answer (read through [`read_expr`]) as a cell, given its
/// column's type.
pub(crate) fn to_cell(v: Value, ty: &str) -> Cell {
    let k = kind(ty);
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(b),
        Value::Number(n) => match k {
            Kind::Real | Kind::Double => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
            Kind::Decimal => Cell::Decimal(n.to_string()),
            _ => match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => Cell::Int(i),
                (None, Some(u)) => Cell::UInt(u),
                _ => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
        },
        Value::String(s) => match k {
            Kind::Real | Kind::Double => Cell::Float(float(&s)),
            Kind::Decimal => Cell::Decimal(s),
            Kind::Binary => match base64::engine::general_purpose::STANDARD.decode(&s) {
                Ok(b) => Cell::Bytes(b),
                Err(_) => Cell::Text(s),
            },
            Kind::Date => Cell::Date(plain_year(s)),
            Kind::Time => Cell::Time(s),
            Kind::Timestamp => Cell::DateTime(plain_year(s)),
            Kind::TimestampTz => Cell::DateTimeTz(iso_to_tz(&s)),
            Kind::Uuid => Cell::Uuid(s),
            Kind::Json | Kind::Nested => Cell::Json(s),
            _ => Cell::Text(s),
        },
        other => Cell::Json(other.to_string()),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// A binary literal: `X'…'`, or `from_base64('…')` when long (a third
/// shorter, so larger values fit in a statement).
fn bin(b: &[u8]) -> String {
    if b.len() > 1024 {
        format!("from_base64('{}')", base64::engine::general_purpose::STANDARD.encode(b))
    } else {
        format!("X'{}'", hex(b))
    }
}

/// A number's text (`[±]digits[.digits][e±n]`) as plain digits with the
/// point where it goes (no exponent); `None` when it isn't one.
pub(crate) fn plain_decimal(s: &str) -> Option<String> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(p) => (&body[..p], body[p + 1..].parse::<i64>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digit = |x: &str| x.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digit(int) || !digit(frac) || exp.abs() > 1000 {
        return None;
    }
    let (int, frac) = if exp == 0 {
        (int.to_string(), frac.to_string())
    } else {
        let digits = format!("{int}{frac}");
        let point = int.len() as i64 + exp;
        if point <= 0 {
            (String::new(), format!("{}{digits}", "0".repeat(point.unsigned_abs() as usize)))
        } else if point as usize >= digits.len() {
            (format!("{digits}{}", "0".repeat(point as usize - digits.len())), String::new())
        } else {
            (digits[..point as usize].to_string(), digits[point as usize..].to_string())
        }
    };
    let int = int.trim_start_matches('0');
    let mut out = String::from(if neg { "-" } else { "" });
    out.push_str(if int.is_empty() { "0" } else { int });
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

/// `decimal(p,s)`'s precision and scale.
fn decimal_params(ty: &str) -> Option<(usize, usize)> {
    let params = ty.split_once('(').and_then(|(_, r)| r.trim().strip_suffix(')'))?;
    let (p, s) = params.split_once(',')?;
    Some((p.trim().parse().ok()?, s.trim().parse().ok()?))
}

/// A decimal literal for `decimal(p,s)`: trailing fraction zeros past the
/// scale are dropped, other digits past it are an error (never rounded);
/// a literal whose integer digits don't fit gets a cast, which fails on the
/// server instead of truncating.
fn decimal_literal(text: &str, ty: &str) -> Result<String> {
    let Some(mut plain) = plain_decimal(text) else {
        return Ok(format!("CAST({} AS {ty})", lit(text.trim())));
    };
    let Some((p, s)) = decimal_params(ty) else {
        return Ok(format!("CAST(DECIMAL '{plain}' AS {ty})"));
    };
    if let Some(dot) = plain.find('.') {
        while plain.len() - dot - 1 > s && plain.ends_with('0') {
            plain.pop();
        }
        if plain.ends_with('.') {
            plain.pop();
        }
        if plain.find('.').is_some_and(|d| plain.len() - d - 1 > s) {
            return Err(Error::Query(format!(
                "El valor {} tiene más decimales de los que admite la columna ({ty}): no se redondea.",
                text.trim()
            )));
        }
    }
    let int = plain.trim_start_matches('-').split('.').next().unwrap_or("").trim_start_matches('0');
    Ok(if int.len() <= p.saturating_sub(s) {
        format!("DECIMAL '{plain}'")
    } else {
        // Too many integer digits: the cast makes the server say so.
        format!("CAST(DECIMAL '{plain}' AS {ty})")
    })
}

fn plain_integer(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A cell as text (for targets that take a string).
fn as_text(c: &Cell) -> Option<String> {
    Some(match c {
        Cell::Null => return None,
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_nan() => "NaN".into(),
        Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
        Cell::DateTime(s) | Cell::DateTimeTz(s) => t_to_space(s),
    })
}

fn cast(c: &Cell, ty: &str) -> String {
    match c {
        Cell::Bytes(b) => format!("CAST({} AS {ty})", bin(b)),
        _ => format!("CAST({} AS {ty})", lit(&as_text(c).unwrap_or_default())),
    }
}

/// `TIMESTAMP '…'` of a timestamp text (ISO `T` and `Z` accepted).
fn ts_lit(s: &str) -> String {
    format!("TIMESTAMP {}", lit(&iso_to_tz(s)))
}

/// An instant as its UTC wall time, cast to the zoneless `ty` (a
/// timestamp, time or date).
fn utc_as(s: &str, ty: &str) -> String {
    format!("CAST({} AT TIME ZONE 'UTC' AS {ty})", ts_lit(s))
}

/// A cell as a literal of the target column's type `ty`. Values that don't
/// fit without loss are an error, never rounded.
pub(crate) fn literal(c: &Cell, ty: &str) -> Result<String> {
    if matches!(c, Cell::Null) {
        return Ok("NULL".into());
    }
    let fitted;
    let c = match c {
        Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s)
            if matches!(kind(ty), Kind::Time | Kind::TimeTz | Kind::Timestamp | Kind::TimestampTz) =>
        {
            let s = fit_fraction(s, ty)?;
            fitted = match c {
                Cell::Text(_) => Cell::Text(s),
                Cell::Date(_) => Cell::Date(s),
                Cell::Time(_) => Cell::Time(s),
                Cell::DateTime(_) => Cell::DateTime(s),
                _ => Cell::DateTimeTz(s),
            };
            &fitted
        }
        _ => c,
    };
    Ok(match kind(ty) {
        Kind::Bool => match c {
            Cell::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Cell::Int(i) => if *i != 0 { "TRUE" } else { "FALSE" }.into(),
            Cell::UInt(u) => if *u != 0 { "TRUE" } else { "FALSE" }.into(),
            Cell::Text(s) | Cell::Decimal(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "y" => "TRUE".into(),
                "false" | "f" | "0" | "no" | "n" => "FALSE".into(),
                _ => cast(c, "boolean"),
            },
            _ => cast(c, "boolean"),
        },
        Kind::Int => {
            let digits = match c {
                Cell::Int(i) => i.to_string(),
                Cell::UInt(u) => u.to_string(),
                Cell::Bool(b) => u8::from(*b).to_string(),
                Cell::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 => (*f as i64).to_string(),
                Cell::Text(s) | Cell::Decimal(s) if plain_integer(s.trim()) => s.trim().trim_start_matches('+').to_string(),
                _ => return Ok(cast(c, ty)),
            };
            // Presto doesn't narrow an integer literal (and `-2147483648`
            // is a BIGINT, the negation of one), so narrow columns get
            // typed literals; the smallest BIGINT has no plain literal.
            match ty.trim().to_ascii_lowercase().as_str() {
                "tinyint" => format!("TINYINT '{digits}'"),
                "smallint" => format!("SMALLINT '{digits}'"),
                "integer" | "int" => format!("INTEGER '{digits}'"),
                _ if digits == i64::MIN.to_string() => format!("BIGINT '{digits}'"),
                _ => digits,
            }
        }
        Kind::Double => match c {
            Cell::Float(f) if f.is_nan() => "nan()".into(),
            Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "infinity()" } else { "-infinity()" }.into(),
            // Rust's `{:e}` is the shortest form that reads back bit for bit.
            Cell::Float(f) => format!("{f:e}"),
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => u.to_string(),
            Cell::Bool(b) => u8::from(*b).to_string(),
            _ => format!("DOUBLE {}", lit(as_text(c).unwrap_or_default().trim())),
        },
        // Parsed straight into a float (Presto doesn't narrow a double).
        Kind::Real => match c {
            Cell::Float(f) if f.is_nan() => "CAST(nan() AS real)".into(),
            Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "CAST(infinity() AS real)" } else { "CAST(-infinity() AS real)" }.into(),
            Cell::Float(f) => format!("REAL '{f:e}'"),
            Cell::Bool(b) => format!("REAL '{}'", u8::from(*b)),
            _ => format!("REAL {}", lit(as_text(c).unwrap_or_default().trim())),
        },
        Kind::Decimal => match c {
            Cell::Int(i) => decimal_literal(&i.to_string(), ty)?,
            Cell::UInt(u) => decimal_literal(&u.to_string(), ty)?,
            Cell::Bool(b) => decimal_literal(if *b { "1" } else { "0" }, ty)?,
            Cell::Float(f) if f.is_finite() => decimal_literal(&f.to_string(), ty)?,
            Cell::Decimal(s) | Cell::Text(s) => decimal_literal(s, ty)?,
            _ => cast(c, ty),
        },
        Kind::Char => match c {
            Cell::Bytes(b) if std::str::from_utf8(b).is_err() => format!("from_utf8({})", bin(b)),
            _ => lit(&as_text(c).unwrap_or_default()),
        },
        Kind::Binary => match c {
            Cell::Bytes(b) => bin(b),
            _ => bin(as_text(c).unwrap_or_default().as_bytes()),
        },
        // An instant into a zoneless column is taken at its UTC wall time,
        // whether it comes as a zoned cell or as a text with a zone.
        // A timestamp goes into a DATE only when nothing is lost: its time
        // of day (in UTC, for an instant) must be midnight.
        Kind::Date => match c {
            Cell::DateTimeTz(s) => {
                whole_day(s, ty)?;
                utc_as(s, ty)
            }
            Cell::Date(s) | Cell::Text(s) | Cell::DateTime(s) if has_zone(s) => {
                whole_day(s, ty)?;
                utc_as(s, ty)
            }
            Cell::Date(s) | Cell::Text(s) if !s.contains(':') => format!("DATE {}", lit(s.trim())),
            Cell::Date(s) | Cell::Text(s) | Cell::DateTime(s) if s.contains(':') => {
                whole_day(s, ty)?;
                format!("CAST({} AS date)", ts_lit(s))
            }
            _ => cast(c, ty),
        },
        Kind::Time => match c {
            Cell::DateTimeTz(s) => utc_as(s, ty),
            Cell::DateTime(s) | Cell::Text(s) if looks_like_date(s) => {
                if has_zone(s) {
                    utc_as(s, ty)
                } else {
                    format!("CAST({} AS {ty})", ts_lit(s))
                }
            }
            Cell::Time(s) | Cell::Text(s) if has_zone(s) => format!("CAST(TIME {} AT TIME ZONE 'UTC' AS {ty})", lit(s.trim())),
            Cell::Time(s) | Cell::Text(s) => format!("TIME {}", lit(s.trim())),
            _ => cast(c, ty),
        },
        // A zoneless value into a zoned column is taken as UTC (never the
        // session's zone).
        Kind::TimeTz => match c {
            Cell::DateTimeTz(s) => format!("CAST({} AS {ty})", ts_lit(s)),
            Cell::DateTime(s) | Cell::Text(s) if looks_like_date(s) => {
                let s = if has_zone(s) { iso_to_tz(s) } else { format!("{} UTC", iso_to_tz(s)) };
                format!("CAST({} AS {ty})", ts_lit(&s))
            }
            Cell::Time(s) | Cell::Text(s) if has_zone(s) => format!("TIME {}", lit(s.trim())),
            Cell::Time(s) | Cell::Text(s) => format!("TIME {}", lit(&format!("{}+00:00", s.trim()))),
            _ => cast(c, ty),
        },
        Kind::Timestamp => match c {
            Cell::DateTimeTz(s) => utc_as(s, ty),
            Cell::DateTime(s) | Cell::Date(s) | Cell::Text(s) if has_zone(s) => utc_as(s, ty),
            Cell::DateTime(s) | Cell::Date(s) | Cell::Text(s) => ts_lit(s),
            _ => cast(c, ty),
        },
        Kind::TimestampTz => match c {
            Cell::DateTimeTz(s) | Cell::DateTime(s) | Cell::Text(s) if has_zone(s) => ts_lit(s),
            Cell::DateTime(s) | Cell::Text(s) => ts_lit(&format!("{} UTC", iso_to_tz(s))),
            Cell::Date(s) => ts_lit(&format!("{} 00:00:00 UTC", s.trim())),
            _ => cast(c, ty),
        },
        Kind::Uuid => match c {
            Cell::Bytes(b) if b.len() == 16 => {
                let h = hex(b).to_ascii_lowercase();
                format!("UUID '{}-{}-{}-{}-{}'", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
            }
            _ => format!("UUID {}", lit(as_text(c).unwrap_or_default().trim())),
        },
        Kind::Json => format!("JSON {}", lit(&json_doc(c))),
        Kind::Nested => nested_literal(c, ty)?,
        Kind::Other => cast(c, ty),
    })
}

/// The fraction digits a TIME / TIMESTAMP column keeps: `time(6)` → 6;
/// without a precision, 3 (Trino's default, and Presto's only one).
fn time_precision(ty: &str) -> usize {
    ty.split_once('(')
        .and_then(|(_, r)| r.split_once(')'))
        .and_then(|(p, _)| p.trim().parse().ok())
        .unwrap_or(3)
}

/// Where the seconds' fraction digits of a time or timestamp text
/// (`…HH:MM:SS.fff`) are: the `.`'s index and the digits' end.
fn fraction_span(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let clock = |i: usize| {
        i >= 8 && b[i - 3] == b':' && b[i - 6] == b':' && [1, 2, 4, 5, 7, 8].iter().all(|k| b[i - k].is_ascii_digit())
    };
    let dot = (0..b.len()).find(|&i| b[i] == b'.' && clock(i))?;
    Some((dot, dot + 1 + b[dot + 1..].iter().take_while(|c| c.is_ascii_digit()).count()))
}

/// A time or timestamp text fitted to the column's precision: trailing
/// fraction zeros past it are dropped (Presto rejects a literal with more
/// digits than milliseconds), any other digit past it is an error, since
/// the server would round it (`23:59:59.9999` into `time(3)` would even
/// wrap to midnight).
fn fit_fraction(s: &str, ty: &str) -> Result<String> {
    let Some((dot, end)) = fraction_span(s) else { return Ok(s.to_string()) };
    let p = time_precision(ty);
    let digits = &s[dot + 1..end];
    let keep = digits.trim_end_matches('0').len().max(p.min(digits.len()));
    if keep > p {
        return Err(Error::Query(format!(
            "El valor {} tiene más decimales de segundo de los que admite la columna ({ty}): no se redondea.",
            s.trim()
        )));
    }
    let cut = if keep == 0 { dot } else { dot + 1 + keep };
    Ok(format!("{}{}", &s[..cut], &s[end..]))
}

/// Checks that a timestamp text (zoned or not) has no time of day to lose
/// in a DATE column: midnight, or for an instant midnight in UTC (the day
/// it's stored as). A zone given by name can't be resolved here, so a
/// value carrying one is refused rather than guessed.
fn whole_day(s: &str, ty: &str) -> Result<()> {
    let norm = iso_to_tz(s);
    let lost = || {
        Error::Query(format!(
            "El valor {} tiene hora del día y la columna ({ty}) guarda solo la fecha: se perdería la hora.",
            s.trim()
        ))
    };
    // After the date: `HH:MM[:SS[.fff]]` and maybe a zone.
    let Some((_, rest)) = norm.trim().split_once(' ') else { return Err(lost()) };
    let rest = rest.trim_start();
    let clock_end = rest.bytes().position(|b| !(b.is_ascii_digit() || b == b':' || b == b'.')).unwrap_or(rest.len());
    let (clock, zone) = (&rest[..clock_end], rest[clock_end..].trim());
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().unwrap_or(-1)).collect();
    if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|p| *p < 0) {
        return Err(lost());
    }
    if frac.bytes().any(|b| b != b'0') {
        return Err(lost());
    }
    let local = parts[0] * 3600 + parts[1] * 60 + parts.get(2).copied().unwrap_or(0);
    let offset = if zone.is_empty() || zone.eq_ignore_ascii_case("UTC") || zone == "Z" {
        0
    } else {
        let (sign, digits) = match zone.as_bytes()[0] {
            b'+' => (1, &zone[1..]),
            b'-' => (-1, &zone[1..]),
            _ => {
                return Err(Error::Query(format!(
                    "El valor {} trae la zona horaria por nombre y la columna ({ty}) guarda solo la fecha: no se puede comprobar que no se pierda la hora. Usá un desplazamiento numérico (±HH:MM).",
                    s.trim()
                )))
            }
        };
        let digits = digits.replace(':', "");
        let (h, m) = digits.split_at(digits.len().min(2));
        match (h.parse::<i64>(), if m.is_empty() { Ok(0) } else { m.parse::<i64>() }) {
            (Ok(h), Ok(m)) if digits.bytes().all(|b| b.is_ascii_digit()) => sign * (h * 3600 + m * 60),
            _ => return Err(lost()),
        }
    };
    if (local - offset).rem_euclid(86_400) != 0 {
        return Err(lost());
    }
    Ok(())
}

/// `YYYY-MM-DD…` (a timestamp), as opposed to a bare time.
fn looks_like_date(s: &str) -> bool {
    let t = s.trim().trim_start_matches(['+', '-']);
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    digits >= 4 && t.as_bytes().get(digits) == Some(&b'-')
}

/// A cell as a JSON document for a JSON column: JSON cells as they are,
/// texts that hold an object or an array as that document, and every other
/// value (a text like `123` or `true` too) as its JSON value, so a string
/// stays a string.
fn json_doc(c: &Cell) -> String {
    match c {
        Cell::Json(s) => s.clone(),
        Cell::Text(s) if s.trim_start().starts_with(['{', '[']) && parse_json(s).is_some() => s.clone(),
        other => other.to_json().to_string(),
    }
}

/// A JSON value that keeps its numbers' text (exact decimals, big
/// integers), unlike `serde_json::Value` without `arbitrary_precision`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

pub(crate) fn parse_json(s: &str) -> Option<J> {
    let (b, mut i) = (s.as_bytes(), 0);
    let v = json_value(s, b, &mut i, 0)?;
    skip_ws(b, &mut i);
    (i == b.len()).then_some(v)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}

fn json_value(s: &str, b: &[u8], i: &mut usize, depth: usize) -> Option<J> {
    skip_ws(b, i);
    if depth > 256 {
        return None;
    }
    let word = |i: &mut usize, w: &str, v: J| {
        s[*i..].starts_with(w).then(|| {
            *i += w.len();
            v
        })
    };
    match *b.get(*i)? {
        b'n' => word(i, "null", J::Null),
        b't' => word(i, "true", J::Bool(true)),
        b'f' => word(i, "false", J::Bool(false)),
        b'"' => json_string(s, b, i).map(J::Str),
        b'[' => {
            *i += 1;
            let mut v = Vec::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Some(J::Arr(v));
            }
            loop {
                v.push(json_value(s, b, i, depth + 1)?);
                skip_ws(b, i);
                match *b.get(*i)? {
                    b',' => *i += 1,
                    b']' => {
                        *i += 1;
                        return Some(J::Arr(v));
                    }
                    _ => return None,
                }
            }
        }
        b'{' => {
            *i += 1;
            let mut v = Vec::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Some(J::Obj(v));
            }
            loop {
                skip_ws(b, i);
                if b.get(*i) != Some(&b'"') {
                    return None;
                }
                let k = json_string(s, b, i)?;
                skip_ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return None;
                }
                *i += 1;
                v.push((k, json_value(s, b, i, depth + 1)?));
                skip_ws(b, i);
                match *b.get(*i)? {
                    b',' => *i += 1,
                    b'}' => {
                        *i += 1;
                        return Some(J::Obj(v));
                    }
                    _ => return None,
                }
            }
        }
        _ => {
            let start = *i;
            while b.get(*i).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                *i += 1;
            }
            let t = &s[start..*i];
            (!t.is_empty() && t.parse::<f64>().is_ok()).then(|| J::Num(t.to_string()))
        }
    }
}

fn json_string(s: &str, b: &[u8], i: &mut usize) -> Option<String> {
    let start = *i;
    *i += 1;
    while let Some(&c) = b.get(*i) {
        match c {
            b'\\' => *i += 2,
            b'"' => {
                *i += 1;
                return serde_json::from_str(&s[start..*i]).ok();
            }
            _ => *i += 1,
        }
    }
    None
}

/// A JSON value back as text.
fn json_out(j: &J) -> String {
    match j {
        J::Null => "null".into(),
        J::Bool(b) => b.to_string(),
        J::Num(n) => n.clone(),
        J::Str(s) => Value::String(s.clone()).to_string(),
        J::Arr(v) => format!("[{}]", v.iter().map(json_out).collect::<Vec<_>>().join(",")),
        J::Obj(v) => format!("{{{}}}", v.iter().map(|(k, x)| format!("{}:{}", Value::String(k.clone()), json_out(x))).collect::<Vec<_>>().join(",")),
    }
}

/// An ARRAY / MAP / ROW value (a JSON document: arrays, objects, a ROW as
/// an object by field name or an array by position) built element by
/// element with each element type's literal, then cast to the column type.
fn nested_literal(c: &Cell, ty: &str) -> Result<String> {
    let text = match c {
        Cell::Json(s) | Cell::Text(s) => s,
        _ => return Err(Error::Query(format!("Un valor de tipo {} no se puede cargar en una columna {ty}.", cell_kind(c)))),
    };
    let j = parse_json(text).ok_or_else(|| Error::Query(format!("El valor no es un documento JSON válido para la columna {ty}.")))?;
    Ok(format!("CAST({} AS {ty})", nested_value(&j, &parse_ty(ty), ty)?))
}

fn cell_kind(c: &Cell) -> &'static str {
    match c {
        Cell::Bool(_) => "booleano",
        Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) | Cell::Decimal(_) => "numérico",
        Cell::Bytes(_) => "binario",
        Cell::Date(_) | Cell::Time(_) | Cell::DateTime(_) | Cell::DateTimeTz(_) => "fecha u hora",
        Cell::Uuid(_) => "UUID",
        _ => "texto",
    }
}

fn nested_value(j: &J, ty: &Ty, column: &str) -> Result<String> {
    let wrong = || Error::Query(format!("El valor {} no tiene la forma de la columna {column}.", json_out(j)));
    let list = |xs: Vec<String>| xs.join(", ");
    Ok(match (ty, j) {
        (_, J::Null) => "NULL".into(),
        (Ty::Array(e), J::Arr(v)) => format!("ARRAY[{}]", list(v.iter().map(|x| nested_value(x, e, column)).collect::<Result<_>>()?)),
        (Ty::Map(_, _), J::Obj(v)) if v.is_empty() => "MAP()".into(),
        (Ty::Map(k, t), J::Obj(v)) => {
            let keys = v.iter().map(|(key, _)| nested_value(&J::Str(key.clone()), k, column)).collect::<Result<Vec<_>>>()?;
            let vals = v.iter().map(|(_, x)| nested_value(x, t, column)).collect::<Result<Vec<_>>>()?;
            format!("MAP(ARRAY[{}], ARRAY[{}])", list(keys), list(vals))
        }
        (Ty::Row(fs), J::Arr(v)) if v.len() == fs.len() => {
            format!("ROW({})", list(v.iter().zip(fs).map(|(x, (_, t))| nested_value(x, t, column)).collect::<Result<_>>()?))
        }
        (Ty::Row(fs), J::Obj(v)) if fs.iter().all(|(n, _)| n.is_some()) => {
            let field = |k: &str| fs.iter().position(|(n, _)| n.as_deref() == Some(k)).or_else(|| fs.iter().position(|(n, _)| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(k))));
            let mut vals = vec!["NULL".to_string(); fs.len()];
            for (k, x) in v {
                let p = field(k).ok_or_else(|| Error::Query(format!("El campo {k} no existe en la columna {column}.")))?;
                vals[p] = nested_value(x, &fs[p].1, column)?;
            }
            format!("ROW({})", list(vals))
        }
        (Ty::Leaf(t), _) => literal(&leaf_cell(j, t).ok_or_else(wrong)?, t)?,
        _ => return Err(wrong()),
    })
}

/// A JSON element as the cell for its element type (numbers keep their
/// text: floats parse once, decimals stay exact).
fn leaf_cell(j: &J, ty: &str) -> Option<Cell> {
    let k = kind(ty);
    Some(match j {
        J::Null => Cell::Null,
        _ if k == Kind::Json => Cell::Json(json_out(j)),
        J::Bool(b) => Cell::Bool(*b),
        J::Num(n) if k == Kind::Decimal => Cell::Decimal(n.clone()),
        J::Num(n) => Cell::Text(n.clone()),
        J::Str(s) => match k {
            Kind::Binary => Cell::Bytes(match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) if h.len() % 2 == 0 => (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect::<Option<_>>()?,
                _ => base64::engine::general_purpose::STANDARD.decode(s).ok()?,
            }),
            Kind::Decimal => Cell::Decimal(s.clone()),
            Kind::Date => Cell::Date(s.clone()),
            Kind::Time => Cell::Time(s.clone()),
            Kind::Timestamp => Cell::DateTime(s.clone()),
            Kind::TimestampTz => Cell::DateTimeTz(s.clone()),
            Kind::Uuid => Cell::Uuid(s.clone()),
            _ => Cell::Text(s.clone()),
        },
        J::Arr(_) | J::Obj(_) => return None,
    })
}

/// `INSERT INTO t (cols) VALUES\n` (the rows follow, comma separated).
pub(crate) fn insert_head(schema: Option<&str>, table: &str, columns: &[String]) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(Quote::Double, c)).collect();
    format!("INSERT INTO {} ({}) VALUES\n", qualified_name(Quote::Double, schema.filter(|s| !s.is_empty()), table), cols.join(", "))
}

/// `(v1, v2, …)` with each value typed for its column. A lone value goes as
/// `ROW(v)`: VALUES spreads a row-typed expression over the columns, so
/// `(CAST(ROW(…) AS row(…)))` into a single ROW column would be taken as
/// its fields, one column each.
pub(crate) fn row_tuple(row: &[Cell], types: &[String]) -> Result<String> {
    let mut s = String::from(if row.len() == 1 { "ROW(" } else { "(" });
    for (i, c) in row.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&literal(c, types.get(i).map_or("", String::as_str))?);
    }
    s.push(')');
    Ok(s)
}

/// Groups rows into INSERT statements of at most [`STMT_BYTES`] /
/// [`STMT_ROWS`] / [`STMT_VALUES`], or a smaller commit window (a single
/// larger row goes alone).
pub(crate) struct Statements {
    head: String,
    sql: String,
    rows: u64,
    max_rows: usize,
    max_bytes: usize,
}

impl Statements {
    pub(crate) fn new(head: String) -> Self {
        Statements { sql: String::new(), head, rows: 0, max_rows: STMT_ROWS, max_bytes: STMT_BYTES }
    }

    /// Statements of `columns` values a row, no larger than the load's
    /// commit window, since each one commits on its own.
    pub(crate) fn window(mut self, columns: usize, commit_rows: u64, commit_bytes: u64) -> Self {
        let by_values = (STMT_VALUES / columns.max(1)).clamp(1, STMT_ROWS);
        self.max_rows = usize::try_from(commit_rows.max(1)).unwrap_or(usize::MAX).min(by_values);
        self.max_bytes = usize::try_from(commit_bytes.max(1)).unwrap_or(usize::MAX).min(STMT_BYTES);
        self
    }

    /// Add a row; a statement that's full comes back with its rows.
    pub(crate) fn push(&mut self, tuple: &str) -> Option<(String, u64)> {
        let full = self.rows > 0 && (self.sql.len() + tuple.len() + 2 > self.max_bytes || self.rows as usize >= self.max_rows);
        let out = if full { self.take() } else { None };
        if self.rows == 0 {
            self.sql.push_str(&self.head);
        } else {
            self.sql.push_str(",\n");
        }
        self.sql.push_str(tuple);
        self.rows += 1;
        out
    }

    pub(crate) fn take(&mut self) -> Option<(String, u64)> {
        if self.rows == 0 {
            return None;
        }
        let rows = std::mem::replace(&mut self.rows, 0);
        Some((std::mem::take(&mut self.sql), rows))
    }
}

/// A statement that may still be running on the server. Dropped while
/// armed (its future dropped: the engine's cancel, an aborted task), it
/// deletes the query so it can't go on and commit.
struct Running {
    conn: Arc<Conn>,
    /// The session's headers (the user: the server asks who deletes).
    headers: HeaderMap,
    uri: Option<String>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(uri) = self.uri.take() else { return };
        let (conn, headers) = (self.conn.clone(), std::mem::take(&mut self.headers));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = conn.auth(conn.http.delete(&uri).headers(headers)).send().await {
                    tracing::debug!("trino cancel failed: {e}");
                }
            });
        }
    }
}

/// The query id in a `nextUri` (`…/v1/statement/{queued|executing}/{id}/…`).
pub(crate) fn query_id(uri: &str) -> Option<&str> {
    let mut parts = uri.split('/');
    parts.by_ref().find(|p| *p == "queued" || *p == "executing")?;
    parts.next().filter(|id| !id.is_empty())
}

/// Part of the error of a statement whose end couldn't be confirmed.
const UNSETTLED: &str = "no se pudo confirmar que terminó en el servidor";

/// How long a deleted statement is waited for until the server says it
/// has ended.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Wait until the query behind `uri` has ended (FINISHED or FAILED) on the
/// server, so it can't commit after this returns.
async fn settle(conn: &Conn, headers: &HeaderMap, uri: &str) -> Result<()> {
    let id = query_id(uri).ok_or_else(|| Error::Query(format!("no se reconoce la consulta de {uri}")))?;
    let url = format!("{}/v1/query/{id}", conn.base);
    let until = std::time::Instant::now() + SETTLE;
    let mut wait = std::time::Duration::from_millis(50);
    loop {
        match conn.auth(conn.http.get(&url).headers(headers.clone())).send().await {
            // Forgotten by the coordinator: long ended.
            Ok(r) if matches!(r.status().as_u16(), 404 | 410) => return Ok(()),
            Ok(r) if r.status().is_success() => {
                let state = r.json::<Value>().await.ok().and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_string));
                if matches!(state.as_deref(), Some("FINISHED" | "FAILED")) {
                    return Ok(());
                }
            }
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => {
                return Err(Error::Query(format!("el servidor no deja consultar el estado de la consulta {id} (HTTP {})", r.status())));
            }
            _ => {}
        }
        if std::time::Instant::now() >= until {
            return Err(Error::Query(format!("la consulta {id} no terminó en {} s", SETTLE.as_secs())));
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(std::time::Duration::from_millis(500));
    }
}

/// Run one statement to the end, handing each page to `on_page`; returns
/// its update count. Stops when the session's interrupter fired, and
/// whenever it stops before the statement's end (cancel, a network or page
/// error) deletes the query; with `wait_end` (writes) it also waits until
/// the server says the query ended, so nothing commits after it returns.
async fn walk(
    conn: &Arc<Conn>,
    headers: HeaderMap,
    sql: String,
    in_flight: &InFlight,
    wait_end: bool,
    on_page: impl FnMut(QueryResults) -> Result<()>,
) -> Result<Option<u64>> {
    let mut running = Running { conn: conn.clone(), headers: headers.clone(), uri: None };
    let r = drive(conn, headers.clone(), sql, in_flight, &mut running, on_page).await;
    let Some(uri) = running.uri.take() else { return r };
    // Stopped halfway: the query may still be running.
    let _ = conn.auth(conn.http.delete(&uri).headers(headers.clone())).send().await;
    if wait_end {
        if let Err(e) = settle(conn, &headers, &uri).await {
            let why = match &r {
                Err(Error::Cancelled) => "se canceló".to_string(),
                Err(e) => e.to_string(),
                Ok(_) => String::new(),
            };
            return Err(Error::Query(format!(
                "Una inserción se cortó ({why}) y {UNSETTLED}: {e}. Puede que sus filas se confirmen igual; \
                 revisá la tabla antes de reintentar."
            )));
        }
    }
    r
}

async fn drive(
    conn: &Conn,
    headers: HeaderMap,
    sql: String,
    in_flight: &InFlight,
    running: &mut Running,
    mut on_page: impl FnMut(QueryResults) -> Result<()>,
) -> Result<Option<u64>> {
    let rb = conn.http.post(format!("{}/v1/statement", conn.base)).headers(headers).body(sql);
    let mut resp = conn.auth(rb).send().await.map_err(http_error)?;
    let mut count = None;
    loop {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::AuthFailed(if text.trim().is_empty() { format!("HTTP {status}") } else { text.trim().to_string() }));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(if in_flight.cancelled.load(Ordering::SeqCst) {
                Error::Cancelled
            } else {
                Error::Query(format!("HTTP {status}: {}", text.trim()))
            });
        }
        let mut page: QueryResults = resp.json().await.map_err(|e| Error::Query(e.to_string()))?;
        if let Some(e) = page.error.take() {
            // Failed: nothing left running.
            running.uri = None;
            return Err(query_error(e));
        }
        if page.update_count.is_some() {
            count = page.update_count;
        }
        let next = page.next_uri.take();
        running.uri.clone_from(&next);
        on_page(page)?;
        let Some(next) = next else { return Ok(count) };
        *in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = Some(next.clone());
        if in_flight.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        resp = next_page(conn, &next).await?;
    }
}

/// GET a `nextUri`. The protocol makes it idempotent (the token in the
/// URI), so a dropped connection (a pooled one the server closed, a busy
/// coordinator) or a 502/503/504 is retried a few times.
async fn next_page(conn: &Conn, uri: &str) -> Result<reqwest::Response> {
    let mut wait = std::time::Duration::from_millis(100);
    for attempt in 0.. {
        let last = attempt == 5;
        match conn.auth(conn.http.get(uri)).send().await {
            Ok(r) if !last && matches!(r.status().as_u16(), 502..=504) => {}
            Ok(r) => return Ok(r),
            Err(e) if !last && !e.is_timeout() => tracing::debug!("trino page retry: {e:?}"),
            Err(e) => return Err(http_error(e)),
        }
        tokio::time::sleep(wait).await;
        wait *= 2;
    }
    unreachable!()
}

/// Server errors of a too long statement, explained.
fn explain_load_error(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("exceeds the maximum length") => Error::Query(format!(
            "Una fila no entra en una sentencia INSERT: el servidor limita el largo del texto de una consulta \
             (query.max-length, 1.000.000 caracteres por defecto). Subí ese límite o achicá el valor. ({m})"
        )),
        e => e,
    }
}

impl TrinoSession {
    /// The session's headers, asking for full-precision date and time values.
    fn transfer_headers(&self) -> HeaderMap {
        let mut h = self.state.headers(self.flavor, &self.conn.user);
        if let Ok(n) = HeaderName::try_from(self.flavor.header("client-capabilities")) {
            h.insert(n, HeaderValue::from_static("PARAMETRIC_DATETIME"));
        }
        h
    }

    /// The table's columns with their types, `wanted` in its order (all of
    /// them when `None`).
    async fn typed_columns(&mut self, table: &dbine_driver::ObjectRef, wanted: Option<&[String]>) -> Result<Vec<TransferColumn>> {
        let all = self.columns(table).await?;
        if all.is_empty() {
            return Err(Error::Query(format!("No se encontró la tabla {} o no tiene columnas.", table.name)));
        }
        let Some(wanted) = wanted else {
            return Ok(all.into_iter().map(|c| TransferColumn { name: c.name, type_name: c.data_type, nullable: c.nullable }).collect());
        };
        wanted
            .iter()
            .map(|w| {
                all.iter()
                    .find(|c| c.name == *w)
                    .or_else(|| all.iter().find(|c| c.name.eq_ignore_ascii_case(w)))
                    .map(|c| TransferColumn { name: c.name.clone(), type_name: c.data_type.clone(), nullable: c.nullable })
                    .ok_or_else(|| Error::Query(format!("La columna {w} no existe en {}.", table.name)))
            })
            .collect()
    }

    fn schema_of(&self, table: &dbine_driver::ObjectRef) -> Option<String> {
        table.schema().map(str::to_string).or(self.state.schema.clone())
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let cols = self.typed_columns(&spec.table, spec.columns.as_deref()).await?;
        let schema = self.schema_of(&spec.table);
        let exprs: Vec<String> = cols.iter().map(|c| read_expr(&c.name, &c.type_name)).collect();
        let mut sql = format!("SELECT {} FROM {}", exprs.join(", "), qualified_name(Quote::Double, schema.as_deref(), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().filter(|f| !f.trim().is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let types: Vec<String> = cols.iter().map(|c| c.type_name.clone()).collect();
        let mut builder = BatchBuilder::new();
        self.in_flight.cancelled.store(false, Ordering::SeqCst);
        let r = walk(&self.conn, self.transfer_headers(), sql, &self.in_flight, false, |page| {
            let Some(data) = page.data else { return Ok(()) };
            let mut sink = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            for row in data {
                let cells = row.into_iter().enumerate().map(|(i, v)| to_cell(v, types.get(i).map_or("", String::as_str))).collect();
                builder.push(cells, &mut *sink)?;
            }
            Ok(())
        })
        .await;
        *self.in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = None;
        r?;
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    /// Whether the catalog's connector takes concurrent INSERTs into a table.
    async fn concurrent_inserts(&mut self) -> bool {
        if self.state.transaction.is_some() {
            return false;
        }
        let Some(cat) = self.state.catalog.clone() else { return false };
        let sql = format!("SELECT connector_name FROM system.metadata.catalogs WHERE catalog_name = {}", lit(&cat));
        match self.strings(&sql).await {
            Ok(rows) => rows.first().and_then(|r| r.first()).is_some_and(|c| CONCURRENT.contains(&c.as_str())),
            Err(_) => false,
        }
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        _columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        if self.state.transaction.is_some() {
            return Err(Error::Unsupported(
                "La carga masiva no puede correr dentro de una transacción abierta: en Trino cada INSERT se \
                 confirma por su cuenta y adentro de la transacción ninguno quedaría confirmado hasta el final. \
                 Confirmá o deshacé la transacción antes de cargar."
                    .into(),
            ));
        }
        let target = self.typed_columns(&spec.table, Some(&spec.columns)).await?;
        let types: Vec<String> = target.iter().map(|c| c.type_name.clone()).collect();
        let names: Vec<String> = target.iter().map(|c| c.name.clone()).collect();
        let schema = self.schema_of(&spec.table);
        let degree = if self.concurrent_inserts().await { IN_FLIGHT } else { 1 };
        let headers = self.transfer_headers();
        self.in_flight.cancelled.store(false, Ordering::SeqCst);

        let mut stmts = Statements::new(insert_head(schema.as_deref(), &spec.table.name, &names)).window(names.len(), spec.commit_rows, spec.commit_bytes);
        let mut tasks: JoinSet<Result<u64>> = JoinSet::new();
        let mut committed = 0u64;
        let spawn = |tasks: &mut JoinSet<Result<u64>>, (sql, rows): (String, u64)| {
            let (conn, in_flight, headers) = (self.conn.clone(), self.in_flight.clone(), headers.clone());
            tasks.spawn(async move {
                let n = walk(&conn, headers, sql, &in_flight, true, |_| Ok(())).await.map_err(explain_load_error)?;
                Ok(n.unwrap_or(rows))
            });
        };
        let r: Result<()> = async {
            loop {
                let batch = source.next().await;
                let end = batch.is_none();
                let mut ready = Vec::new();
                if let Some(b) = batch {
                    for row in &b.rows {
                        if row.len() != types.len() {
                            return Err(Error::Query(format!(
                                "La fila tiene {} valores y la carga espera {} columnas.",
                                row.len(),
                                types.len()
                            )));
                        }
                        ready.extend(stmts.push(&row_tuple(row, &types)?));
                    }
                } else {
                    ready.extend(stmts.take());
                }
                for s in ready {
                    while tasks.len() >= degree {
                        committed += joined(tasks.join_next().await)?;
                        progress(committed);
                    }
                    spawn(&mut tasks, s);
                }
                if end {
                    break;
                }
            }
            while let Some(j) = tasks.join_next().await {
                committed += joined(Some(j))?;
                progress(committed);
            }
            Ok(())
        }
        .await;
        if r.is_err() && !tasks.is_empty() {
            // Stop the other statements (each deletes its query and waits
            // for the server to end it) and wait for them: none may commit
            // after this returns.
            self.in_flight.cancelled.store(true, Ordering::SeqCst);
            let mut unsettled = None;
            while let Some(j) = tasks.join_next().await {
                match joined(Some(j)) {
                    Ok(n) => committed += n,
                    Err(Error::Cancelled) => {}
                    // A statement that may still commit matters more than
                    // the load's own error.
                    Err(e @ Error::Query(_)) if e.to_string().contains(UNSETTLED) => unsettled = Some(e),
                    Err(e) => tracing::warn!("trino bulk load: a statement stopped after the load's error: {e}"),
                }
            }
            if let Some(e) = unsettled {
                *self.in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Err(e);
            }
        }
        *self.in_flight.next_uri.lock().unwrap_or_else(|e| e.into_inner()) = None;
        r?;
        Ok(committed)
    }
}

fn joined(j: Option<std::result::Result<Result<u64>, tokio::task::JoinError>>) -> Result<u64> {
    match j {
        None => Ok(0),
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("una inserción terminó con un error inesperado: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lit_ok(c: &Cell, ty: &str) -> String {
        literal(c, ty).unwrap()
    }

    #[test]
    fn kinds_of_types() {
        assert_eq!(kind("timestamp(6) with time zone"), Kind::TimestampTz);
        assert_eq!(kind("timestamp(3)"), Kind::Timestamp);
        assert_eq!(kind("time(9) with time zone"), Kind::TimeTz);
        assert_eq!(kind("time"), Kind::Time);
        assert_eq!(kind("decimal(38,10)"), Kind::Decimal);
        assert_eq!(kind("varchar(10)"), Kind::Char);
        assert_eq!(kind("char(3)"), Kind::Char);
        assert_eq!(kind("row(\"a\" integer, \"b\" varchar)"), Kind::Nested);
        assert_eq!(kind("map(varchar, integer)"), Kind::Nested);
        assert_eq!(kind("array(integer)"), Kind::Nested);
        assert_eq!(kind("tinyint"), Kind::Int);
        assert_eq!(kind("ipaddress"), Kind::Other);
    }

    #[test]
    fn read_expressions() {
        assert_eq!(read_expr("d", "double"), "CAST(\"d\" AS VARCHAR)");
        assert_eq!(read_expr("r", "real"), "CAST(\"r\" AS VARCHAR)");
        assert_eq!(read_expr("t", "timestamp(9) with time zone"), "to_iso8601(\"t\")");
        assert_eq!(read_expr("a", "array(integer)"), "json_format(CAST(\"a\" AS JSON))");
        assert_eq!(read_expr("m", "map(varchar, varbinary)"), "\"m\"");
        assert_eq!(read_expr("x\"y", "bigint"), "\"x\"\"y\"");
    }

    #[test]
    fn cells_from_the_answer() {
        assert_eq!(to_cell(Value::Null, "bigint"), Cell::Null);
        assert_eq!(to_cell(json!(9007199254740993i64), "bigint"), Cell::Int(9007199254740993));
        assert_eq!(to_cell(json!(true), "boolean"), Cell::Bool(true));
        assert_eq!(to_cell(json!("1.0E-1"), "double"), Cell::Float(0.1));
        assert_eq!(to_cell(json!("-Infinity"), "real"), Cell::Float(f64::NEG_INFINITY));
        assert!(matches!(to_cell(json!("NaN"), "double"), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(json!("12345678901234567890.123456789"), "decimal(38,9)"), Cell::Decimal("12345678901234567890.123456789".into()));
        assert_eq!(to_cell(json!("yv4="), "varbinary"), Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(to_cell(json!("-0005-03-01"), "date"), Cell::Date("-0005-03-01".into()));
        assert_eq!(to_cell(json!("10:00:00.123456789012"), "time(12)"), Cell::Time("10:00:00.123456789012".into()));
        assert_eq!(to_cell(json!("10:00:00.12+01:00"), "time(2) with time zone"), Cell::Text("10:00:00.12+01:00".into()));
        assert_eq!(
            to_cell(json!("2024-01-02 03:04:05.123456789012"), "timestamp(12)"),
            Cell::DateTime("2024-01-02 03:04:05.123456789012".into())
        );
        assert_eq!(
            to_cell(json!("2024-01-02T03:04:05.123456-05:00"), "timestamp(6) with time zone"),
            Cell::DateTimeTz("2024-01-02 03:04:05.123456-05:00".into())
        );
        assert_eq!(to_cell(json!("2024-01-02T03:04:05Z"), "timestamp(0) with time zone"), Cell::DateTimeTz("2024-01-02 03:04:05+00:00".into()));
        assert_eq!(to_cell(json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), "uuid"), Cell::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()));
        assert_eq!(to_cell(json!("{\"z\":[1]}"), "json"), Cell::Json("{\"z\":[1]}".into()));
        assert_eq!(to_cell(json!("{\"a\":1}"), "row(\"a\" integer)"), Cell::Json("{\"a\":1}".into()));
        assert_eq!(to_cell(json!({"k": "yv4="}), "map(varchar, varbinary)"), Cell::Json("{\"k\":\"yv4=\"}".into()));
        assert_eq!(to_cell(json!("10.0.0.1"), "ipaddress"), Cell::Text("10.0.0.1".into()));
        assert_eq!(to_cell(json!("ab "), "char(3)"), Cell::Text("ab ".into()));
    }

    #[test]
    fn literals_per_target_type() {
        let t = |s: &str| Cell::Text(s.into());
        assert_eq!(lit_ok(&Cell::Null, "integer"), "NULL");
        assert_eq!(lit_ok(&Cell::Bool(true), "boolean"), "TRUE");
        assert_eq!(lit_ok(&Cell::Int(0), "boolean"), "FALSE");
        assert_eq!(lit_ok(&t("t"), "boolean"), "TRUE");
        assert_eq!(lit_ok(&t("quizá"), "boolean"), "CAST('quizá' AS boolean)");
        assert_eq!(lit_ok(&Cell::Int(-5), "tinyint"), "TINYINT '-5'");
        assert_eq!(lit_ok(&Cell::Int(300), "smallint"), "SMALLINT '300'");
        assert_eq!(lit_ok(&Cell::Int(70_000), "integer"), "INTEGER '70000'");
        assert_eq!(lit_ok(&Cell::Int(i64::MIN), "bigint"), "BIGINT '-9223372036854775808'");
        assert_eq!(lit_ok(&Cell::Int(-1), "bigint"), "-1");
        assert_eq!(lit_ok(&Cell::UInt(u64::MAX), "decimal(20,0)"), "DECIMAL '18446744073709551615'");
        assert_eq!(lit_ok(&t(" +42 "), "bigint"), "42");
        assert_eq!(lit_ok(&Cell::Float(3.0), "bigint"), "3");
        assert_eq!(lit_ok(&t("4x"), "bigint"), "CAST('4x' AS bigint)");
        assert_eq!(lit_ok(&Cell::Float(0.1), "double"), "1e-1");
        assert_eq!(lit_ok(&Cell::Float(f64::NAN), "real"), "CAST(nan() AS real)");
        assert_eq!(lit_ok(&Cell::Float(f64::NAN), "double"), "nan()");
        assert_eq!(lit_ok(&Cell::Float(1.5), "real"), "REAL '1.5e0'");
        assert_eq!(lit_ok(&Cell::Float(f64::INFINITY), "real"), "CAST(infinity() AS real)");
        assert_eq!(lit_ok(&Cell::Int(3), "real"), "REAL '3'");
        assert_eq!(lit_ok(&Cell::Float(f64::NEG_INFINITY), "double"), "-infinity()");
        assert_eq!(lit_ok(&Cell::Decimal("1.5".into()), "double"), "DOUBLE '1.5'");
        assert_eq!(lit_ok(&Cell::Decimal("-12.340".into()), "decimal(10,3)"), "DECIMAL '-12.340'");
        assert_eq!(lit_ok(&Cell::Int(7), "decimal(10,2)"), "DECIMAL '7'");
        assert_eq!(lit_ok(&Cell::Float(0.25), "decimal(10,2)"), "DECIMAL '0.25'");
        assert_eq!(lit_ok(&Cell::Decimal("123456789".into()), "decimal(10,2)"), "CAST(DECIMAL '123456789' AS decimal(10,2))");
        assert_eq!(lit_ok(&Cell::Decimal("-0012345678.9".into()), "decimal(10,2)"), "DECIMAL '-12345678.9'");
        assert_eq!(lit_ok(&t("abc"), "decimal(10,2)"), "CAST('abc' AS decimal(10,2))");
        assert_eq!(lit_ok(&t("O'Brien"), "varchar"), "'O''Brien'");
        assert_eq!(lit_ok(&Cell::Int(5), "varchar(3)"), "'5'");
        assert_eq!(lit_ok(&Cell::Bytes(vec![0xFF]), "varchar"), "from_utf8(X'FF')");
        assert_eq!(lit_ok(&Cell::Bytes(b"ok".to_vec()), "varchar"), "'ok'");
        assert_eq!(lit_ok(&Cell::Bytes(vec![0xCA, 0xFE]), "varbinary"), "X'CAFE'");
        assert_eq!(lit_ok(&Cell::Bytes(vec![]), "varbinary"), "X''");
        assert!(lit_ok(&Cell::Bytes(vec![0; 2000]), "varbinary").starts_with("from_base64('AAAA"));
        assert_eq!(lit_ok(&t("ab"), "varbinary"), "X'6162'");
        assert_eq!(lit_ok(&Cell::Date("2024-01-31".into()), "date"), "DATE '2024-01-31'");
        assert_eq!(lit_ok(&Cell::DateTime("2024-01-31T00:00:00".into()), "date"), "CAST(TIMESTAMP '2024-01-31 00:00:00' AS date)");
        assert_eq!(lit_ok(&Cell::Time("10:00:00.123456789012".into()), "time(12)"), "TIME '10:00:00.123456789012'");
        assert_eq!(lit_ok(&Cell::Text("10:00:00+01:00".into()), "time(0) with time zone"), "TIME '10:00:00+01:00'");
        assert_eq!(
            lit_ok(&Cell::DateTime("2024-01-31T10:00:00.1234567".into()), "timestamp(7)"),
            "TIMESTAMP '2024-01-31 10:00:00.1234567'"
        );
        assert_eq!(
            lit_ok(&Cell::DateTimeTz("2024-01-31 10:00:00+01:00".into()), "timestamp(3)"),
            "CAST(TIMESTAMP '2024-01-31 10:00:00+01:00' AT TIME ZONE 'UTC' AS timestamp(3))"
        );
        assert_eq!(
            lit_ok(&Cell::DateTimeTz("2024-01-31 10:00:00.5-03:00".into()), "timestamp(3) with time zone"),
            "TIMESTAMP '2024-01-31 10:00:00.5-03:00'"
        );
        assert_eq!(lit_ok(&Cell::DateTime("2024-01-31 10:00:00".into()), "timestamp(3) with time zone"), "TIMESTAMP '2024-01-31 10:00:00 UTC'");
        assert_eq!(lit_ok(&Cell::Date("2024-01-31".into()), "timestamp(3) with time zone"), "TIMESTAMP '2024-01-31 00:00:00 UTC'");
        assert_eq!(lit_ok(&Cell::Uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11".into()), "uuid"), "UUID 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'");
        assert_eq!(lit_ok(&Cell::Bytes((0..16).collect()), "uuid"), "UUID '00010203-0405-0607-0809-0a0b0c0d0e0f'");
        assert_eq!(lit_ok(&Cell::Json("{\"a\":1}".into()), "json"), "JSON '{\"a\":1}'");
        assert_eq!(lit_ok(&t("hola"), "json"), "JSON '\"hola\"'");
        assert_eq!(lit_ok(&Cell::Int(3), "json"), "JSON '3'");
        assert_eq!(lit_ok(&Cell::Json("[1,\"it's\"]".into()), "row(\"a\" integer, \"b\" varchar)"), "CAST(ROW(INTEGER '1', 'it''s') AS row(\"a\" integer, \"b\" varchar))");
        assert_eq!(lit_ok(&t("10.0.0.1"), "ipaddress"), "CAST('10.0.0.1' AS ipaddress)");
        assert_eq!(lit_ok(&Cell::Bytes(vec![1]), "geometry"), "CAST(X'01' AS geometry)");
    }

    #[test]
    fn generated_statements() {
        let types = vec!["bigint".to_string(), "varchar".to_string(), "decimal(10,2)".to_string()];
        let head = insert_head(Some("s"), "t", &["id".into(), "name".into(), "amount".into()]);
        assert_eq!(head, "INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"amount\") VALUES\n");
        let mut st = Statements::new(head.clone());
        assert!(st.push(&row_tuple(&[Cell::Int(1), Cell::Text("a".into()), Cell::Decimal("1.50".into())], &types).unwrap()).is_none());
        assert!(st.push(&row_tuple(&[Cell::Int(2), Cell::Null, Cell::Null], &types).unwrap()).is_none());
        let (sql, rows) = st.take().unwrap();
        assert_eq!(rows, 2);
        assert_eq!(sql, format!("{head}(1, 'a', DECIMAL '1.50'),\n(2, NULL, NULL)"));
        assert!(st.take().is_none());

        // Split by bytes: no statement over the bound (a single larger row goes alone).
        let mut st = Statements::new(head.clone());
        let tuple = format!("('{}')", "x".repeat(300_000));
        let mut out = Vec::new();
        for _ in 0..7 {
            out.extend(st.push(&tuple));
        }
        out.extend(st.take());
        assert_eq!(out.iter().map(|(_, n)| n).sum::<u64>(), 7);
        assert!(out.iter().all(|(s, _)| s.len() <= STMT_BYTES));
        let mut st = Statements::new(head.clone());
        let huge = format!("('{}')", "x".repeat(STMT_BYTES));
        assert!(st.push("(1)").is_none());
        assert_eq!(st.push(&huge).map(|(_, n)| n), Some(1));
        assert_eq!(st.take().map(|(_, n)| n), Some(1));

        // Split by rows.
        let mut st = Statements::new(head);
        let mut n = 0;
        for _ in 0..STMT_ROWS + 1 {
            n += st.push("(1)").map_or(0, |(_, r)| r);
        }
        assert_eq!(n, STMT_ROWS as u64);
        assert_eq!(st.take().map(|(_, n)| n), Some(1));
    }

    #[test]
    fn too_long_statements_are_explained() {
        let e = explain_load_error(Error::Query("Query text length (1200000) exceeds the maximum length (1000000)".into()));
        assert!(matches!(e, Error::Query(m) if m.contains("query.max-length")));
        assert!(matches!(explain_load_error(Error::Cancelled), Error::Cancelled));
    }

    #[test]
    fn nested_types_parse() {
        let leaf = |s: &str| Ty::Leaf(s.into());
        assert_eq!(parse_ty("array(date)"), Ty::Array(Box::new(leaf("date"))));
        assert_eq!(parse_ty("map(varchar, array(decimal(10, 2)))"), Ty::Map(Box::new(leaf("varchar")), Box::new(Ty::Array(Box::new(leaf("decimal(10, 2)"))))));
        assert_eq!(
            parse_ty("row(\"a\"\"b\" uuid, c timestamp(3) with time zone, \"time\" time(3))"),
            Ty::Row(vec![(Some("a\"b".into()), leaf("uuid")), (Some("c".into()), leaf("timestamp(3) with time zone")), (Some("time".into()), leaf("time(3)"))])
        );
        assert_eq!(parse_ty("row(integer, timestamp(3) with time zone)"), Ty::Row(vec![(None, leaf("integer")), (None, leaf("timestamp(3) with time zone"))]));
        assert_eq!(parse_ty("row(time time, x decimal(10, 2))"), Ty::Row(vec![(Some("time".into()), leaf("time")), (Some("x".into()), leaf("decimal(10, 2)"))]));
    }

    /// Problem: nested columns with types JSON can't take failed the whole read.
    #[test]
    fn nested_reads_fall_back_to_raw_values() {
        for ty in ["array(uuid)", "array(time(3))", "array(timestamp(3) with time zone)", "array(char(3))", "map(date, integer)", "row(\"a\" integer, \"b\" ipaddress)", "array(varbinary)", "map(varchar, varbinary)"] {
            assert_eq!(read_expr("c", ty), "\"c\"", "{ty}");
        }
        for ty in ["array(date)", "array(timestamp(6))", "map(integer, varchar)", "row(\"a\" decimal(10,2), \"b\" array(json))", "array(varchar(3))"] {
            assert_eq!(read_expr("c", ty), "json_format(CAST(\"c\" AS JSON))", "{ty}");
        }
    }

    /// Problem: `CAST(JSON … AS array(date))` fails and rounds decimals;
    /// nested values are now built element by element.
    #[test]
    fn nested_values_are_built_element_by_element() {
        let j = |s: &str| Cell::Json(s.into());
        assert_eq!(lit_ok(&j("[\"2024-01-01\",null]"), "array(date)"), "CAST(ARRAY[DATE '2024-01-01', NULL] AS array(date))");
        assert_eq!(lit_ok(&j("[\"2024-01-01 00:00:00.123456\"]"), "array(timestamp(6))"), "CAST(ARRAY[TIMESTAMP '2024-01-01 00:00:00.123456'] AS array(timestamp(6)))");
        assert_eq!(lit_ok(&j("[]"), "array(uuid)"), "CAST(ARRAY[] AS array(uuid))");
        assert_eq!(
            lit_ok(&j("[\"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\"]"), "array(uuid)"),
            "CAST(ARRAY[UUID 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'] AS array(uuid))"
        );
        assert_eq!(lit_ok(&j("{\"k\":\"yv4=\",\"h\":\"0xCAFE\"}"), "map(varchar, varbinary)"), "CAST(MAP(ARRAY['k', 'h'], ARRAY[X'CAFE', X'CAFE']) AS map(varchar, varbinary))");
        assert_eq!(lit_ok(&j("{}"), "map(date, integer)"), "CAST(MAP() AS map(date, integer))");
        assert_eq!(lit_ok(&j("{\"2024-01-01\":1}"), "map(date, integer)"), "CAST(MAP(ARRAY[DATE '2024-01-01'], ARRAY[INTEGER '1']) AS map(date, integer))");
        // Decimals keep every digit (no f64 on the way) and aren't rounded.
        assert_eq!(
            lit_ok(&j("[12345678901234567890.123456789]"), "array(decimal(38,9))"),
            "CAST(ARRAY[DECIMAL '12345678901234567890.123456789'] AS array(decimal(38,9)))"
        );
        assert!(literal(&j("[1.239]"), "array(decimal(10,2))").is_err());
        // A ROW by field name (any order, missing ones NULL) or by position.
        assert_eq!(
            lit_ok(&j("{\"b\":\"2024-01-01\",\"a\":\"10:00:00.5\"}"), "row(\"a\" time(3), \"b\" date, \"c\" varchar)"),
            "CAST(ROW(TIME '10:00:00.5', DATE '2024-01-01', NULL) AS row(\"a\" time(3), \"b\" date, \"c\" varchar))"
        );
        assert_eq!(lit_ok(&j("[[1.5,\"NaN\"]]"), "array(array(double))"), "CAST(ARRAY[ARRAY[DOUBLE '1.5', DOUBLE 'NaN']] AS array(array(double)))");
        assert_eq!(lit_ok(&j("[{\"x\":\"it's\"}]"), "array(json)"), "CAST(ARRAY[JSON '{\"x\":\"it''s\"}'] AS array(json))");
        assert!(literal(&j("{\"zz\":1}"), "row(\"a\" integer)").is_err());
        assert!(literal(&j("[1,2]"), "row(\"a\" integer)").is_err());
        assert!(literal(&j("{\"a\":1}"), "array(integer)").is_err());
        assert!(literal(&j("no es json"), "array(integer)").is_err());
        assert!(literal(&Cell::Int(1), "array(integer)").is_err());
    }

    /// Problem: a decimal with more fraction digits than the scale was rounded.
    #[test]
    fn decimals_are_never_rounded() {
        let d = |s: &str| Cell::Decimal(s.into());
        assert!(matches!(literal(&d("1.239"), "decimal(10,2)"), Err(Error::Query(m)) if m.contains("no se redondea")));
        assert!(literal(&Cell::Float(1.005), "decimal(10,2)").is_err());
        assert_eq!(lit_ok(&d("1.230"), "decimal(10,2)"), "DECIMAL '1.23'");
        assert_eq!(lit_ok(&d("2.000"), "decimal(10,0)"), "DECIMAL '2'");
        assert_eq!(lit_ok(&d("1.5E3"), "decimal(10,2)"), "DECIMAL '1500'");
        assert_eq!(lit_ok(&d("-1.25e-1"), "decimal(10,3)"), "DECIMAL '-0.125'");
        assert!(literal(&d("1.25e-3"), "decimal(10,3)").is_err());
        assert_eq!(plain_decimal("+007.50"), Some("7.50".into()));
        assert_eq!(plain_decimal("12e2"), Some("1200".into()));
        assert_eq!(plain_decimal("x"), None);
    }

    /// Problem: TIME / TIMESTAMP values with more fraction digits than the
    /// column's precision were rounded by the server (`23:59:59.9999` into
    /// `time(3)` became midnight).
    #[test]
    fn times_are_never_rounded() {
        let no = |c: Cell, ty: &str| matches!(literal(&c, ty), Err(Error::Query(m)) if m.contains("no se redondea"));
        assert!(no(Cell::Time("23:59:59.9999".into()), "time(3)"));
        assert!(no(Cell::Time("23:59:59.9999".into()), "time"));
        assert!(no(Cell::DateTime("2024-06-01 12:00:00.123999".into()), "timestamp(3)"));
        assert!(no(Cell::DateTime("2024-06-01T12:00:00.1234".into()), "timestamp"));
        assert!(no(Cell::DateTimeTz("2024-06-01 12:00:00.1234+01:00".into()), "timestamp(3)"));
        assert!(no(Cell::DateTimeTz("2024-06-01 12:00:00.1234+01:00".into()), "timestamp(3) with time zone"));
        assert!(no(Cell::Text("2024-06-01 12:00:00.1234 America/Tijuana".into()), "timestamp(3) with time zone"));
        assert!(no(Cell::Text("10:00:00.5+01:00".into()), "time(0) with time zone"));
        assert!(no(Cell::Text("10:00:00.12+01:00".into()), "time(1)"));
        assert!(no(Cell::DateTime("2024-06-01 12:00:00.5".into()), "time(0)"));
        assert!(no(Cell::Time("10:00:00.5".into()), "time(0) with time zone"));
        assert!(no(Cell::DateTime("2024-06-01 12:00:00.5".into()), "time(0) with time zone"));
        assert!(no(Cell::Time("10:00:00.1234567890123".into()), "time(12)"));
        // Nested elements go through the same literals.
        assert!(literal(&Cell::Json("[\"10:00:00.1234\"]".into()), "array(time(3))").is_err());
        // Trailing zeros past the precision are not a loss.
        assert_eq!(lit_ok(&Cell::Time("23:59:59.999000".into()), "time(3)"), "TIME '23:59:59.999'");
        assert_eq!(lit_ok(&Cell::DateTime("2024-06-01 12:00:00.000".into()), "timestamp(0)"), "TIMESTAMP '2024-06-01 12:00:00'");
        assert_eq!(
            lit_ok(&Cell::DateTimeTz("2024-06-01 12:00:00.500000+01:00".into()), "timestamp(3) with time zone"),
            "TIMESTAMP '2024-06-01 12:00:00.500+01:00'"
        );
        assert_eq!(lit_ok(&Cell::Time("10:00:00.12".into()), "time(3)"), "TIME '10:00:00.12'");
        assert_eq!(lit_ok(&Cell::Time("10:00:00".into()), "time(0)"), "TIME '10:00:00'");
        // A date target checks for a time of day, not the fraction's precision.
        assert!(literal(&Cell::DateTime("2024-06-01 00:00:00.0000".into()), "date").is_ok());
        assert_eq!(fit_fraction("2024-06-01 12:00:00.123000+05:30", "timestamp(3)").unwrap(), "2024-06-01 12:00:00.123+05:30");
        assert_eq!(fit_fraction("12:00:00+05:30", "time(0)").unwrap(), "12:00:00+05:30");
        assert_eq!(time_precision("timestamp(6) with time zone"), 6);
        assert_eq!(time_precision("time with time zone"), 3);
    }

    /// Problem: a text with an offset into a zoneless TIMESTAMP kept its
    /// local wall time while a zoned cell was converted to UTC.
    #[test]
    fn zones_are_handled_the_same_whatever_the_cell() {
        let t = |s: &str| Cell::Text(s.into());
        let utc = "CAST(TIMESTAMP '2024-01-01 00:00:00+01:00' AT TIME ZONE 'UTC' AS timestamp(3))";
        assert_eq!(lit_ok(&t("2024-01-01 00:00:00+01:00"), "timestamp(3)"), utc);
        assert_eq!(lit_ok(&t("2024-01-01T00:00:00+01:00"), "timestamp(3)"), utc);
        assert_eq!(lit_ok(&Cell::DateTimeTz("2024-01-01 00:00:00+01:00".into()), "timestamp(3)"), utc);
        assert_eq!(lit_ok(&t("2024-01-01T00:00:00Z"), "timestamp(3)"), "CAST(TIMESTAMP '2024-01-01 00:00:00+00:00' AT TIME ZONE 'UTC' AS timestamp(3))");
        assert_eq!(
            lit_ok(&t("2024-01-01 00:00:00 America/Tijuana"), "timestamp(3)"),
            "CAST(TIMESTAMP '2024-01-01 00:00:00 America/Tijuana' AT TIME ZONE 'UTC' AS timestamp(3))"
        );
        assert_eq!(lit_ok(&t("2024-01-01 00:00:00"), "timestamp(3)"), "TIMESTAMP '2024-01-01 00:00:00'");
        // Zoneless into a zoned column: UTC, as for a DateTime cell.
        assert_eq!(lit_ok(&t("2024-01-01 00:00:00"), "timestamp(3) with time zone"), "TIMESTAMP '2024-01-01 00:00:00 UTC'");
        assert_eq!(lit_ok(&Cell::Time("10:00:00".into()), "time(0) with time zone"), "TIME '10:00:00+00:00'");
        assert_eq!(lit_ok(&t("10:00:00+01:00"), "time(3)"), "CAST(TIME '10:00:00+01:00' AT TIME ZONE 'UTC' AS time(3))");
        assert_eq!(lit_ok(&Cell::DateTimeTz("2024-01-01 01:00:00+01:00".into()), "date"), "CAST(TIMESTAMP '2024-01-01 01:00:00+01:00' AT TIME ZONE 'UTC' AS date)");
        assert_eq!(lit_ok(&Cell::Date("-0005-03-01".into()), "date"), "DATE '-0005-03-01'");
        assert!(!has_zone("-0005-03-01 00:00:00"));
        assert!(!has_zone("10:00:00.123"));
    }

    /// Problem: a text `123` / `true` / `null` became a JSON number,
    /// boolean or null in a JSON column.
    #[test]
    fn texts_stay_strings_in_json_columns() {
        let t = |s: &str| Cell::Text(s.into());
        assert_eq!(lit_ok(&t("123"), "json"), "JSON '\"123\"'");
        assert_eq!(lit_ok(&t("true"), "json"), "JSON '\"true\"'");
        assert_eq!(lit_ok(&t("null"), "json"), "JSON '\"null\"'");
        assert_eq!(lit_ok(&t("{\"a\":1}"), "json"), "JSON '{\"a\":1}'");
        assert_eq!(lit_ok(&t("[1]"), "json"), "JSON '[1]'");
        assert_eq!(lit_ok(&Cell::Json("123".into()), "json"), "JSON '123'");
    }

    /// Problem: `to_iso8601` writes a year past 9999 as `+12024-…`.
    #[test]
    fn years_past_9999() {
        assert_eq!(iso_to_tz("+12024-01-02T03:04:05Z"), "12024-01-02 03:04:05+00:00");
        assert_eq!(to_cell(json!("+12024-01-02T03:04:05.5Z"), "timestamp(1) with time zone"), Cell::DateTimeTz("12024-01-02 03:04:05.5+00:00".into()));
        assert_eq!(to_cell(json!("+12024-01-01"), "date"), Cell::Date("12024-01-01".into()));
        assert_eq!(to_cell(json!("+12024-01-02 03:04:05"), "timestamp(0)"), Cell::DateTime("12024-01-02 03:04:05".into()));
        assert_eq!(lit_ok(&Cell::DateTimeTz("12024-01-02 03:04:05+00:00".into()), "timestamp(0) with time zone"), "TIMESTAMP '12024-01-02 03:04:05+00:00'");
    }

    /// Problems: the spec's commit window was ignored, and statements were
    /// sized by rows only.
    #[test]
    fn statements_stay_within_the_commit_window() {
        let mut st = Statements::new("INSERT INTO t VALUES\n".into()).window(1, 3, u64::MAX);
        let mut sizes = Vec::new();
        for _ in 0..7 {
            sizes.extend(st.push("(1)").map(|(_, n)| n));
        }
        sizes.extend(st.take().map(|(_, n)| n));
        assert_eq!(sizes, vec![3, 3, 1]);
        let mut st = Statements::new("H".into()).window(1, u64::MAX, 10);
        assert!(st.push("(12345)").is_none());
        assert_eq!(st.push("(12345)").map(|(_, n)| n), Some(1));
        let st = Statements::new("H".into()).window(1, 0, 0);
        assert_eq!((st.max_rows, st.max_bytes), (1, 1));
        let st = Statements::new("H".into()).window(6, LoadSpec::DEFAULT_COMMIT_ROWS, LoadSpec::DEFAULT_COMMIT_BYTES);
        assert_eq!((st.max_rows, st.max_bytes), (STMT_ROWS, STMT_BYTES));
        // Wide tables: fewer rows, so the values a statement holds stay bounded.
        assert_eq!(Statements::new("H".into()).window(60, u64::MAX, u64::MAX).max_rows, 100);
        assert_eq!(Statements::new("H".into()).window(10_000, u64::MAX, u64::MAX).max_rows, 1);
    }

    /// Problem: a lone ROW column was written `(CAST(ROW(…) AS row(…)))`,
    /// which VALUES spreads over the row's fields.
    #[test]
    fn lone_values_go_as_rows() {
        let ty = vec!["row(a integer, b varchar)".to_string()];
        let row = [Cell::Json("{\"a\":1,\"b\":\"x\"}".into())];
        let t = row_tuple(&row, &ty).unwrap();
        assert!(t.starts_with("ROW(CAST(ROW(") && t.ends_with("))"), "{t}");
        assert_eq!(row_tuple(&[Cell::Null], &ty).unwrap(), "ROW(NULL)");
        assert_eq!(row_tuple(&[Cell::Int(1)], &["bigint".to_string()]).unwrap(), "ROW(1)");
        assert_eq!(row_tuple(&[Cell::Int(1), Cell::Null], &["bigint".to_string(), "varchar".to_string()]).unwrap(), "(1, NULL)");
    }

    /// Problem: a timestamp into a DATE column lost its time of day
    /// without an error.
    #[test]
    fn dates_never_lose_the_time_of_day() {
        let lost = |c: Cell| {
            let r = literal(&c, "date");
            assert!(matches!(&r, Err(Error::Query(m)) if m.contains("se perdería la hora")), "{c:?}: {r:?}");
        };
        lost(Cell::DateTime("2024-01-01 12:00:00".into()));
        lost(Cell::Text("2024-01-01 12:00:00".into()));
        lost(Cell::Text("2024-01-01T00:00:00.001".into()));
        lost(Cell::DateTimeTz("2024-01-01 00:00:00+01:00".into()));
        lost(Cell::DateTimeTz("2024-01-01 00:30:00+01:00".into()));
        lost(Cell::Text("2024-01-01 00:00:00-03:00".into()));
        let r = literal(&Cell::Text("2024-01-01 00:00:00 America/Tijuana".into()), "date");
        assert!(matches!(&r, Err(Error::Query(m)) if m.contains("por nombre")), "{r:?}");
        assert_eq!(lit_ok(&Cell::DateTime("2024-01-01 00:00:00".into()), "date"), "CAST(TIMESTAMP '2024-01-01 00:00:00' AS date)");
        assert_eq!(lit_ok(&Cell::Text("2024-01-01T00:00:00.000".into()), "date"), "CAST(TIMESTAMP '2024-01-01 00:00:00.000' AS date)");
        assert_eq!(lit_ok(&Cell::Text("2024-01-01 00:00".into()), "date"), "CAST(TIMESTAMP '2024-01-01 00:00' AS date)");
        assert_eq!(
            lit_ok(&Cell::DateTimeTz("2024-01-01T00:00:00Z".into()), "date"),
            "CAST(TIMESTAMP '2024-01-01 00:00:00+00:00' AT TIME ZONE 'UTC' AS date)"
        );
        assert_eq!(
            lit_ok(&Cell::DateTimeTz("2024-01-01 21:00:00-03:00".into()), "date"),
            "CAST(TIMESTAMP '2024-01-01 21:00:00-03:00' AT TIME ZONE 'UTC' AS date)"
        );
        assert_eq!(lit_ok(&Cell::Text("2024-01-01 05:30:00+0530".into()), "date"), "CAST(TIMESTAMP '2024-01-01 05:30:00+0530' AT TIME ZONE 'UTC' AS date)");
        assert_eq!(lit_ok(&Cell::Text("2024-01-01 00:00:00 UTC".into()), "date"), "CAST(TIMESTAMP '2024-01-01 00:00:00 UTC' AT TIME ZONE 'UTC' AS date)");
        assert_eq!(lit_ok(&Cell::Date("2024-01-01".into()), "date"), "DATE '2024-01-01'");
        assert_eq!(lit_ok(&Cell::Date("-0005-03-01 00:00:00".into()), "date"), "CAST(TIMESTAMP '-0005-03-01 00:00:00' AS date)");
    }

    #[test]
    fn query_ids_from_next_uris() {
        assert_eq!(query_id("http://h:8080/v1/statement/queued/20240101_000000_00001_abcde/y123/1"), Some("20240101_000000_00001_abcde"));
        assert_eq!(query_id("http://h:8080/v1/statement/executing/20240101_000000_00001_abcde/y123/2"), Some("20240101_000000_00001_abcde"));
        assert_eq!(query_id("http://h:8080/v1/statement/executing/q1/5"), Some("q1"));
        assert_eq!(query_id("http://h:8080/v1/info"), None);
    }
}

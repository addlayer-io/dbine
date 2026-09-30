//! Bulk transfer (see `dbine_driver::transfer`) for Amazon Athena.
//!
//! Reading: one `SELECT <columns> FROM t [WHERE filter]` query execution,
//! paged with `GetQueryResults` (1,000 rows a page, `NextToken`) and handed
//! over page by page, never kept whole. Values are typed by the table's
//! catalog types (Glue spells them the Hive way: `string`, `int`,
//! `binary`, `struct<…>`): DECIMAL comes as exact text; REAL / DOUBLE as
//! text (`CAST … AS VARCHAR`, the shortest exact form); TIMESTAMP WITH TIME
//! ZONE through `to_iso8601` (a numeric offset); VARBINARY through
//! `to_hex`, decoded whole; ARRAY / MAP / STRUCT as JSON (`json_format(CAST
//! (… AS JSON))`, so a struct keeps its field names).
//!
//! Loading: Athena only writes through `INSERT INTO`, so it's multi-row
//! `INSERT … VALUES` with each literal typed for the target column
//! (`DECIMAL '…'`, `TIMESTAMP '…'`, `X'…'`, `CAST(… AS <column type>)`,
//! nested values from JSON…). Statements close at `commit_rows` rows or at
//! `min(commit_bytes, STMT_BYTES)` bytes of SQL text (Athena's query text
//! limit), whichever comes first. Only Iceberg tables load: an Iceberg
//! INSERT is one snapshot commit, all or nothing, so each statement is a
//! committed window and `progress` is called after each one. A Hive table's
//! failed INSERT can leave the files it already wrote in S3, with no way to
//! undo them, so those (and views, Delta Lake and Hudi tables, which Athena
//! only reads) are refused up front with the reason. A statement that hits
//! Athena's 100-open-partitions limit committed nothing and is run again in
//! halves. Statements run one at a time: concurrent commits to an Iceberg
//! table conflict. Nested columns holding VARBINARY are refused both ways:
//! Athena can't turn them into JSON or back.
//!
//! Cancelling: the finished query's id stays in the session's `running`
//! slot while its pages are fetched (and between INSERTs; before the first
//! one, a marker); the interrupter takes it, and the next page or statement
//! sees that and stops. A new query replaces what's in the slot only if it's
//! still there, so a cancel during `StartQueryExecution` stops that query.
//! No query is left running when the transfer returns or is dropped: if its
//! polling fails, it's stopped and watched to its final state (an INSERT
//! stopped while committing may still succeed: then its rows are counted).
//! Nested date / time values are checked like top-level ones (no rounding,
//! no zone dropped) and their ISO `T` form is accepted.

use crate::{err, is_header, row_values, AthenaSession};
use aws_sdk_athena::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_athena::Client;
use aws_sdk_athena::types::{QueryExecutionContext, QueryExecutionState, ResultConfiguration, StatementType, TableMetadata};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Athena's query text limit (`StartQueryExecution`'s `QueryString`).
pub(crate) const STMT_BYTES: usize = 262_144;
/// Rows of a `GetQueryResults` page.
const PAGE: i32 = 1000;

/// The families of types the transfer tells apart.
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
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    /// ARRAY / MAP / STRUCT (ROW).
    Nested,
    Other,
}

/// A type as the catalog (Hive names) or the engine (Trino names) spells it.
pub(crate) fn kind(ty: &str) -> Kind {
    let t = ty.trim().to_ascii_lowercase();
    let base = t.split(['(', '<']).next().unwrap_or("").trim();
    let tz = t.ends_with("with time zone") || base == "timestamptz";
    match base {
        "boolean" => Kind::Bool,
        "tinyint" | "smallint" | "integer" | "int" | "bigint" => Kind::Int,
        "real" | "float" => Kind::Real,
        "double" => Kind::Double,
        "decimal" => Kind::Decimal,
        "varchar" | "char" | "string" => Kind::Char,
        "varbinary" | "binary" => Kind::Binary,
        "date" => Kind::Date,
        "time" => Kind::Time,
        "timestamp" | "timestamp with time zone" | "timestamptz" if tz => Kind::TimestampTz,
        "timestamp" => Kind::Timestamp,
        "uuid" => Kind::Uuid,
        "json" => Kind::Json,
        "array" | "map" | "struct" | "row" => Kind::Nested,
        _ => Kind::Other,
    }
}

/// Split at the commas outside `<…>` / `(…)`.
fn top_level(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut out) = (0i32, 0usize, Vec::new());
    for (i, ch) in s.char_indices() {
        match ch {
            '<' | '(' => depth += 1,
            '>' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// A type as a tree: the nested types and their leaves.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Array(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    /// Fields with their names (none in an anonymous `row(integer)`).
    Row(Vec<(Option<String>, Ty)>),
    Leaf(String),
}

/// What's inside `kw<…>` / `kw(…)`, and whether it was `<…>` (Hive syntax).
fn wrapped<'a>(t: &'a str, kw: &str) -> Option<(&'a str, bool)> {
    if !t.get(..kw.len())?.eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = t[kw.len()..].trim_start();
    let (hive, close) = match rest.chars().next()? {
        '<' => (true, '>'),
        '(' => (false, ')'),
        _ => return None,
    };
    rest.strip_suffix(close).map(|r| (&r[1..], hive))
}

/// A field of an engine-syntax `row(…)`: `"na""me" type`, `name type` or
/// just `type`.
fn row_field(f: &str) -> (Option<String>, &str) {
    if let Some(rest) = f.strip_prefix('"') {
        let mut from = 0;
        while let Some(j) = rest[from..].find('"') {
            let k = from + j;
            if rest[k + 1..].starts_with('"') {
                from = k + 2;
            } else {
                return (Some(rest[..k].replace("\"\"", "\"")), rest[k + 1..].trim());
            }
        }
        return (None, f);
    }
    let lower = f.to_ascii_lowercase();
    match f.split_once(char::is_whitespace) {
        // `timestamp(3) with time zone` alone is a type, not a named field.
        Some(_) if lower.starts_with("time") && lower.ends_with("time zone") => (None, f),
        Some((name, ty)) => (Some(name.to_string()), ty.trim()),
        None => (None, f),
    }
}

fn parse_ty(s: &str) -> Ty {
    let t = s.trim();
    if let Some((inner, _)) = wrapped(t, "array") {
        return Ty::Array(Box::new(parse_ty(inner)));
    }
    if let Some((inner, _)) = wrapped(t, "map") {
        if let [k, v] = top_level(inner).as_slice() {
            return Ty::Map(Box::new(parse_ty(k)), Box::new(parse_ty(v)));
        }
    }
    for kw in ["struct", "row"] {
        if let Some((inner, hive)) = wrapped(t, kw) {
            let fields = top_level(inner)
                .into_iter()
                .filter(|f| !f.is_empty())
                .map(|f| {
                    let (name, ty) = match (hive, f.split_once(':')) {
                        (true, Some((n, ty))) => (Some(n.trim().to_string()), ty),
                        (true, None) => (None, f),
                        (false, _) => row_field(f),
                    };
                    (name, parse_ty(ty))
                })
                .collect();
            return Ty::Row(fields);
        }
    }
    Ty::Leaf(t.to_string())
}

/// `t` in the engine's syntax, each leaf written by `leaf`.
fn render(t: &Ty, leaf: &dyn Fn(&str) -> String) -> String {
    match t {
        Ty::Array(e) => format!("array({})", render(e, leaf)),
        Ty::Map(k, v) => format!("map({}, {})", render(k, leaf), render(v, leaf)),
        Ty::Row(fields) => {
            let f: Vec<String> = fields
                .iter()
                .map(|(n, ty)| match n {
                    Some(n) => format!("{} {}", quote_ident(Quote::Double, n), render(ty, leaf)),
                    None => render(ty, leaf),
                })
                .collect();
            format!("row({})", f.join(", "))
        }
        Ty::Leaf(l) => leaf(l),
    }
}

fn any_leaf(t: &Ty, pred: &dyn Fn(&str) -> bool) -> bool {
    match t {
        Ty::Array(e) => any_leaf(e, pred),
        Ty::Map(k, v) => any_leaf(k, pred) || any_leaf(v, pred),
        Ty::Row(fields) => fields.iter().any(|(_, t)| any_leaf(t, pred)),
        Ty::Leaf(l) => pred(l),
    }
}

/// A scalar catalog type in the engine's syntax.
fn engine_leaf(ty: &str) -> String {
    let l = ty.trim().to_ascii_lowercase();
    match l.as_str() {
        "string" => "varchar".into(),
        "int" => "integer".into(),
        "float" => "real".into(),
        "binary" => "varbinary".into(),
        // Iceberg keeps microseconds; a bare `timestamp` in Trino is timestamp(3).
        "timestamp" => "timestamp(6)".into(),
        "timestamptz" | "timestamp with time zone" => "timestamp(6) with time zone".into(),
        _ => l,
    }
}

/// A catalog type in the engine's syntax, for `CAST`s: `string` →
/// `varchar`, `struct<a:int>` → `row("a" integer)`, `array<…>` →
/// `array(…)`, `timestamp` → `timestamp(6)`. Types already in the engine's
/// syntax keep their meaning.
pub(crate) fn engine_type(ty: &str) -> String {
    render(&parse_ty(ty), &engine_leaf)
}

/// `ty` with the leaves a cast from JSON can't produce (date, time,
/// timestamps, uuid, char…) as `varchar`: nested values are cast from JSON
/// to this first, then to the column's type.
fn json_stage_type(ty: &str) -> String {
    render(&parse_ty(ty), &|l| {
        let e = engine_leaf(l);
        match kind(&e) {
            Kind::Bool | Kind::Int | Kind::Real | Kind::Double | Kind::Decimal | Kind::Json => e,
            Kind::Char if e.starts_with("varchar") => e,
            _ => "varchar".into(),
        }
    })
}

/// A nested type with VARBINARY somewhere inside (`struct<binary_flag:string>`
/// has none).
pub(crate) fn binary_inside(ty: &str) -> bool {
    kind(ty) == Kind::Nested && any_leaf(&parse_ty(ty), &|l| kind(l) == Kind::Binary)
}

fn nested_binary(name: &str, ty: &str, what: &str) -> Error {
    Error::Unsupported(format!(
        "La columna «{name}» ({ty}) tiene valores binarios dentro de un tipo anidado: Athena no puede {what} en JSON, \
         y pasarlos como texto los alteraría. Dejá esa columna fuera de la copia."
    ))
}

/// A zoned time / timestamp leaf.
fn tz_leaf(l: &str) -> bool {
    kind(l) == Kind::TimestampTz || l.trim().to_ascii_lowercase().ends_with("with time zone")
}

/// The SELECT expression that reads column `name` of type `ty` without loss.
pub(crate) fn read_expr(name: &str, ty: &str) -> Result<String> {
    let c = quote_ident(Quote::Double, name);
    Ok(match kind(ty) {
        Kind::Real | Kind::Double => format!("CAST({c} AS VARCHAR)"),
        Kind::TimestampTz => format!("to_iso8601({c})"),
        Kind::Binary => format!("to_hex({c})"),
        // JSON can't hold VARBINARY (the cast fails), and the engine's own
        // printing (`{a=…}`) isn't JSON.
        Kind::Nested if binary_inside(ty) => return Err(nested_binary(name, ty, "convertirlos")),
        // The engine can't cast a zoned timestamp to JSON: those leaves go
        // to text first (`2024-01-02 03:04:05.123456 UTC`, zone kept whole).
        Kind::Nested if any_leaf(&parse_ty(ty), &tz_leaf) => {
            let staged = render(&parse_ty(ty), &|l| if tz_leaf(l) { "varchar".into() } else { engine_leaf(l) });
            format!("json_format(CAST(CAST({c} AS {staged}) AS JSON))")
        }
        Kind::Nested => format!("json_format(CAST({c} AS JSON))"),
        _ => c,
    })
}

fn float(s: &str) -> f64 {
    match s.trim() {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        other => other.parse().unwrap_or(f64::NAN),
    }
}

/// A REAL as the engine prints it (the shortest text of the f32), widened.
fn float32(s: &str) -> f64 {
    match s.trim() {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        other => other.parse::<f32>().map_or(f64::NAN, f64::from),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s: String = s.split_whitespace().collect();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// `2024-01-02T03:04:05.123-05:00` / `…Z` as `2024-01-02 03:04:05.123-05:00`.
fn iso_to_tz(s: &str) -> String {
    let s = s.replacen('T', " ", 1);
    match s.strip_suffix('Z') {
        Some(l) => format!("{l}+00:00"),
        None => s,
    }
}

/// A value of the answer (read through [`read_expr`], always text) as a
/// cell, given its column's type.
pub(crate) fn to_cell(v: Option<String>, ty: &str) -> Cell {
    let Some(s) = v else { return Cell::Null };
    match kind(ty) {
        Kind::Bool => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Cell::Bool(true),
            "false" => Cell::Bool(false),
            _ => Cell::Text(s),
        },
        Kind::Int => match s.trim().parse::<i64>() {
            Ok(i) => Cell::Int(i),
            Err(_) => Cell::Text(s),
        },
        Kind::Real => Cell::Float(float32(&s)),
        Kind::Double => Cell::Float(float(&s)),
        Kind::Decimal => Cell::Decimal(s),
        Kind::Binary => match unhex(&s) {
            Some(b) => Cell::Bytes(b),
            None => Cell::Text(s),
        },
        Kind::Date => Cell::Date(s),
        Kind::Time => Cell::Time(s),
        Kind::Timestamp => Cell::DateTime(s.replacen('T', " ", 1)),
        Kind::TimestampTz => Cell::DateTimeTz(iso_to_tz(&s)),
        Kind::Uuid => Cell::Uuid(s),
        Kind::Json | Kind::Nested => Cell::Json(s),
        Kind::Char | Kind::Other => Cell::Text(s),
    }
}

/// A string literal.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// `[-]digits[.digits]`.
fn plain_number(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    !(i.is_empty() && f.is_empty()) && i.bytes().all(|b| b.is_ascii_digit()) && f.bytes().all(|b| b.is_ascii_digit())
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
        Cell::DateTime(s) | Cell::DateTimeTz(s) => s.replacen('T', " ", 1),
    })
}

fn cast(c: &Cell, ty: &str) -> String {
    let ty = engine_type(ty);
    match c {
        Cell::Bytes(b) => format!("CAST(X'{}' AS {ty})", hex(b)),
        _ => format!("CAST({} AS {ty})", lit(&as_text(c).unwrap_or_default())),
    }
}

/// A cell as a JSON document: JSON cells and texts that are documents as
/// they are, everything else as its JSON value.
fn json_text(c: &Cell) -> String {
    match c {
        Cell::Json(s) => s.clone(),
        Cell::Text(s) if is_json(s) => s.clone(),
        other => other.to_json().to_string(),
    }
}

/// The fractional seconds of a time or date-time text (`05.123456+01:00`
/// → `123456`).
fn fraction(s: &str) -> &str {
    let Some(colon) = s.find(':') else { return "" };
    let rest = &s[colon..];
    let Some(dot) = rest.find('.') else { return "" };
    let f = &rest[dot + 1..];
    &f[..f.find(|c: char| !c.is_ascii_digit()).unwrap_or(f.len())]
}

/// The fractional-second digits a time / timestamp type (engine syntax)
/// keeps: its `(p)`, or 3 for a bare `time` / `timestamp`.
fn precision(engine: &str) -> usize {
    engine
        .split_once('(')
        .and_then(|(_, r)| r.split_once(')'))
        .and_then(|(p, _)| p.trim().parse().ok())
        .unwrap_or(3)
}

/// Fails when `s` has more fractional-second digits than the column keeps
/// (Iceberg's timestamps keep microseconds): the cast would round them away.
fn fits_micros(s: &str, ty: &str) -> Result<()> {
    let p = precision(&engine_leaf(ty));
    if fraction(s).bytes().skip(p).any(|b| b != b'0') {
        return Err(Error::Query(format!(
            "El valor «{s}» tiene más de {p} decimales de segundo y el tipo es {ty}: se perdería precisión al cargarlo."
        )));
    }
    Ok(())
}

/// Where the zone of a date-time text starts (`+01:00`, `Z`, ` UTC`…).
fn zone_start(s: &str) -> Option<usize> {
    let t = s.get(11..)?;
    t.find(|c: char| c == '+' || c == '-' || c == ' ' || c.is_ascii_alphabetic()).map(|i| i + 11)
}

/// A date / time text inside a nested value, as the engine's cast from
/// VARCHAR takes it for leaf type `leaf` (a catalog type): `T` → space;
/// into a zoned timestamp, `Z` → ` UTC` and no zone → UTC (never the
/// session's zone). A value the cast would round, or whose zone it would
/// drop without a word (into a zoneless timestamp), is an error.
fn nested_time(s: &str, leaf: &str) -> Result<String> {
    let engine = engine_leaf(leaf);
    let mut v = s.trim().to_string();
    if v.len() > 10 && v.as_bytes()[10] == b'T' {
        v.replace_range(10..11, " ");
    }
    match kind(&engine) {
        Kind::Timestamp if zone_start(&v).is_some() => {
            return Err(Error::Query(format!(
                "El valor «{s}» tiene zona horaria y el campo anidado es {leaf}, sin zona: Athena la descartaría sin avisar. \
                 Pasalo a hora UTC sin zona antes de cargarlo."
            )))
        }
        Kind::TimestampTz => match zone_start(&v) {
            Some(i) if v[i..].eq_ignore_ascii_case("z") => {
                v.truncate(i);
                v.push_str(" UTC");
            }
            Some(_) => {}
            None if v.len() > 10 => v.push_str(" UTC"),
            None => v.push_str(" 00:00:00 UTC"),
        },
        _ => {}
    }
    fits_micros(&v, leaf)?;
    Ok(v)
}

/// A leaf that goes JSON → VARCHAR → its type and needs [`nested_time`].
fn time_leaf(l: &str) -> bool {
    matches!(kind(l), Kind::Time | Kind::Timestamp | Kind::TimestampTz)
}

/// A number as text (`-12.50`, `1e2`, a JSON number or a plain numeric
/// string) as its integer digits (no leading zeros) and fraction digits
/// (no trailing zeros), the exponent applied. The sign is dropped.
fn exact_number(s: &str) -> Option<(String, String)> {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) || exp.abs() > 400 {
        return None;
    }
    let digits = format!("{int}{frac}");
    // Where the point goes in `digits`, which may fall outside it.
    let point = int.len() as i64 + i64::from(exp);
    let (i, f) = if point <= 0 {
        (String::new(), format!("{}{digits}", "0".repeat(point.unsigned_abs() as usize)))
    } else if point as usize >= digits.len() {
        (format!("{digits}{}", "0".repeat(point as usize - digits.len())), String::new())
    } else {
        (digits[..point as usize].to_string(), digits[point as usize..].to_string())
    };
    Some((i.trim_start_matches('0').to_string(), f.trim_end_matches('0').to_string()))
}

/// The `(p, s)` of `decimal(p, s)`; a bare `decimal` is `(38, 0)` in the engine.
fn decimal_ps(engine: &str) -> (usize, usize) {
    let inner = engine.split_once('(').and_then(|(_, r)| r.split_once(')')).map(|(i, _)| i);
    let mut it = inner.unwrap_or("").split(',').map(|x| x.trim().parse::<usize>().ok());
    match (it.next().flatten(), it.next().flatten()) {
        (Some(p), Some(s)) => (p, s),
        (Some(p), None) => (p, 0),
        _ => (38, 0),
    }
}

/// Checks one scalar JSON value (`raw`, the token as written; `string`,
/// its text when it's a string) against the nested leaf type `leaf`. The
/// engine's cast from JSON rounds and coerces without a word (`1.5` into
/// an integer is 2, `1.005` into decimal(10,2) is 1.01, `true` into a
/// number is 1, a number into varchar is rewritten as `1.25E1`, a longer
/// text into char(n) is cut): those values are errors here, like at the
/// top level.
fn check_leaf(raw: &str, string: Option<&str>, leaf: &str) -> Result<()> {
    if raw == "null" {
        return Ok(());
    }
    let engine = engine_leaf(leaf);
    let bad = |why: String| {
        let preview: String = raw.chars().take(60).collect();
        Err(Error::Query(format!(
            "El valor {preview} de un campo anidado de tipo {leaf} {why}: Athena lo cambiaría sin avisar al cargarlo. \
             Corregilo en el origen o elegí otro tipo para ese campo."
        )))
    };
    let is_number = raw.starts_with(|c: char| c == '-' || c.is_ascii_digit());
    // The numeric text the engine would read: a number, or a string's text.
    let numeric = || if is_number { Some(raw) } else { string.map(str::trim) };
    match kind(&engine) {
        Kind::Int => {
            let Some((int, frac)) = numeric().and_then(exact_number) else {
                return bad("no es un número entero".into());
            };
            if !frac.is_empty() {
                return bad("tiene decimales y el campo es entero (se redondearía)".into());
            }
            // A JSON number written with a point or an exponent reaches the
            // integer through a double: past 2^53 its last digits change
            // (`12345678901234567.0` loads as 12345678901234568).
            if is_number && raw.contains(['.', 'e', 'E']) && (int.len() > 16 || int.parse::<u64>().is_ok_and(|v| v > 1 << 53)) {
                return bad("está escrito con decimales o exponente y pasa de 2^53 (pasaría por un double y cambiarían sus últimos dígitos); escribilo como entero".into());
            }
            let neg = numeric().is_some_and(|n| n.starts_with('-'));
            let (lo, hi): (i128, i128) = match engine.as_str() {
                "tinyint" => (i8::MIN.into(), i8::MAX.into()),
                "smallint" => (i16::MIN.into(), i16::MAX.into()),
                "integer" => (i32::MIN.into(), i32::MAX.into()),
                _ => (i64::MIN.into(), i64::MAX.into()),
            };
            let v = if int.is_empty() { Some(0) } else if int.len() > 20 { None } else { int.parse::<i128>().ok() };
            match v.map(|v| if neg { -v } else { v }) {
                Some(v) if (lo..=hi).contains(&v) => Ok(()),
                _ => bad(format!("no entra en {engine}")),
            }
        }
        Kind::Decimal => {
            let Some((int, frac)) = numeric().and_then(exact_number) else {
                return bad("no es un número".into());
            };
            let (p, s) = decimal_ps(&engine);
            if frac.len() > s {
                return bad(format!("tiene más de {s} decimales (se redondearía)"));
            }
            if int.len() > p.saturating_sub(s) {
                return bad(format!("tiene más de {} dígitos enteros", p.saturating_sub(s)));
            }
            Ok(())
        }
        Kind::Real | Kind::Double => {
            let Some(n) = numeric() else { return bad("no es un número".into()) };
            if matches!(n, "NaN" | "Infinity" | "-Infinity") {
                return Ok(());
            }
            // REAL is an f32: what overflows it becomes Infinity and what is
            // too small becomes 0 (1e39 / 1e-50); DOUBLE the same with f64.
            let (finite, zero) = if engine == "real" {
                match n.parse::<f32>() {
                    Ok(f) => (f.is_finite(), f == 0.0),
                    Err(_) => return bad("no es un número".into()),
                }
            } else {
                match n.parse::<f64>() {
                    Ok(f) => (f.is_finite(), f == 0.0),
                    Err(_) => return bad("no es un número".into()),
                }
            };
            let nonzero = exact_number(n).is_some_and(|(i, f)| !i.is_empty() || !f.is_empty());
            if !finite {
                return bad(format!("no entra en {engine} (se cargaría como Infinity)"));
            }
            if zero && nonzero {
                return bad(format!("es demasiado chico para {engine} (se cargaría como 0)"));
            }
            Ok(())
        }
        Kind::Bool => match (raw, string.map(|s| s.trim().to_ascii_lowercase())) {
            ("true" | "false", _) => Ok(()),
            (_, Some(s)) if s == "true" || s == "false" => Ok(()),
            _ => bad("no es true ni false".into()),
        },
        Kind::Char => {
            let Some(text) = string else { return bad("no es texto (se reescribiría)".into()) };
            let Some(n) = char_len(&engine) else { return Ok(()) };
            let counted = if engine.starts_with("char") { text.trim_end_matches(' ') } else { text };
            let len = counted.chars().count();
            if len > n {
                return bad(format!("tiene {len} caracteres (se recortaría)"));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Copies a JSON document token by token, following its type: strings at
/// time / timestamp leaves go through [`nested_time`], every other scalar
/// is checked by [`check_leaf`] and copied as it is (numbers keep their
/// exact digits).
struct NestedJson<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    out: String,
}

impl NestedJson<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// The end of the value at `i` (the document is known to be valid JSON).
    fn value_end(&self) -> usize {
        let mut j = self.i;
        let (mut depth, mut in_str) = (0i32, false);
        while j < self.s.len() {
            let b = self.s[j];
            if in_str {
                match b {
                    b'\\' => j += 1,
                    b'"' => {
                        in_str = false;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
            } else {
                match b {
                    b'"' => in_str = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' if depth == 0 => return j,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    b',' | b' ' | b'\t' | b'\n' | b'\r' if depth == 0 => return j,
                    _ => {}
                }
            }
            j += 1;
        }
        j
    }

    fn copy(&mut self) {
        let end = self.value_end();
        self.out.push_str(&self.src[self.i..end]);
        self.i = end;
    }

    fn string(&mut self) -> Result<String> {
        let end = self.value_end();
        let s = serde_json::from_str(&self.src[self.i..end]).map_err(|e| Error::Query(e.to_string()))?;
        self.i = end;
        Ok(s)
    }

    /// `[…]` / `{…}`, each item written by `item` (its position given).
    fn items(&mut self, close: u8, mut item: impl FnMut(&mut Self, usize) -> Result<()>) -> Result<()> {
        self.out.push(self.s[self.i] as char);
        self.i += 1;
        let mut n = 0;
        loop {
            self.ws();
            match self.peek() {
                None => return Ok(()),
                Some(b) if b == close => {
                    self.out.push(close as char);
                    self.i += 1;
                    return Ok(());
                }
                Some(b',') => {
                    self.out.push(',');
                    self.i += 1;
                }
                Some(_) => {
                    item(self, n)?;
                    n += 1;
                }
            }
        }
    }

    /// `"key": value`, the value written as type `ty` (copied when `None`).
    fn member(&mut self, key: impl FnOnce(&mut Self) -> Result<Option<Ty>>) -> Result<()> {
        let ty = key(self)?;
        self.ws();
        if self.peek() == Some(b':') {
            self.out.push(':');
            self.i += 1;
        }
        match ty {
            Some(t) => self.value(&t),
            None => {
                self.ws();
                self.copy();
                Ok(())
            }
        }
    }

    fn value(&mut self, ty: &Ty) -> Result<()> {
        self.ws();
        match (ty, self.peek()) {
            (Ty::Leaf(l), Some(b'"')) if time_leaf(l) => {
                let s = self.string()?;
                let fixed = nested_time(&s, l)?;
                self.out.push_str(&serde_json::Value::String(fixed).to_string());
                Ok(())
            }
            (Ty::Leaf(l), Some(_)) if kind(&engine_leaf(l)) == Kind::Json => {
                let end = self.value_end();
                check_json_leaf(&self.src[self.i..end], l)?;
                self.copy();
                Ok(())
            }
            (Ty::Leaf(l), Some(b)) if b != b'[' && b != b'{' => {
                let end = self.value_end();
                let raw = &self.src[self.i..end];
                let string = if b == b'"' { Some(serde_json::from_str::<String>(raw).map_err(|e| Error::Query(e.to_string()))?) } else { None };
                check_leaf(raw, string.as_deref(), l)?;
                self.copy();
                Ok(())
            }
            (Ty::Array(e), Some(b'[')) => self.items(b']', |w, _| w.value(e)),
            // A JSON array cast to a ROW fills its fields by position.
            (Ty::Row(fields), Some(b'[')) => self.items(b']', |w, n| match fields.get(n) {
                Some((_, t)) => w.value(t),
                None => {
                    w.copy();
                    Ok(())
                }
            }),
            (Ty::Map(k, v), Some(b'{')) => self.items(b'}', |w, _| {
                w.member(|w| {
                    w.value(k)?;
                    Ok(Some((**v).clone()))
                })
            }),
            // …and a JSON object by name, ignoring case.
            (Ty::Row(fields), Some(b'{')) => {
                let mut seen = vec![false; fields.len()];
                self.items(b'}', |w, _| {
                    w.member(|w| {
                        let raw_start = w.i;
                        let name = w.string()?;
                        w.out.push_str(&w.src[raw_start..w.i]);
                        // The engine's cast drops a key that names no field
                        // without a word, so that is an error here.
                        match fields.iter().position(|(n, _)| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&name))) {
                            Some(f) if seen[f] => Err(Error::Query(format!(
                                "El campo \"{}\" aparece más de una vez (sin distinguir mayúsculas) en un valor anidado: \
                                 Athena no puede cargarlo sin descartar uno de los valores. Corregilo en el origen.",
                                name.chars().take(60).collect::<String>()
                            ))),
                            Some(f) => {
                                seen[f] = true;
                                Ok(Some(fields[f].1.clone()))
                            }
                            None => {
                                let names: Vec<&str> = fields.iter().filter_map(|(n, _)| n.as_deref()).collect();
                                let preview: String = name.chars().take(60).collect();
                                Err(Error::Query(format!(
                                    "El campo \"{preview}\" de un valor anidado no existe en el tipo de destino (campos: {}): \
                                     Athena lo descartaría sin avisar al cargarlo. Corregilo en el origen o agregá ese campo al tipo.",
                                    names.join(", ")
                                )))
                            }
                        }
                    })
                })
            }
            _ => {
                self.copy();
                Ok(())
            }
        }
    }
}

fn duplicate_key_error(key: &str) -> Error {
    let preview: String = key.chars().take(60).collect();
    Error::Query(format!(
        "La clave \"{preview}\" aparece más de una vez en el mismo objeto de un valor JSON: \
         Athena se quedaría solo con el último valor y descartaría los demás sin avisar. \
         Corregilo en el origen antes de cargarlo."
    ))
}

/// The spans of the number-like runs of `s` outside its strings (a run
/// starts at `-` or a digit and goes on over `-+.eE` and digits).
fn number_runs(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                runs.push((start, i));
            }
            _ => i += 1,
        }
    }
    runs
}

/// Whether `t` is a JSON number token.
fn is_number_token(t: &[u8]) -> bool {
    let mut i = usize::from(t.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < t.len() && t[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    match t.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(&mut i);
        }
        _ => return false,
    }
    if t.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(t.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(t.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == t.len()
}

/// Whether `s` is a JSON document. `serde_json` (without
/// `arbitrary_precision`) refuses a number past f64 (`1e400`) although
/// that is valid JSON and the engine takes it (as Infinity, or kept in a
/// JSON value): every well-formed number token is replaced by `0` before
/// `serde_json` checks the rest.
fn is_json(s: &str) -> bool {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, z) in number_runs(s) {
        out.push_str(&s[last..a]);
        out.push_str(if is_number_token(&s.as_bytes()[a..z]) { "0" } else { &s[a..z] });
        last = z;
    }
    out.push_str(&s[last..]);
    serde_json::from_str::<serde_json::Value>(&out).is_ok()
}

/// A JSON value nested at a `json` leaf (`raw`, valid JSON) is re-read by
/// the engine with its decimal numbers as doubles: `1e400` becomes
/// "Infinity", `1e-400` 0.0 and `123456789012345678.5`
/// 1.2345678901234568E17, `-0.0` 0.0 (integers stay exact, and a number whose double
/// reads back as the same value, like `0.10` or `1E2`, keeps its value).
/// An error for a number whose value would change.
fn check_json_leaf(raw: &str, leaf: &str) -> Result<()> {
    for (a, z) in number_runs(raw) {
        let tok = &raw[a..z];
        if !tok.contains(['.', 'e', 'E']) {
            continue;
        }
        // A subnormal double is printed back by the engine with other digits
        // (5e-324 comes back as 4.9E-324): never exact, so refused.
        let kept = tok.parse::<f64>().ok().filter(|f| f.is_finite() && (*f == 0.0 || f.is_normal())).and_then(|f| {
            let (want, got) = (exact_number(tok)?, exact_number(&f.abs().to_string())?);
            // A negative zero comes back as 0.0: its sign is lost too.
            (want == got && tok.starts_with('-') == (f < 0.0)).then_some(())
        });
        if kept.is_none() {
            let preview: String = tok.chars().take(60).collect();
            let shown = match tok.parse::<f64>() {
                Ok(f) if f.is_infinite() => if f > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() },
                Ok(f) => format!("{f:e}"),
                Err(_) => "otro número".to_string(),
            };
            return Err(Error::Query(format!(
                "El número {preview} de un campo anidado de tipo {leaf} no se puede guardar exacto: Athena lo leería como un double \
                 y lo cargaría como {shown} sin avisar. Corregilo en el origen o guardalo como texto."
            )));
        }
    }
    Ok(())
}

/// An error when an object anywhere in `json` (valid JSON) repeats a key
/// with the same spelling: the engine's JSON parser keeps only the last
/// value of those without a word.
fn no_duplicate_keys(json: &str) -> Result<()> {
    let s = json.as_bytes();
    // One entry per open container: the keys seen so far for an object.
    let mut stack: Vec<Option<std::collections::HashSet<String>>> = Vec::new();
    let mut want_key = false;
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'"' => {
                let mut j = i + 1;
                while j < s.len() && s[j] != b'"' {
                    j += if s[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(s.len());
                if want_key {
                    let key: String = serde_json::from_str(&json[i..end]).map_err(|e| Error::Query(e.to_string()))?;
                    if let Some(Some(keys)) = stack.last_mut() {
                        if !keys.insert(key.clone()) {
                            return Err(duplicate_key_error(&key));
                        }
                    }
                    want_key = false;
                }
                i = end;
                continue;
            }
            b'{' => {
                stack.push(Some(Default::default()));
                want_key = true;
            }
            b'[' => {
                stack.push(None);
                want_key = false;
            }
            b'}' | b']' => {
                stack.pop();
                want_key = false;
            }
            b',' => want_key = matches!(stack.last(), Some(Some(_))),
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// `json` (a document for nested type `ty`) with its date / time leaves
/// fixed by [`nested_time`] and its other leaves checked by
/// [`check_leaf`]; as it is when it isn't valid JSON (the engine's cast
/// reports that).
fn nested_json(json: String, ty: &str) -> Result<String> {
    let t = parse_ty(ty);
    if !is_json(&json) {
        return Ok(json);
    }
    no_duplicate_keys(&json)?;
    let mut w = NestedJson { s: json.as_bytes(), src: &json, i: 0, out: String::with_capacity(json.len()) };
    w.value(&t)?;
    Ok(w.out)
}

/// The `n` of `char(n)` / `varchar(n)`.
fn char_len(ty: &str) -> Option<usize> {
    let l = ty.trim().to_ascii_lowercase();
    let inner = l.strip_prefix("varchar(").or_else(|| l.strip_prefix("char("))?.strip_suffix(')')?;
    inner.trim().parse().ok()
}

/// A cell as a literal of the target column's type `ty` (a catalog type).
/// Every literal whose own type isn't the column's goes through a `CAST`,
/// and a value the column would cut or round is an error.
pub(crate) fn literal(c: &Cell, ty: &str) -> Result<String> {
    if matches!(c, Cell::Null) {
        return Ok("NULL".into());
    }
    let engine = engine_type(ty);
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
            let n = match c {
                Cell::Int(i) => Some(i.to_string()),
                Cell::UInt(u) => Some(u.to_string()),
                Cell::Bool(b) => Some(u8::from(*b).to_string()),
                Cell::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 => Some((*f as i64).to_string()),
                Cell::Text(s) | Cell::Decimal(s) if plain_integer(s.trim()) => Some(s.trim().trim_start_matches('+').to_string()),
                _ => None,
            };
            match n {
                // An integer literal is INTEGER or BIGINT by its size: cast
                // it to the column's width (out of range fails, never wraps).
                Some(n) if engine == "bigint" || engine.is_empty() => n,
                Some(n) => format!("CAST({n} AS {engine})"),
                None => cast(c, ty),
            }
        }
        Kind::Real | Kind::Double => {
            let v = match c {
                Cell::Float(f) if f.is_nan() => "nan()".into(),
                Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "infinity()" } else { "-infinity()" }.into(),
                // `{:e}` is the shortest form that reads back bit for bit.
                Cell::Float(f) => format!("{f:e}"),
                Cell::Int(i) => i.to_string(),
                Cell::UInt(u) => u.to_string(),
                Cell::Bool(b) => u8::from(*b).to_string(),
                _ => format!("DOUBLE {}", lit(as_text(c).unwrap_or_default().trim())),
            };
            // Those are DOUBLE (or integer) literals: a REAL column gets them cast.
            if kind(ty) == Kind::Real {
                format!("CAST({v} AS real)")
            } else {
                v
            }
        }
        Kind::Decimal => match c {
            Cell::Int(i) => format!("DECIMAL '{i}'"),
            Cell::UInt(u) => format!("DECIMAL '{u}'"),
            Cell::Float(f) if f.is_finite() => format!("DECIMAL '{f}'"),
            Cell::Decimal(s) | Cell::Text(s) if plain_number(s.trim()) => format!("DECIMAL '{}'", s.trim().trim_start_matches('+')),
            _ => cast(c, ty),
        },
        Kind::Char => {
            // Bytes that aren't UTF-8 would come in with U+FFFD in place of
            // the bad ones (`from_utf8`): refused, never altered.
            if let Cell::Bytes(b) = c {
                if std::str::from_utf8(b).is_err() {
                    return Err(Error::Query(format!(
                        "Un valor binario ({} bytes) no es texto UTF-8 válido y la columna es {ty}: cargarlo como texto \
                         cambiaría los bytes que no son UTF-8. Usá una columna binary como destino.",
                        b.len()
                    )));
                }
            }
            let text = as_text(c).unwrap_or_default();
            let v = lit(&text);
            match char_len(ty) {
                // CHAR / VARCHAR(n) casts cut longer text silently: check first.
                // Only CHAR's trailing spaces are padding; VARCHAR(n) cuts them too.
                Some(n) => {
                    let is_char = ty.trim().to_ascii_lowercase().starts_with("char");
                    let counted = if is_char { text.trim_end_matches(' ') } else { text.as_str() };
                    let len = counted.chars().count();
                    if len > n {
                        let preview: String = text.chars().take(40).collect();
                        return Err(Error::Query(format!(
                            "El valor «{preview}» tiene {len} caracteres y la columna es {ty}: no entra sin recortarlo."
                        )));
                    }
                    format!("CAST({v} AS {engine})")
                }
                None => v,
            }
        }
        Kind::Binary => match c {
            Cell::Bytes(b) => format!("X'{}'", hex(b)),
            _ => format!("X'{}'", hex(as_text(c).unwrap_or_default().as_bytes())),
        },
        Kind::Date => match c {
            Cell::Date(s) | Cell::Text(s) => format!("DATE {}", lit(s.trim())),
            Cell::DateTime(s) => format!("CAST(TIMESTAMP {} AS date)", lit(&s.replacen('T', " ", 1))),
            _ => cast(c, ty),
        },
        Kind::Time => match c {
            Cell::Time(s) | Cell::Text(s) => format!("TIME {}", lit(s.trim())),
            // A bare `time` is time(3): keep the literal's own precision.
            Cell::DateTime(s) | Cell::DateTimeTz(s) => {
                format!("CAST(TIMESTAMP {} AS time({}))", lit(&s.replacen('T', " ", 1)), fraction(s).len().min(12))
            }
            _ => cast(c, ty),
        },
        Kind::Timestamp => match c {
            Cell::DateTime(s) | Cell::Date(s) | Cell::Text(s) => {
                fits_micros(s, ty)?;
                format!("TIMESTAMP {}", lit(&s.trim().replacen('T', " ", 1)))
            }
            // An instant into a zoneless column: its UTC wall time, at the
            // literal's own precision (a bare `timestamp` is timestamp(3)).
            Cell::DateTimeTz(s) => {
                fits_micros(s, ty)?;
                format!(
                    "CAST(TIMESTAMP {} AT TIME ZONE 'UTC' AS timestamp({}))",
                    lit(&s.replacen('T', " ", 1)),
                    fraction(s).len().min(12)
                )
            }
            _ => cast(c, ty),
        },
        Kind::TimestampTz => match c {
            Cell::DateTimeTz(s) | Cell::Text(s) => {
                fits_micros(s, ty)?;
                format!("TIMESTAMP {}", lit(&s.trim().replacen('T', " ", 1)))
            }
            // A zoneless value is taken as UTC (never the session's zone).
            Cell::DateTime(s) => {
                fits_micros(s, ty)?;
                format!("TIMESTAMP {}", lit(&format!("{} UTC", s.replacen('T', " ", 1))))
            }
            Cell::Date(s) => format!("TIMESTAMP {}", lit(&format!("{s} 00:00:00 UTC"))),
            _ => cast(c, ty),
        },
        Kind::Uuid => match c {
            Cell::Bytes(b) if b.len() == 16 => {
                let h = hex(b).to_ascii_lowercase();
                format!("UUID '{}-{}-{}-{}-{}'", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
            }
            _ => format!("UUID {}", lit(as_text(c).unwrap_or_default().trim())),
        },
        Kind::Json => {
            let json = json_text(c);
            if is_json(&json) {
                no_duplicate_keys(&json)?;
            }
            format!("JSON {}", lit(&json))
        }
        Kind::Nested if binary_inside(ty) => return Err(nested_binary("destino", ty, "leerlos")),
        Kind::Nested => {
            // A cast from JSON makes no DATE / TIMESTAMP / TIME / UUID: those
            // come out as VARCHAR first, then cast to the column's type.
            // Their texts are checked and made castable first (no rounding,
            // `T` separators, zones).
            let json = format!("JSON {}", lit(&nested_json(json_text(c), ty)?));
            let staged = json_stage_type(ty);
            if staged == engine {
                format!("CAST({json} AS {engine})")
            } else {
                format!("CAST(CAST({json} AS {staged}) AS {engine})")
            }
        }
        Kind::Other => cast(c, ty),
    })
}

/// `INSERT INTO db.t (cols) VALUES\n` (the rows follow, comma separated).
pub(crate) fn insert_head(db: Option<&str>, table: &str, columns: &[String]) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(Quote::Double, c)).collect();
    format!("INSERT INTO {} ({}) VALUES\n", qualified_name(Quote::Double, db.filter(|s| !s.is_empty()), table), cols.join(", "))
}

/// `(v1, v2, …)` with each value typed for its column.
pub(crate) fn row_tuple(row: &[Cell], types: &[String]) -> Result<String> {
    let mut s = String::from("(");
    for (i, c) in row.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&literal(c, types.get(i).map_or("", String::as_str))?);
    }
    s.push(')');
    Ok(s)
}

/// The INSERT of `tuples` (rows already rendered by [`row_tuple`]).
pub(crate) fn insert_sql(head: &str, tuples: &[String]) -> String {
    let mut sql = String::with_capacity(head.len() + tuples.iter().map(|t| t.len() + 2).sum::<usize>());
    sql.push_str(head);
    for (i, t) in tuples.iter().enumerate() {
        if i > 0 {
            sql.push_str(",\n");
        }
        sql.push_str(t);
    }
    sql
}

/// Groups rows into INSERT statements, each one a commit window: at most
/// `commit_rows` rows and `min(commit_bytes, STMT_BYTES)` bytes of SQL text
/// (the text is never smaller than the data it carries, so it bounds that
/// too). A row that doesn't fit alone in [`STMT_BYTES`] is an error (Athena
/// would reject the statement); a window of one row always goes, however
/// small `commit_bytes` is.
pub(crate) struct Statements {
    head_len: usize,
    tuples: Vec<String>,
    /// Bytes of the pending statement's text.
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
}

impl Statements {
    pub(crate) fn new(head: &str, commit_rows: u64, commit_bytes: u64) -> Self {
        Statements {
            head_len: head.len(),
            tuples: Vec::new(),
            bytes: 0,
            max_rows: usize::try_from(commit_rows).unwrap_or(usize::MAX).max(1),
            max_bytes: usize::try_from(commit_bytes).unwrap_or(usize::MAX).min(STMT_BYTES),
        }
    }

    /// Add a row; the statements that closed come back with it.
    pub(crate) fn push(&mut self, tuple: String) -> Result<Vec<Vec<String>>> {
        if self.head_len + tuple.len() > STMT_BYTES {
            return Err(Error::Query(format!(
                "Una fila ocupa {} bytes y no entra en una sentencia INSERT: Athena limita el texto de una consulta a \
                 {STMT_BYTES} bytes. Achicá el valor o cargá esa tabla por otra vía (archivos en S3).",
                tuple.len()
            )));
        }
        let mut out = Vec::new();
        if !self.tuples.is_empty() && self.bytes + 2 + tuple.len() > self.max_bytes {
            out.extend(self.take());
        }
        self.bytes += if self.tuples.is_empty() { self.head_len + tuple.len() } else { 2 + tuple.len() };
        self.tuples.push(tuple);
        if self.tuples.len() >= self.max_rows {
            out.extend(self.take());
        }
        Ok(out)
    }

    pub(crate) fn take(&mut self) -> Option<Vec<String>> {
        if self.tuples.is_empty() {
            return None;
        }
        self.bytes = 0;
        Some(std::mem::take(&mut self.tuples))
    }
}

/// Athena's "more than 100 partitions in one INSERT" failure.
fn too_many_partitions(e: &Error) -> bool {
    matches!(e, Error::Query(m) if m.contains("TOO_MANY_OPEN_PARTITIONS") || m.contains("open writers for partitions"))
}

/// The row ranges of one statement still to run. A range that fails on the
/// open-partitions limit (an Iceberg INSERT that fails commits nothing) is
/// run again as two halves, in order; any other failure ends the load.
pub(crate) struct Windows(Vec<(usize, usize)>);

impl Windows {
    pub(crate) fn new(rows: usize) -> Self {
        Windows(vec![(0, rows)])
    }

    pub(crate) fn next(&mut self) -> Option<(usize, usize)> {
        self.0.pop()
    }

    pub(crate) fn failed(&mut self, (a, b): (usize, usize), e: Error) -> Result<()> {
        if b - a > 1 && too_many_partitions(&e) {
            let m = a + (b - a) / 2;
            self.0.push((m, b));
            self.0.push((a, m));
            return Ok(());
        }
        Err(e)
    }
}

/// Why Athena can't `INSERT INTO` this table, if it can't.
pub(crate) fn not_writable(t: &TableMetadata) -> Option<String> {
    let name = t.name();
    if t.table_type() == Some("VIRTUAL_VIEW") {
        return Some(format!("«{name}» es una vista: Athena no carga filas en vistas."));
    }
    let param = |k: &str| t.parameters().and_then(|p| p.get(k)).map(|v| v.to_ascii_lowercase()).unwrap_or_default();
    let formats = format!("{} {} {}", param("inputformat"), param("serde.serialization.lib"), param("spark.sql.sources.provider"));
    if param("table_type") == "delta" || formats.contains("delta") {
        return Some(format!("«{name}» es una tabla Delta Lake: Athena solo la lee, no puede insertar filas."));
    }
    if param("table_type") == "hudi" || formats.contains("hudi") {
        return Some(format!("«{name}» es una tabla Apache Hudi: Athena solo la lee, no puede insertar filas."));
    }
    if param("table_type") != "iceberg" {
        return Some(format!(
            "«{name}» es una tabla Hive sobre archivos en S3: si un INSERT de Athena falla a mitad de camino, los archivos \
             que ya escribió quedan en la tabla y no hay forma de deshacerlos, así que la carga no puede ser todo o nada. \
             La carga masiva en Athena solo va a tablas Iceberg (TBLPROPERTIES ('table_type' = 'ICEBERG'))."
        ));
    }
    None
}

/// What `running` holds while a transfer has no query of its own there
/// yet: the interrupter takes it like an id (its stop fails, harmlessly)
/// and the transfer sees the cancel before its first query.
pub(crate) const IDLE: &str = "dbine-transfer-idle";

/// First pause between polls of a running query.
#[cfg(not(test))]
const POLL: Duration = Duration::from_millis(200);
#[cfg(test)]
const POLL: Duration = Duration::from_millis(5);
/// First pause before retrying a call that failed (network, throttling…).
#[cfg(not(test))]
const RETRY: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY: Duration = Duration::from_millis(1);
/// Tries of such a call before giving up on it.
const TRIES: u32 = 6;
/// How long a stopped query is watched until its final state.
#[cfg(not(test))]
const SETTLE: Duration = Duration::from_secs(300);
#[cfg(test)]
const SETTLE: Duration = Duration::from_secs(3);

fn backoff(attempt: u32) -> Duration {
    RETRY * 2u32.pow(attempt.min(4))
}

/// A `ClientRequestToken` (32 to 128 characters), new for each statement.
fn request_token() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("dbine-{nanos:032x}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// A failure that may pass if the call is made again.
fn transient<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> bool {
    match e {
        SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) | SdkError::ResponseError(_) => true,
        SdkError::ServiceError(s) => {
            matches!(s.err().code(), Some("ThrottlingException" | "TooManyRequestsException" | "InternalServerException"))
        }
        _ => false,
    }
}

/// A query that succeeded.
pub(crate) struct Ran {
    id: String,
    statement_type: Option<StatementType>,
    /// A cancel came while it ran; it succeeded anyway (its rows are in).
    cancelled: bool,
}

/// The final state of a query execution.
enum Final {
    Succeeded(Option<StatementType>),
    Failed(String),
    Cancelled,
}

/// The state of `id`, if final (`None` while it runs).
async fn state(client: &Client, id: &str) -> Result<Option<Final>> {
    let out = client.get_query_execution().query_execution_id(id).send().await.map_err(err)?;
    let qe = out.query_execution();
    let status = qe.and_then(|q| q.status());
    Ok(match status.and_then(|s| s.state()) {
        Some(QueryExecutionState::Succeeded) => Some(Final::Succeeded(qe.and_then(|q| q.statement_type()).cloned())),
        Some(QueryExecutionState::Failed) => {
            Some(Final::Failed(status.and_then(|s| s.state_change_reason()).unwrap_or("la consulta falló").to_string()))
        }
        Some(QueryExecutionState::Cancelled) => Some(Final::Cancelled),
        _ => None,
    })
}

/// Stops `id` and waits for its final state (an INSERT stopped while it
/// commits may still succeed); `None` when that can't be learned in
/// [`SETTLE`].
async fn settle(client: &Client, id: &str) -> Option<Final> {
    for attempt in 0..TRIES {
        match client.stop_query_execution().query_execution_id(id).send().await {
            Ok(_) => break,
            Err(e) => {
                tracing::debug!("athena stop of {id} failed: {}", DisplayErrorContext(&e));
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
    let deadline = tokio::time::Instant::now() + SETTLE;
    let mut failures = 0;
    loop {
        match state(client, id).await {
            Ok(Some(f)) => return Some(f),
            Ok(None) => failures = 0,
            Err(_) => failures += 1,
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL.max(backoff(failures))).await;
    }
}

/// A query whose end couldn't be learned (see [`settle`]).
fn unknown(id: &str, e: &Error) -> Error {
    Error::Query(format!(
        "No se pudo confirmar cómo terminó la consulta {id} de Athena ({e}): si era un INSERT, puede haber cargado sus \
         filas. Revisá la tabla antes de reanudar la carga."
    ))
}

/// The query a [`StopOnDrop`] watches: its id once known, and whether the
/// future waiting for it is gone.
#[derive(Default)]
struct Watched {
    id: Option<String>,
    abandoned: bool,
}

/// Stops the query when the future waiting for it is dropped (an abort, a
/// timeout) and watches it to its end in the background. It's armed before
/// `StartQueryExecution` goes out: the start runs in its own task, so a
/// drop during that call (or its retries) doesn't lose the id Athena gives
/// back; the task stops that query itself when it finds the waiter gone.
struct StopOnDrop {
    client: Client,
    watched: std::sync::Arc<std::sync::Mutex<Watched>>,
}

impl StopOnDrop {
    fn new(client: &Client) -> Self {
        StopOnDrop { client: client.clone(), watched: Default::default() }
    }

    /// Nothing left to stop: the query ended while watched.
    fn disarm(&self) {
        if let Ok(mut w) = self.watched.lock() {
            w.id = None;
        }
    }
}

/// Stops `id` and watches it to its end, in the background.
fn settle_in_background(client: Client, id: String) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            if settle(&client, &id).await.is_none() {
                tracing::warn!("athena: couldn't confirm that query {id} ended after its transfer was dropped");
            }
        });
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let id = match self.watched.lock() {
            Ok(mut w) => {
                w.abandoned = true;
                w.id.take()
            }
            Err(_) => None,
        };
        if let Some(id) = id {
            settle_in_background(self.client.clone(), id);
        }
    }
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

impl AthenaSession {
    fn db_of(&self, table: &ObjectRef) -> Result<String> {
        table
            .schema()
            .map(str::to_string)
            .or_else(|| self.database.clone())
            .ok_or_else(|| Error::Query("no hay una base de datos seleccionada".into()))
    }

    async fn table_meta(&self, table: &ObjectRef) -> Result<TableMetadata> {
        let db = self.db_of(table)?;
        let resp = self
            .client
            .get_table_metadata()
            .catalog_name(&self.catalog)
            .database_name(&db)
            .table_name(&table.name)
            .send()
            .await
            .map_err(crate::err)?;
        resp.table_metadata().cloned().ok_or_else(|| Error::Query(format!("No se encontró la tabla {}.", table.name)))
    }

    /// The table's columns (partition keys last) with their catalog types,
    /// `wanted` in its order (all of them when `None`).
    fn typed_columns(meta: &TableMetadata, wanted: Option<&[String]>) -> Result<Vec<TransferColumn>> {
        let all: Vec<TransferColumn> = meta
            .columns()
            .iter()
            .chain(meta.partition_keys())
            .map(|c| TransferColumn { name: c.name().to_string(), type_name: c.r#type().unwrap_or("").to_string(), nullable: true })
            .collect();
        if all.is_empty() {
            return Err(Error::Query(format!("La tabla {} no tiene columnas.", meta.name())));
        }
        let Some(wanted) = wanted else { return Ok(all) };
        wanted
            .iter()
            .map(|w| {
                all.iter()
                    .find(|c| c.name == *w)
                    .or_else(|| all.iter().find(|c| c.name.eq_ignore_ascii_case(w)))
                    .cloned()
                    .ok_or_else(|| Error::Query(format!("La columna {w} no existe en {}.", meta.name())))
            })
            .collect()
    }

    /// Whether the interrupter took `armed` (what this transfer last left
    /// in `running`: [`IDLE`] or its last finished query) since: a cancel.
    fn cancelled(&self, armed: &str) -> bool {
        self.running.lock().map(|r| r.as_deref() != Some(armed)).unwrap_or(true)
    }

    /// Puts `to` in `running` in place of `from`, unless the interrupter took
    /// `from` meanwhile: then the slot stays empty (the cancel stands) and
    /// `false` comes back. The interrupter takes under the same lock, so no
    /// cancel is lost between a check and the next query.
    fn swap_running(&self, from: &str, to: &str) -> bool {
        match self.running.lock() {
            Ok(mut r) if r.as_deref() == Some(from) => {
                *r = Some(to.to_string());
                true
            }
            _ => false,
        }
    }

    /// Starts a query execution and waits for it to end. `armed` is what
    /// `running` holds now; the query takes its place (see
    /// [`Self::swap_running`]) and stays there when it succeeds. It never
    /// comes back while the query may still be running: a query it stops
    /// watching (a cancel during `StartQueryExecution`, `GetQueryExecution`
    /// failing, the future dropped) is stopped and watched to its end.
    async fn start_and_wait(&self, sql: &str, armed: &str) -> Result<Ran> {
        let mut ctx = QueryExecutionContext::builder().catalog(&self.catalog);
        if let Some(d) = &self.database {
            ctx = ctx.database(d);
        }
        // The token makes a retried start the same query execution, never a second one.
        let mut req = self
            .client
            .start_query_execution()
            .query_string(sql)
            .query_execution_context(ctx.build())
            .work_group(&self.workgroup)
            .client_request_token(request_token());
        if let Some(o) = &self.output {
            req = req.result_configuration(ResultConfiguration::builder().output_location(o).build());
        }
        // Armed before the start goes out: see [`StopOnDrop`].
        let guard = StopOnDrop::new(&self.client);
        let (client, watched) = (self.client.clone(), guard.watched.clone());
        let start = tokio::spawn(async move {
            let mut attempt = 0;
            let id = loop {
                match req.clone().send().await {
                    Ok(o) => break o.query_execution_id.unwrap_or_default(),
                    Err(e) if transient(&e) && attempt + 1 < TRIES => {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    // Athena may have taken the request before the network failed.
                    Err(e) if transient(&e) => {
                        return Err(Error::Query(format!(
                            "No se pudo confirmar si Athena empezó la consulta ({}): si era un INSERT, puede haber \
                             cargado sus filas. Revisá la tabla antes de reanudar la carga.",
                            err(e)
                        )))
                    }
                    Err(e) => return Err(err(e)),
                }
            };
            let abandoned = match watched.lock() {
                Ok(mut w) if !w.abandoned => {
                    w.id = Some(id.clone());
                    false
                }
                _ => true,
            };
            // Whoever waited for it is gone: nobody else would stop it.
            if abandoned {
                settle_in_background(client, id.clone());
            }
            Ok(id)
        });
        let id = start.await.map_err(|e| Error::Query(format!("No se pudo empezar la consulta en Athena: {e}")))??;
        let took = self.swap_running(armed, &id);
        let fin = if took {
            self.wait(&id).await
        } else {
            // The cancel came during the start's round trip: stop it here.
            settle(&self.client, &id).await.ok_or_else(|| unknown(&id, &Error::Cancelled))
        };
        guard.disarm();
        let cancelled = !took || self.cancelled(&id);
        match fin {
            Err(e) => Err(e),
            Ok(Final::Succeeded(statement_type)) => Ok(Ran { id, statement_type, cancelled }),
            Ok(_) if cancelled => Err(Error::Cancelled),
            Ok(Final::Failed(m)) => {
                // Back to what was armed, unless the interrupter took the query.
                self.swap_running(&id, armed);
                Err(Error::Query(m))
            }
            Ok(Final::Cancelled) => Err(Error::Cancelled),
        }
    }

    /// Polls `id` to its final state. `GetQueryExecution` failing
    /// ([`TRIES`] times in a row) stops the query and watches it to its end:
    /// an error never leaves it running behind.
    async fn wait(&self, id: &str) -> Result<Final> {
        let mut delay = POLL;
        let mut failures = 0;
        loop {
            match state(&self.client, id).await {
                Ok(Some(f)) => return Ok(f),
                Ok(None) => {
                    failures = 0;
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(POLL * 5);
                }
                Err(e) => {
                    failures += 1;
                    if failures >= TRIES {
                        // Stopped before it committed: the error as it was.
                        return match settle(&self.client, id).await {
                            Some(Final::Succeeded(t)) => Ok(Final::Succeeded(t)),
                            Some(_) => Err(e),
                            None => Err(unknown(id, &e)),
                        };
                    }
                    tokio::time::sleep(backoff(failures)).await;
                }
            }
        }
    }
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        self.set_running(Some(IDLE.into()));
        let r = self.read_inner(spec, sink).await;
        self.set_running(None);
        r
    }

    async fn read_inner(&self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let meta = self.table_meta(&spec.table).await?;
        let cols = Self::typed_columns(&meta, spec.columns.as_deref())?;
        let db = self.db_of(&spec.table)?;
        let exprs: Vec<String> = cols.iter().map(|c| read_expr(&c.name, &c.type_name)).collect::<Result<_>>()?;
        let mut sql = format!("SELECT {} FROM {}", exprs.join(", "), qualified_name(Quote::Double, Some(&db), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().filter(|f| !f.trim().is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        sink.lock().map_err(lock_err)?.begin(&cols)?;
        let types: Vec<String> = cols.iter().map(|c| c.type_name.clone()).collect();
        let Ran { id, statement_type, cancelled } = self.start_and_wait(&sql, IDLE).await?;
        if cancelled {
            return Err(Error::Cancelled);
        }
        // The header row is compared against the names the query gives back.
        let mut result_names: Vec<(String, String)> = Vec::new();
        let mut builder = BatchBuilder::new();
        let mut token: Option<String> = None;
        let mut first = true;
        loop {
            // The query finished, so stopping it does nothing: a cancel
            // shows as its id gone from `running`.
            if self.cancelled(&id) {
                return Err(Error::Cancelled);
            }
            let page = self
                .client
                .get_query_results()
                .query_execution_id(&id)
                .max_results(PAGE)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(err)?;
            if let Some(rs) = page.result_set() {
                if first {
                    result_names = rs
                        .result_set_metadata()
                        .map(|m| m.column_info().iter().map(|c| (c.name().to_string(), c.r#type().to_string())).collect())
                        .unwrap_or_default();
                }
                let mut rows = rs.rows().iter().map(row_values).peekable();
                // SELECT results repeat the column names as their first row.
                if first && statement_type == Some(StatementType::Dml) && rows.peek().is_some_and(|h| is_header(h, &result_names)) {
                    rows.next();
                }
                let mut guard = sink.lock().map_err(lock_err)?;
                for r in rows {
                    let cells = r.into_iter().enumerate().map(|(i, v)| to_cell(v, types.get(i).map_or("", String::as_str))).collect();
                    builder.push(cells, &mut *guard)?;
                }
            }
            first = false;
            match page.next_token() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        _columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        self.set_running(Some(IDLE.into()));
        let r = self.load_inner(spec, source, progress).await;
        self.set_running(None);
        r
    }

    async fn load_inner(&self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        let meta = self.table_meta(&spec.table).await?;
        if let Some(why) = not_writable(&meta) {
            return Err(Error::Unsupported(why));
        }
        let target = Self::typed_columns(&meta, Some(&spec.columns))?;
        if let Some(c) = target.iter().find(|c| binary_inside(&c.type_name)) {
            return Err(nested_binary(&c.name, &c.type_name, "leerlos"));
        }
        let types: Vec<String> = target.iter().map(|c| c.type_name.clone()).collect();
        let names: Vec<String> = target.iter().map(|c| c.name.clone()).collect();
        let db = self.db_of(&spec.table)?;
        let head = insert_head(Some(&db), &spec.table.name, &names);
        let mut stmts = Statements::new(&head, spec.commit_rows, spec.commit_bytes);
        let mut committed = 0u64;
        // What this load last left in `running`: IDLE, then the last INSERT
        // that went through.
        let mut armed = IDLE.to_string();
        loop {
            let batch = source.next().await;
            let mut ready = Vec::new();
            match &batch {
                Some(b) => {
                    for row in &b.rows {
                        if row.len() != types.len() {
                            return Err(Error::Query(format!(
                                "La fila tiene {} valores y la carga espera {} columnas.",
                                row.len(),
                                types.len()
                            )));
                        }
                        ready.extend(stmts.push(row_tuple(row, &types)?)?);
                    }
                }
                None => ready.extend(stmts.take()),
            }
            for tuples in ready {
                let mut windows = Windows::new(tuples.len());
                while let Some((a, b)) = windows.next() {
                    if self.cancelled(&armed) {
                        return Err(Error::Cancelled);
                    }
                    match self.start_and_wait(&insert_sql(&head, &tuples[a..b]), &armed).await {
                        Ok(ran) => {
                            armed = ran.id;
                            committed += (b - a) as u64;
                            progress(committed);
                            // Cancelled too late to stop it: counted, then stop.
                            if ran.cancelled {
                                return Err(Error::Cancelled);
                            }
                        }
                        // A failed statement put `armed` back in `running`
                        // (unless a cancel took it: the check above sees that).
                        Err(e) => windows.failed((a, b), e)?,
                    }
                }
            }
            if batch.is_none() {
                break;
            }
        }
        Ok(committed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn kinds_of_catalog_and_engine_types() {
        assert_eq!(kind("string"), Kind::Char);
        assert_eq!(kind("varchar(10)"), Kind::Char);
        assert_eq!(kind("char(3)"), Kind::Char);
        assert_eq!(kind("int"), Kind::Int);
        assert_eq!(kind("tinyint"), Kind::Int);
        assert_eq!(kind("float"), Kind::Real);
        assert_eq!(kind("double"), Kind::Double);
        assert_eq!(kind("decimal(38,10)"), Kind::Decimal);
        assert_eq!(kind("binary"), Kind::Binary);
        assert_eq!(kind("timestamp"), Kind::Timestamp);
        assert_eq!(kind("timestamp(6) with time zone"), Kind::TimestampTz);
        assert_eq!(kind("timestamptz"), Kind::TimestampTz);
        assert_eq!(kind("array<int>"), Kind::Nested);
        assert_eq!(kind("struct<a:int,b:string>"), Kind::Nested);
        assert_eq!(kind("map<string,int>"), Kind::Nested);
        assert_eq!(kind("row(\"a\" integer)"), Kind::Nested);
        assert_eq!(kind("ipaddress"), Kind::Other);
    }

    #[test]
    fn catalog_types_in_engine_syntax() {
        assert_eq!(engine_type("string"), "varchar");
        assert_eq!(engine_type("array<int>"), "array(integer)");
        assert_eq!(engine_type("map<string,array<binary>>"), "map(varchar, array(varbinary))");
        assert_eq!(
            engine_type("struct<Id:int,tags:array<string>,m:map<string,decimal(10,2)>>"),
            "row(\"Id\" integer, \"tags\" array(varchar), \"m\" map(varchar, decimal(10,2)))"
        );
        assert_eq!(engine_type("decimal(10,2)"), "decimal(10,2)");
        assert_eq!(engine_type("row(\"a\" integer)"), "row(\"a\" integer)");
        assert_eq!(engine_type("float"), "real");
        assert_eq!(engine_type("timestamp"), "timestamp(6)");
        assert_eq!(engine_type("array<timestamp>"), "array(timestamp(6))");
        assert_eq!(engine_type("row(\"a \"\"b\" integer, c timestamp(3) with time zone)"), "row(\"a \"\"b\" integer, \"c\" timestamp(3) with time zone)");
        assert_eq!(engine_type("row(timestamp(3) with time zone)"), "row(timestamp(3) with time zone)");
        assert_eq!(json_stage_type("map<string,struct<d:date,x:double>>"), "map(varchar, row(\"d\" varchar, \"x\" double))");
        assert!(binary_inside("map<string,array<binary>>"));
        assert!(binary_inside("row(\"b\" varbinary)"));
        assert!(!binary_inside("struct<binary_flag:string>"));
        assert!(!binary_inside("binary"));
    }

    #[test]
    fn read_expressions() {
        let re = |n: &str, ty: &str| read_expr(n, ty).unwrap();
        assert_eq!(re("d", "double"), "CAST(\"d\" AS VARCHAR)");
        assert_eq!(re("r", "float"), "CAST(\"r\" AS VARCHAR)");
        assert_eq!(re("t", "timestamp with time zone"), "to_iso8601(\"t\")");
        assert_eq!(re("b", "binary"), "to_hex(\"b\")");
        assert_eq!(re("a", "array<int>"), "json_format(CAST(\"a\" AS JSON))");
        assert_eq!(re("s", "struct<a:int>"), "json_format(CAST(\"s\" AS JSON))");
        // Binary inside a nested type is refused, not read as the engine prints it.
        assert!(matches!(read_expr("m", "map<string,binary>"), Err(Error::Unsupported(m)) if m.contains("«m»")));
        assert!(matches!(read_expr("a", "array<struct<x:binary>>"), Err(Error::Unsupported(_))));
        // …but a field *named* like binary is no binary.
        assert_eq!(re("f", "struct<binary_flag:string>"), "json_format(CAST(\"f\" AS JSON))");
        assert_eq!(re("x\"y", "bigint"), "\"x\"\"y\"");
        assert_eq!(re("n", "string"), "\"n\"");
    }

    #[test]
    fn cells_from_the_answer() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(to_cell(None, "bigint"), Cell::Null);
        assert_eq!(to_cell(s(""), "string"), Cell::Text(String::new()));
        assert_eq!(to_cell(s("9007199254740993"), "bigint"), Cell::Int(9007199254740993));
        assert_eq!(to_cell(s("-4"), "int"), Cell::Int(-4));
        assert_eq!(to_cell(s("true"), "boolean"), Cell::Bool(true));
        assert_eq!(to_cell(s("1.0E-1"), "double"), Cell::Float(0.1));
        assert_eq!(to_cell(s("-Infinity"), "float"), Cell::Float(f64::NEG_INFINITY));
        // A REAL prints as its shortest f32 text: read back as that f32, widened.
        assert_eq!(to_cell(s("0.1"), "float"), Cell::Float(f64::from(0.1f32)));
        assert_eq!(to_cell(s("0.1"), "double"), Cell::Float(0.1));
        assert!(matches!(to_cell(s("NaN"), "double"), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(s("12345678901234567890.123456789"), "decimal(38,9)"), Cell::Decimal("12345678901234567890.123456789".into()));
        assert_eq!(to_cell(s("CAFE"), "binary"), Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(to_cell(s("ca fe"), "varbinary"), Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(to_cell(s(""), "binary"), Cell::Bytes(vec![]));
        assert_eq!(to_cell(s("2024-01-31"), "date"), Cell::Date("2024-01-31".into()));
        assert_eq!(to_cell(s("2024-01-02 03:04:05.123456"), "timestamp"), Cell::DateTime("2024-01-02 03:04:05.123456".into()));
        assert_eq!(
            to_cell(s("2024-01-02T03:04:05.123-05:00"), "timestamp with time zone"),
            Cell::DateTimeTz("2024-01-02 03:04:05.123-05:00".into())
        );
        assert_eq!(to_cell(s("2024-01-02T03:04:05Z"), "timestamptz"), Cell::DateTimeTz("2024-01-02 03:04:05+00:00".into()));
        assert_eq!(to_cell(s("{\"a\":1,\"b\":[\"x\"]}"), "struct<a:int,b:array<string>>"), Cell::Json("{\"a\":1,\"b\":[\"x\"]}".into()));
        assert_eq!(to_cell(s("{\"z\":1}"), "json"), Cell::Json("{\"z\":1}".into()));
        assert_eq!(to_cell(s("ab "), "char(3)"), Cell::Text("ab ".into()));
    }

    #[test]
    fn literals_per_target_type() {
        let lt = |c: &Cell, ty: &str| literal(c, ty).unwrap();
        let t = |s: &str| Cell::Text(s.into());
        assert_eq!(lt(&Cell::Null, "int"), "NULL");
        assert_eq!(lt(&Cell::Bool(true), "boolean"), "TRUE");
        assert_eq!(lt(&Cell::Int(0), "boolean"), "FALSE");
        assert_eq!(lt(&t("quizá"), "boolean"), "CAST('quizá' AS boolean)");
        assert_eq!(lt(&Cell::Int(-5), "tinyint"), "CAST(-5 AS tinyint)");
        assert_eq!(lt(&Cell::Int(7), "int"), "CAST(7 AS integer)");
        assert_eq!(lt(&Cell::Int(7), "bigint"), "7");
        assert_eq!(lt(&t(" +42 "), "bigint"), "42");
        assert_eq!(lt(&t("4x"), "int"), "CAST('4x' AS integer)");
        assert_eq!(lt(&Cell::Float(0.1), "double"), "1e-1");
        assert_eq!(lt(&Cell::Float(f64::NAN), "float"), "CAST(nan() AS real)");
        assert_eq!(lt(&Cell::Float(0.1), "float"), "CAST(1e-1 AS real)");
        assert_eq!(lt(&Cell::Float(f64::INFINITY), "real"), "CAST(infinity() AS real)");
        assert_eq!(lt(&Cell::Float(f64::NEG_INFINITY), "double"), "-infinity()");
        assert_eq!(lt(&Cell::Decimal("1.5".into()), "double"), "DOUBLE '1.5'");
        assert_eq!(lt(&Cell::Decimal("-12.340".into()), "decimal(10,3)"), "DECIMAL '-12.340'");
        assert_eq!(lt(&Cell::UInt(u64::MAX), "decimal(20,0)"), "DECIMAL '18446744073709551615'");
        assert_eq!(lt(&t("O'Brien"), "string"), "'O''Brien'");
        assert_eq!(lt(&Cell::Int(5), "varchar(3)"), "CAST('5' AS varchar(3))");
        assert_eq!(lt(&t("ab"), "char(3)"), "CAST('ab' AS char(3))");
        assert_eq!(lt(&t("abc  "), "char(3)"), "CAST('abc  ' AS char(3))");
        // A cast would cut it silently: refused.
        assert!(matches!(literal(&t("abcd"), "varchar(3)"), Err(Error::Query(m)) if m.contains("4 caracteres")));
        assert_eq!(lt(&t(""), "string"), "''");
        // VARCHAR(n) cuts trailing spaces too: they count (CHAR's are padding).
        assert!(matches!(literal(&t("ab   "), "varchar(3)"), Err(Error::Query(m)) if m.contains("5 caracteres")));
        assert_eq!(lt(&t("ab "), "varchar(3)"), "CAST('ab ' AS varchar(3))");
        // Bytes that aren't UTF-8 into text: refused, not replaced with U+FFFD.
        assert!(matches!(literal(&Cell::Bytes(vec![0xFF, 0x41]), "string"), Err(Error::Query(m)) if m.contains("UTF-8")));
        assert!(matches!(literal(&Cell::Bytes(vec![0xFF]), "varchar(10)"), Err(Error::Query(_))));
        assert_eq!(lt(&Cell::Bytes(b"ok".to_vec()), "string"), "'ok'");
        assert_eq!(lt(&Cell::Bytes(vec![0xCA, 0xFE]), "binary"), "X'CAFE'");
        assert_eq!(lt(&Cell::Bytes(vec![]), "binary"), "X''");
        assert_eq!(lt(&t("ab"), "binary"), "X'6162'");
        assert_eq!(lt(&Cell::Date("2024-01-31".into()), "date"), "DATE '2024-01-31'");
        assert_eq!(lt(&Cell::DateTime("2024-01-31T10:00:00".into()), "date"), "CAST(TIMESTAMP '2024-01-31 10:00:00' AS date)");
        assert_eq!(lt(&Cell::Time("10:00:00.123".into()), "time"), "TIME '10:00:00.123'");
        assert_eq!(lt(&Cell::DateTime("2024-01-31T10:00:00.123456".into()), "timestamp"), "TIMESTAMP '2024-01-31 10:00:00.123456'");
        assert_eq!(
            lt(&Cell::DateTimeTz("2024-01-31 10:00:00+01:00".into()), "timestamp"),
            "CAST(TIMESTAMP '2024-01-31 10:00:00+01:00' AT TIME ZONE 'UTC' AS timestamp(0))"
        );
        // The instant keeps its microseconds (a bare `timestamp` would be timestamp(3)).
        assert_eq!(
            lt(&Cell::DateTimeTz("2024-01-31 10:00:00.123456+01:00".into()), "timestamp"),
            "CAST(TIMESTAMP '2024-01-31 10:00:00.123456+01:00' AT TIME ZONE 'UTC' AS timestamp(6))"
        );
        assert_eq!(
            lt(&Cell::DateTimeTz("2024-01-31T10:00:00.123456Z".into()), "time"),
            "CAST(TIMESTAMP '2024-01-31 10:00:00.123456Z' AS time(6))"
        );
        // Nanoseconds don't fit Iceberg's microseconds: refused, not rounded.
        assert!(matches!(literal(&Cell::DateTime("2024-01-31 10:00:00.123456789".into()), "timestamp"), Err(Error::Query(_))));
        assert!(matches!(literal(&Cell::DateTimeTz("2024-01-31 10:00:00.1234567+00:00".into()), "timestamp"), Err(Error::Query(_))));
        assert_eq!(lt(&Cell::DateTime("2024-01-31 10:00:00.123456000".into()), "timestamp"), "TIMESTAMP '2024-01-31 10:00:00.123456000'");
        assert_eq!(lt(&Cell::DateTime("2024-01-31 10:00:00".into()), "timestamptz"), "TIMESTAMP '2024-01-31 10:00:00 UTC'");
        assert_eq!(lt(&Cell::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()), "uuid"), "UUID 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'");
        assert_eq!(lt(&Cell::Bytes((0..16).collect()), "uuid"), "UUID '00010203-0405-0607-0809-0a0b0c0d0e0f'");
        assert_eq!(lt(&Cell::Json("{\"a\":1}".into()), "json"), "JSON '{\"a\":1}'");
        assert_eq!(
            lt(&Cell::Json("{\"a\":1,\"b\":\"it's\"}".into()), "struct<a:int,b:string>"),
            "CAST(JSON '{\"a\":1,\"b\":\"it''s\"}' AS row(\"a\" integer, \"b\" varchar))"
        );
        assert_eq!(lt(&Cell::Json("[1,2]".into()), "array<int>"), "CAST(JSON '[1,2]' AS array(integer))");
        // Dates and timestamps inside: cast from JSON to VARCHAR, then to the type.
        assert_eq!(
            lt(&Cell::Json("{\"d\":\"2024-01-02\",\"ts\":\"2024-01-02 03:04:05.123456\",\"n\":1}".into()), "struct<d:date,ts:timestamp,n:int>"),
            "CAST(CAST(JSON '{\"d\":\"2024-01-02\",\"ts\":\"2024-01-02 03:04:05.123456\",\"n\":1}' \
             AS row(\"d\" varchar, \"ts\" varchar, \"n\" integer)) AS row(\"d\" date, \"ts\" timestamp(6), \"n\" integer))"
        );
        assert_eq!(
            lt(&Cell::Json("[\"2024-01-02\"]".into()), "array<date>"),
            "CAST(CAST(JSON '[\"2024-01-02\"]' AS array(varchar)) AS array(date))"
        );
        assert!(matches!(literal(&Cell::Json("[\"AA\"]".into()), "array<binary>"), Err(Error::Unsupported(_))));
        // Nested timestamps: nanoseconds refused (the cast would round them),
        // the ISO `T` taken, numbers elsewhere kept digit for digit.
        let ts = "struct<ts:timestamp,n:decimal(38,3),s:string>";
        assert!(matches!(
            literal(&Cell::Json("{\"ts\":\"2024-01-02 03:04:05.123456789\"}".into()), ts),
            Err(Error::Query(m)) if m.contains("6 decimales")
        ));
        assert_eq!(
            lt(&Cell::Json("{ \"TS\" : \"2024-01-02T03:04:05.5\", \"n\": 12345678901234567890.123, \"s\": \"2024-01-02T00:00:00\"}".into()), ts),
            "CAST(CAST(JSON '{\"TS\":\"2024-01-02 03:04:05.5\",\"n\":12345678901234567890.123,\"s\":\"2024-01-02T00:00:00\"}' \
             AS row(\"ts\" varchar, \"n\" decimal(38,3), \"s\" varchar)) AS row(\"ts\" timestamp(6), \"n\" decimal(38,3), \"s\" varchar))"
        );
        assert!(lt(&Cell::Json("{\"ts\":null,\"n\":1}".into()), ts).contains("{\"ts\":null,\"n\":1}"));
        // A zone into a zoneless timestamp would be dropped: refused.
        assert!(matches!(literal(&Cell::Json("[\"2024-01-02T03:04:05Z\"]".into()), "array<timestamp>"), Err(Error::Query(m)) if m.contains("zona")));
        // Zoned: `Z` and no zone are UTC, an offset stays.
        assert!(lt(
            &Cell::Json("[\"2024-01-02T03:04:05Z\",\"2024-01-02T03:04:05\",\"2024-01-02 03:04:05.1+01:00\",\"2024-01-02\"]".into()),
            "array<timestamptz>"
        )
        .contains("[\"2024-01-02 03:04:05 UTC\",\"2024-01-02 03:04:05 UTC\",\"2024-01-02 03:04:05.1+01:00\",\"2024-01-02 00:00:00 UTC\"]"));
        // Rows as JSON arrays (by position) and maps (keys and values) too.
        assert!(lt(&Cell::Json("[[\"2024-01-02T03:04:05\", 7]]".into()), "array<struct<t:timestamp,i:int>>").contains("[[\"2024-01-02 03:04:05\",7]]"));
        assert!(lt(&Cell::Json("{\"k\":{\"x\":[\"2024-01-02T03:04:05\"]}}".into()), "map<string,map<string,array<timestamp>>>")
            .contains("{\"k\":{\"x\":[\"2024-01-02 03:04:05\"]}}"));
        assert!(matches!(literal(&Cell::Json("{\"k\":\"2024-01-02 03:04:05.0000001\"}".into()), "map<string,timestamp>"), Err(Error::Query(_))));
        assert_eq!(lt(&t("10.0.0.1"), "ipaddress"), "CAST('10.0.0.1' AS ipaddress)");
    }

    /// A local stand-in for Athena's endpoint: each call (by its
    /// `X-Amz-Target`) goes to `handler`, which answers status and body.
    type Handler = Arc<dyn Fn(&str, &serde_json::Value) -> (u16, String) + Send + Sync>;

    async fn fake_athena(handler: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let h = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            let mut tmp = [0u8; 8192];
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let header = |k: &str| head.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
                        let len: usize = header("content-length:").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let target = header("x-amz-target:").unwrap_or_default();
                        while buf.len() < end + len {
                            let mut tmp = [0u8; 8192];
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                        let body: serde_json::Value = serde_json::from_slice(&buf[end..end + len]).unwrap_or_default();
                        buf.drain(..end + len);
                        let op = target.rsplit('.').next().unwrap_or("").to_string();
                        let (status, resp) = h(&op, &body);
                        let out = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/x-amz-json-1.1\r\ncontent-length: {}\r\n\r\n{resp}",
                            resp.len()
                        );
                        if sock.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        url
    }

    fn fake_session(url: &str) -> AthenaSession {
        use aws_sdk_athena::config::{retry::RetryConfig, Credentials, Region};
        let conf = aws_sdk_athena::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .endpoint_url(url)
            .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "test"))
            .retry_config(RetryConfig::disabled())
            .http_client(crate::plain_http_client())
            .build();
        crate::tls_provider();
        AthenaSession {
            client: Client::from_conf(conf),
            region: "us-east-1".into(),
            catalog: "AwsDataCatalog".into(),
            workgroup: "primary".into(),
            output: None,
            database: Some("db".into()),
            running: Arc::new(std::sync::Mutex::new(None)),
            profiler: None,
        }
    }

    /// The calls the fake saw (lowercased operation, query id), in order.
    type Calls = Arc<std::sync::Mutex<Vec<(String, String)>>>;

    fn qe(id: &str, state: &str, reason: &str) -> (u16, String) {
        let v = serde_json::json!({"QueryExecution": {"QueryExecutionId": id, "StatementType": "DML",
            "Status": {"State": state, "StateChangeReason": reason}}});
        (200, v.to_string())
    }

    /// An Iceberg table `t (id bigint)`; `get` answers GetQueryExecution
    /// (given whether StopQueryExecution came and how many Gets came before),
    /// `on_start` runs while StartQueryExecution is in flight.
    fn handler(
        calls: Calls,
        get: impl Fn(&str, bool, usize) -> (u16, String) + Send + Sync + 'static,
        on_start: impl Fn() + Send + Sync + 'static,
    ) -> Handler {
        let started = Arc::new(AtomicU64::new(0));
        Arc::new(move |op: &str, body: &serde_json::Value| {
            let id = body["QueryExecutionId"].as_str().unwrap_or("").to_string();
            calls.lock().unwrap().push((op.to_string(), id.clone()));
            let log = calls.lock().unwrap().clone();
            let stopped = log.iter().any(|(o, i)| o == "stopqueryexecution" && *i == id);
            let gets = log.iter().filter(|(o, i)| o == "getqueryexecution" && *i == id).count();
            match op {
                "gettablemetadata" => (200, serde_json::json!({"TableMetadata": {"Name": "t", "TableType": "EXTERNAL_TABLE",
                    "Columns": [{"Name": "id", "Type": "bigint"}], "Parameters": {"table_type": "ICEBERG"}}}).to_string()),
                "startqueryexecution" => {
                    on_start();
                    let n = started.fetch_add(1, Ordering::SeqCst) + 1;
                    (200, format!("{{\"QueryExecutionId\":\"q{n}\"}}"))
                }
                "stopqueryexecution" => (200, "{}".into()),
                "getqueryexecution" => get(&id, stopped, gets),
                _ => (400, "{\"__type\":\"InvalidRequestException\",\"Message\":\"?\"}".into()),
            }
        })
    }

    fn load_spec(commit_rows: u64) -> LoadSpec {
        LoadSpec {
            table: ObjectRef { kind: dbine_driver::kinds::TABLE.into(), schema: Some("db".into()), name: "t".into() },
            columns: vec!["id".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        }
    }

    struct Rows(Option<dbine_driver::transfer::RowBatch>);

    #[dbine_driver::async_trait]
    impl BatchSource for Rows {
        async fn next(&mut self) -> Option<dbine_driver::transfer::RowBatch> {
            self.0.take()
        }
    }

    fn rows(n: i64) -> Rows {
        Rows(Some(dbine_driver::transfer::RowBatch { rows: (0..n).map(|i| vec![Cell::Int(i)]).collect(), bytes: 0 }))
    }

    async fn run_load(s: &mut AthenaSession, n: i64, commit_rows: u64) -> (Result<u64>, Vec<u64>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let progress = move |c: u64| seen2.lock().unwrap().push(c);
        let r = s.transfer_load(&load_spec(commit_rows), &[], &mut rows(n), &progress).await;
        let p = seen.lock().unwrap().clone();
        (r, p)
    }

    fn stops(calls: &Calls) -> Vec<String> {
        calls.lock().unwrap().iter().filter(|(o, _)| o == "stopqueryexecution").map(|(_, i)| i.clone()).collect()
    }

    fn last_get_after_stop(calls: &Calls, id: &str) -> bool {
        let log = calls.lock().unwrap();
        let stop = log.iter().position(|(o, i)| o == "stopqueryexecution" && i == id);
        stop.is_some_and(|p| log[p..].iter().any(|(o, i)| o == "getqueryexecution" && i == id))
    }

    #[tokio::test]
    async fn polling_errors_stop_the_insert_before_returning() {
        let calls: Calls = Default::default();
        // GetQueryExecution fails (network, throttling…) until the query is stopped.
        let get = |id: &str, stopped: bool, _| {
            if stopped {
                qe(id, "CANCELLED", "")
            } else {
                (500, "{\"__type\":\"InternalServerException\",\"Message\":\"boom\"}".into())
            }
        };
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || {})).await);
        let (r, progress) = run_load(&mut s, 3, 100).await;
        // The original error, not a cancel; nothing counted; the INSERT
        // stopped and seen in its final state before returning.
        assert!(r.is_err() && !matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert!(progress.is_empty());
        assert_eq!(stops(&calls), ["q1"]);
        assert!(last_get_after_stop(&calls, "q1"));

        // Stopped while it committed: it succeeded, so its rows count.
        let calls: Calls = Default::default();
        let get = |id: &str, stopped: bool, _| {
            if stopped {
                qe(id, "SUCCEEDED", "")
            } else {
                (500, "{\"__type\":\"InternalServerException\",\"Message\":\"boom\"}".into())
            }
        };
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || {})).await);
        let (r, progress) = run_load(&mut s, 3, 100).await;
        assert_eq!(r.unwrap(), 3);
        assert_eq!(progress, [3]);
    }

    #[tokio::test]
    async fn a_dropped_load_stops_its_insert() {
        let calls: Calls = Default::default();
        let get = |id: &str, stopped: bool, _| if stopped { qe(id, "CANCELLED", "") } else { qe(id, "RUNNING", "") };
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || {})).await);
        let spec = load_spec(100);
        let mut src = rows(3);
        let progress = |_: u64| {};
        let r = tokio::time::timeout(Duration::from_millis(150), s.transfer_load(&spec, &[], &mut src, &progress)).await;
        assert!(r.is_err(), "the load should still be polling");
        for _ in 0..200 {
            if last_get_after_stop(&calls, "q1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(stops(&calls), ["q1"]);
        assert!(last_get_after_stop(&calls, "q1"));
    }

    #[tokio::test]
    async fn a_cancel_during_the_start_stops_that_query() {
        for final_state in ["CANCELLED", "SUCCEEDED"] {
            let calls: Calls = Default::default();
            let running: Arc<std::sync::Mutex<Option<String>>> = Default::default();
            let get = move |id: &str, stopped: bool, _| if stopped { qe(id, final_state, "") } else { qe(id, "RUNNING", "") };
            // The interrupter takes what's in `running` while the start is in flight.
            let taker = running.clone();
            let url = fake_athena(handler(calls.clone(), get, move || {
                taker.lock().unwrap().take();
            }))
            .await;
            let mut s = fake_session(&url);
            s.running = running;
            let (r, progress) = run_load(&mut s, 3, 100).await;
            assert!(matches!(r, Err(Error::Cancelled)), "{final_state}: {r:?}");
            assert_eq!(stops(&calls), ["q1"]);
            assert!(last_get_after_stop(&calls, "q1"));
            // Stopped too late (it committed): counted, still a cancel.
            let expect: &[u64] = if final_state == "SUCCEEDED" { &[3] } else { &[] };
            assert_eq!(progress, expect);
        }
    }

    #[tokio::test]
    async fn a_cancel_during_a_failed_statement_is_kept() {
        let calls: Calls = Default::default();
        let running: Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let taker = running.clone();
        // The statement fails on the partitions limit, and a cancel takes it
        // from `running` meanwhile: no halves run after that.
        let get = move |id: &str, _, _| {
            taker.lock().unwrap().take();
            qe(id, "FAILED", "ICEBERG_TOO_MANY_OPEN_PARTITIONS: Exceeded limit of 100 open writers for partitions")
        };
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || {})).await);
        s.running = running;
        let (r, progress) = run_load(&mut s, 4, 100).await;
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert!(progress.is_empty());
        assert_eq!(calls.lock().unwrap().iter().filter(|(o, _)| o == "startqueryexecution").count(), 1);

        // Without the cancel, the halves run.
        let calls: Calls = Default::default();
        let get = |id: &str, _, _| {
            if id == "q1" {
                qe(id, "FAILED", "ICEBERG_TOO_MANY_OPEN_PARTITIONS: Exceeded limit of 100 open writers for partitions")
            } else {
                qe(id, "SUCCEEDED", "")
            }
        };
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || {})).await);
        let (r, progress) = run_load(&mut s, 4, 100).await;
        assert_eq!(r.unwrap(), 4);
        assert_eq!(progress, [2, 4]);
    }

    #[test]
    fn statements_stay_under_athenas_limit() {
        let types = vec!["bigint".to_string(), "string".to_string(), "decimal(10,2)".to_string()];
        let head = insert_head(Some("s"), "t", &["id".into(), "name".into(), "amount".into()]);
        assert_eq!(head, "INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"amount\") VALUES\n");
        let big = LoadSpec::DEFAULT_COMMIT_BYTES;
        let mut st = Statements::new(&head, 100_000, big);
        let row = |cells: &[Cell]| row_tuple(cells, &types).unwrap();
        assert!(st.push(row(&[Cell::Int(1), Cell::Text("a".into()), Cell::Decimal("1.50".into())])).unwrap().is_empty());
        assert!(st.push(row(&[Cell::Int(2), Cell::Null, Cell::Null])).unwrap().is_empty());
        let tuples = st.take().unwrap();
        assert_eq!(tuples.len(), 2);
        assert_eq!(insert_sql(&head, &tuples), format!("{head}(1, 'a', DECIMAL '1.50'),\n(2, NULL, NULL)"));
        assert!(st.take().is_none());

        // Split by bytes: no statement over the limit.
        let mut st = Statements::new(&head, 100_000, big);
        let tuple = format!("('{}')", "x".repeat(100_000));
        let mut out = Vec::new();
        for _ in 0..7 {
            out.extend(st.push(tuple.clone()).unwrap());
        }
        out.extend(st.take());
        assert_eq!(out.iter().map(Vec::len).sum::<usize>(), 7);
        assert!(out.iter().all(|t| insert_sql(&head, t).len() <= STMT_BYTES));
        assert_eq!(out.len(), 4);

        // A row that can't fit alone is an error, not a rejected query.
        let mut st = Statements::new(&head, 100_000, big);
        assert!(matches!(st.push(format!("('{}')", "x".repeat(STMT_BYTES))), Err(Error::Query(m)) if m.contains("262144")));
    }

    #[test]
    fn statements_honor_the_commit_windows() {
        let head = insert_head(None, "t", &["id".into()]);
        // commit_rows: narrow rows close a statement at 100, not at the byte limit.
        let mut st = Statements::new(&head, 100, LoadSpec::DEFAULT_COMMIT_BYTES);
        let mut out = Vec::new();
        for i in 0..250 {
            out.extend(st.push(format!("({i})")).unwrap());
        }
        assert_eq!(out.iter().map(Vec::len).collect::<Vec<_>>(), [100, 100]);
        out.extend(st.take());
        assert_eq!(out.iter().map(Vec::len).collect::<Vec<_>>(), [100, 100, 50]);
        assert_eq!(out[1][0], "(100)");

        // commit_bytes below Athena's limit bounds each statement's text.
        let mut st = Statements::new(&head, 100_000, 1_000);
        let mut out = Vec::new();
        for _ in 0..50 {
            out.extend(st.push(format!("('{}')", "y".repeat(96))).unwrap());
        }
        out.extend(st.take());
        assert_eq!(out.iter().map(Vec::len).sum::<usize>(), 50);
        assert!(out.iter().all(|t| insert_sql(&head, t).len() <= 1_000));
        // A window smaller than one row still takes that row.
        let mut st = Statements::new(&head, 100_000, 10);
        assert_eq!(st.push(format!("('{}')", "z".repeat(50))).unwrap().len(), 0);
        assert_eq!(st.push("(1)".into()).unwrap().len(), 1);
    }

    #[test]
    fn partition_limit_splits_the_statement_in_order() {
        let limit = || Error::Query("ICEBERG_TOO_MANY_OPEN_PARTITIONS: Exceeded limit of 100 open writers for partitions".into());
        // A statement of 400 rows that fails on the limit until it is 100 rows or fewer.
        let mut w = Windows::new(400);
        let mut ran = Vec::new();
        while let Some((a, b)) = w.next() {
            if b - a > 100 {
                w.failed((a, b), limit()).unwrap();
            } else {
                ran.push((a, b));
            }
        }
        assert_eq!(ran, [(0, 100), (100, 200), (200, 300), (300, 400)]);
        // Any other failure ends the load; so does the limit on a single row.
        let mut w = Windows::new(10);
        let first = w.next().unwrap();
        assert!(w.failed(first, Error::Query("TYPE_MISMATCH".into())).is_err());
        let mut w = Windows::new(1);
        let first = w.next().unwrap();
        assert!(w.failed(first, limit()).is_err());
    }

    #[test]
    fn only_writable_tables_load() {
        let hive = TableMetadata::builder()
            .name("h")
            .table_type("EXTERNAL_TABLE")
            .parameters("inputformat", "org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
            .build()
            .unwrap();
        // A failed INSERT into a Hive table can leave its files behind: refused.
        assert!(not_writable(&hive).unwrap().contains("Iceberg"));
        let ice = TableMetadata::builder().name("i").table_type("EXTERNAL_TABLE").parameters("table_type", "ICEBERG").build().unwrap();
        assert!(not_writable(&ice).is_none());
        let view = TableMetadata::builder().name("v").table_type("VIRTUAL_VIEW").build().unwrap();
        assert!(not_writable(&view).unwrap().contains("vista"));
        let delta = TableMetadata::builder().name("d").table_type("EXTERNAL_TABLE").parameters("table_type", "DELTA").build().unwrap();
        assert!(not_writable(&delta).unwrap().contains("Delta Lake"));
        let hudi = TableMetadata::builder()
            .name("u")
            .table_type("EXTERNAL_TABLE")
            .parameters("inputformat", "org.apache.hudi.hadoop.HoodieParquetInputFormat")
            .build()
            .unwrap();
        assert!(not_writable(&hudi).unwrap().contains("Hudi"));
    }

    #[test]
    fn columns_follow_the_requested_order() {
        use aws_sdk_athena::types::Column;
        let meta = TableMetadata::builder()
            .name("t")
            .columns(Column::builder().name("id").r#type("bigint").build().unwrap())
            .columns(Column::builder().name("nombre").r#type("string").build().unwrap())
            .partition_keys(Column::builder().name("dia").r#type("date").build().unwrap())
            .build()
            .unwrap();
        let all = AthenaSession::typed_columns(&meta, None).unwrap();
        assert_eq!(all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "nombre", "dia"]);
        let some = AthenaSession::typed_columns(&meta, Some(&["DIA".into(), "id".into()])).unwrap();
        assert_eq!(some.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect::<Vec<_>>(), [("dia", "date"), ("id", "bigint")]);
        assert!(AthenaSession::typed_columns(&meta, Some(&["zz".into()])).is_err());
    }

    #[test]
    fn nested_leaves_are_checked_not_coerced() {
        let j = |v: &str| Cell::Json(v.into());
        let bad = |v: &str, ty: &str| matches!(literal(&j(v), ty), Err(Error::Query(_)));
        let ty = "struct<d:decimal(10,2),i:int>";
        // The engine's cast would round these (1.01, 2) or coerce them (1).
        assert!(bad("{\"d\":1.005,\"i\":1}", ty));
        assert!(bad("{\"d\":\"1.005\",\"i\":1}", ty));
        assert!(bad("{\"d\":1.5,\"i\":1.5}", ty));
        assert!(bad("{\"d\":1,\"i\":true}", ty));
        assert!(bad("{\"d\":true,\"i\":1}", ty));
        assert!(bad("{\"d\":123456789.5,\"i\":1}", ty));
        assert!(bad("[1.005, 7]", ty));
        assert!(bad("[1.5, 7.5]", ty));
        // A number into a varchar field would be rewritten (`1.25E1`).
        assert!(bad("{\"s\":12.50}", "struct<s:string>"));
        assert!(bad("[true]", "array<varchar(5)>"));
        assert!(bad("[\"abc\"]", "array<char(2)>"));
        assert!(bad("[200]", "array<tinyint>"));
        assert!(bad("[99999999999]", "array<int>"));
        assert!(bad("[1]", "array<boolean>"));
        assert!(bad("{\"1.5\":\"x\"}", "map<int,string>"));
        // Exact values pass with their digits untouched.
        let ok = literal(&j("{\"d\":1.50,\"i\":\"12\"}"), ty).unwrap();
        assert!(ok.contains("{\"d\":1.50,\"i\":\"12\"}"), "{ok}");
        assert!(literal(&j("[1e2, -128, 1.000]"), "array<bigint>").is_ok());
        assert!(literal(&j("[-128, 127, null]"), "array<tinyint>").is_ok());
        assert!(literal(&j("[\"ab  \"]"), "array<char(2)>").is_ok());
        assert!(literal(&j("{\"1\":\"x\"}"), "map<int,string>").is_ok());
        assert!(literal(&j("[0.1, 1e-300, \"NaN\"]"), "array<double>").is_ok());
        assert!(literal(&j("[true, \"false\"]"), "array<boolean>").is_ok());
        assert!(literal(&j("[12345678901234567890123456789012345.12]"), "array<decimal(38,2)>").is_ok());
        assert!(literal(&j("[1e-2]"), "array<decimal(4,2)>").is_ok());
        assert_eq!(exact_number("-0012.500e1"), Some(("125".into(), String::new())));
        assert_eq!(exact_number("1.5e-3"), Some((String::new(), "0015".into())));
        assert_eq!(exact_number("1.2.3"), None);
    }

    /// Nested values the engine's cast would change without a word: a
    /// float-spelled bigint past 2^53, a REAL out of f32 range (or a DOUBLE
    /// out of f64 range), an object key that names no struct field.
    fn silent_changes() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            ("[12345678901234567.0]", "array<bigint>", "array(bigint)"),
            ("[1.2345678901234567e16]", "array<bigint>", "array(bigint)"),
            ("[9007199254740993.0]", "array<bigint>", "array(bigint)"),
            ("[1e39]", "array<float>", "array(real)"),
            ("[\"1e39\"]", "array<float>", "array(real)"),
            ("[-1e39]", "array<float>", "array(real)"),
            ("[1e-50]", "array<float>", "array(real)"),
            ("[1e-400]", "array<double>", "array(double)"),
            // Subnormal doubles in a json value come back with other digits.
            ("[5e-324]", "array<json>", "array(json)"),
            ("{\"a\":[1e-323]}", "struct<a:json>", "row(a json)"),
            ("{\"a\":1,\"x\":2}", "struct<a:int>", "row(a integer)"),
            ("{\"b\":1}", "struct<a:int>", "row(a integer)"),
            // A key repeated with the same spelling: the engine's JSON
            // parser keeps only the last value.
            ("{\"a\":1,\"a\":2}", "struct<a:int>", "row(a integer)"),
            ("[{\"a\":1,\"a\":2}]", "array<struct<a:int>>", "array(row(a integer))"),
            ("{\"k\":1,\"k\":2}", "map<string,int>", "map(varchar, integer)"),
            ("{\"k\":[1],\"k\":[2,3]}", "map<string,array<int>>", "map(varchar, array(integer))"),
            ("{\"1\":1,\"1\":2}", "map<int,int>", "map(integer, integer)"),
            ("{\"a\":{\"k\":1,\"k\":2}}", "struct<a:json>", "row(a json)"),
            ("{\"k\":1,\"k\":2}", "json", "json"),
            ("[{\"k\":1,\"k\":2}]", "json", "json"),
            // A number past f64 anywhere in the document: valid JSON the
            // engine takes (as Infinity), so every other check still runs.
            ("[1e400]", "array<double>", "array(double)"),
            ("[-1e400]", "array<double>", "array(double)"),
            ("[\"1e400\"]", "array<double>", "array(double)"),
            ("[1e400]", "array<float>", "array(real)"),
            ("{\"k\":1,\"k\":2,\"z\":1e400}", "map<string,double>", "map(varchar, double)"),
            ("{\"k\":1,\"k\":2,\"z\":1e400}", "json", "json"),
            ("{\"a\":1,\"x\":2,\"b\":1e400}", "struct<a:int,b:double>", "row(a integer, b double)"),
            ("{\"a\":{\"k\":1,\"k\":2},\"b\":1e400}", "struct<a:json,b:json>", "row(a json, b json)"),
            ("{\"a\":1.5,\"b\":[1e400]}", "struct<a:int,b:json>", "row(a integer, b json)"),
            // A JSON value inside a nested type is re-read with its decimal
            // numbers as doubles (at the top level it keeps its text).
            ("{\"a\":[1e400]}", "struct<a:json>", "row(a json)"),
            ("[1e-400]", "array<json>", "array(json)"),
            ("[[-1e400]]", "array<json>", "array(json)"),
            ("[{\"x\":-0.0}]", "array<json>", "array(json)"),
            ("{\"a\":[123456789012345678.5]}", "struct<a:json>", "row(a json)"),
            ("{\"k\":{\"x\":0.1000000000000000055511151231257827}}", "map<string,json>", "map(varchar, json)"),
        ]
    }

    #[test]
    fn json_validity_does_not_depend_on_f64_range() {
        for v in ["[1e400]", "[-1E+400, 0, -0.5e-7, 10]", "{\"a\":1e400,\"b\":\"1e400\"}", "1e400", "[\"a-1\", 2]", "[1e-400]"] {
            assert!(is_json(v), "{v}");
        }
        for v in ["[01]", "[1.]", "[-]", "[1e]", "[+1]", "[.5]", "[1e400", "[1e4e4]", "[1-2]", "[nul1]", "[Infinity]", "{1e400:1}"] {
            assert!(!is_json(v), "{v}");
        }
        // A text holding a document with such a number still counts as one.
        assert!(matches!(literal(&Cell::Text("[1e400]".into()), "array<double>"), Err(Error::Query(_))));
        assert!(matches!(literal(&Cell::Text("{\"k\":1,\"k\":2,\"z\":1e400}".into()), "json"), Err(Error::Query(_))));
    }

    #[test]
    fn nested_values_the_engine_would_change_are_refused() {
        for (v, ty, _) in silent_changes() {
            assert!(matches!(literal(&Cell::Json(v.into()), ty), Err(Error::Query(_))), "{v} as {ty}");
        }
        // Keys that differ only in case name the same struct field (the
        // engine refuses those itself; refused here too).
        assert!(matches!(literal(&Cell::Json("{\"a\":1,\"A\":2}".into()), "struct<a:int>"), Err(Error::Query(_))));
        assert!(matches!(literal(&Cell::Text("{\"k\":1,\"k\":2}".into()), "json"), Err(Error::Query(_))));
        // Still exact: float-spelled integers up to 2^53, REAL in range
        // (subnormals too), the special values, keys in another case, a
        // struct with a missing field (NULL, as the source says).
        for (v, ty) in [
            ("[9007199254740992.0, -9007199254740992.0, 1e15, 12345678901234567]", "array<bigint>"),
            ("[3.4028235e38, 1e-40, 0.0, -0, \"Infinity\", \"NaN\"]", "array<float>"),
            ("[1e-300, 0e10]", "array<double>"),
            ("{\"A\":1}", "struct<a:int,b:int>"),
            ("{}", "struct<a:int>"),
            // The same key in different objects, or as a value, is no repeat.
            ("[{\"a\":1},{\"a\":2}]", "array<struct<a:int>>"),
            ("{\"a\":{\"a\":1}}", "map<string,map<string,int>>"),
            ("{\"a\\\"\":\"a\",\"a\":\"a\\\"\"}", "map<string,string>"),
            ("{\"a\":[{\"a\":1}],\"b\":{\"a\":2}}", "json"),
            // Past f64 in a JSON value is kept by the engine (as 1E+400).
            ("[1e400, {\"a\":-1E+400}]", "json"),
            ("{\"a\":[0.1, 1.10, 1E2, -2.5e-3, 12345678901234567890123, 1.7976931348623157e308, \"1e400\"]}", "struct<a:json>"),
            ("[{\"x\":0.0}, -0]", "array<json>"),
        ] {
            assert!(literal(&Cell::Json(v.into()), ty).is_ok(), "{v} as {ty}");
        }
    }

    /// Checks against a Trino (the engine behind Athena) that every value
    /// in [`silent_changes`] really comes back changed from the cast
    /// (ignored; needs the `dbine-test-trino` container):
    /// `cargo test -p dbine-driver-athena -- --ignored engine_changes`.
    #[test]
    #[ignore]
    fn engine_changes_what_the_load_refuses() {
        for (v, ty, engine) in silent_changes() {
            let q = format!("SELECT json_format(CAST(CAST(JSON '{v}' AS {engine}) AS JSON))");
            let out = std::process::Command::new("docker")
                .args(["exec", "dbine-test-trino", "trino", "--output-format", "TSV", "--execute", &q])
                .output()
                .expect("docker");
            assert!(out.status.success(), "{q}: {}", String::from_utf8_lossy(&out.stderr));
            let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let norm = |s: &str| s.replace(' ', "");
            assert_ne!(norm(&got), norm(v), "{q} kept the value, so refusing it isn't needed");
            eprintln!("{v} AS {engine} -> {got}");
            assert!(literal(&Cell::Json(v.into()), ty).is_err(), "{v} as {ty}");
        }
    }

    #[test]
    fn nested_zoned_timestamps_are_read_as_text() {
        assert_eq!(
            read_expr("a", "array<timestamptz>").unwrap(),
            "json_format(CAST(CAST(\"a\" AS array(varchar)) AS JSON))"
        );
        assert_eq!(
            read_expr("s", "struct<t:timestamp with time zone,u:timestamp,n:string>").unwrap(),
            "json_format(CAST(CAST(\"s\" AS row(\"t\" varchar, \"u\" timestamp(6), \"n\" varchar)) AS JSON))"
        );
        // What that read gives back loads again, zone and digits kept.
        let back = literal(&Cell::Json("[\"2024-01-02 03:04:05.123456 America/New_York\"]".into()), "array<timestamptz>").unwrap();
        assert!(back.contains("2024-01-02 03:04:05.123456 America/New_York"), "{back}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_load_dropped_during_the_start_stops_that_query() {
        let calls: Calls = Default::default();
        let get = |id: &str, stopped: bool, _| if stopped { qe(id, "CANCELLED", "") } else { qe(id, "RUNNING", "") };
        // Athena takes the request, but its answer comes after the load is gone.
        let mut s = fake_session(&fake_athena(handler(calls.clone(), get, || std::thread::sleep(Duration::from_millis(300)))).await);
        let spec = load_spec(100);
        let mut src = rows(3);
        let progress = |_: u64| {};
        let r = tokio::time::timeout(Duration::from_millis(50), s.transfer_load(&spec, &[], &mut src, &progress)).await;
        assert!(r.is_err(), "the load should still be starting its INSERT");
        for _ in 0..300 {
            if last_get_after_stop(&calls, "q1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(stops(&calls), ["q1"]);
        assert!(last_get_after_stop(&calls, "q1"));
    }
}

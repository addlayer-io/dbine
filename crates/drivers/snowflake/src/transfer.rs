//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! The driver talks to Snowflake only through the SQL API v2, and that
//! shapes both directions:
//!
//! - Reading: one `SELECT` (plus the optional filter; nothing else runs on
//!   the source). The SQL API only serves results as `jsonv2` (every value
//!   as text, no Arrow chunks), so each value is turned into its typed cell
//!   by the column's `rowType`: `NUMBER` with scale 0 → integer (exact
//!   decimal when it doesn't fit 64 bits), other `NUMBER`s → exact decimal
//!   from the server's own digits, `FLOAT` → float, `BINARY` → bytes
//!   (whole), `DATE` / `TIME` / `TIMESTAMP_NTZ` → date, time, date-time,
//!   `TIMESTAMP_LTZ` → date-time in UTC with `+00:00`, `TIMESTAMP_TZ` →
//!   date-time with its own offset, `VARIANT` / `OBJECT` / `ARRAY` /
//!   `VECTOR` → JSON (the server's text, only its layout whitespace
//!   removed: numbers are never re-parsed, so `NUMBER(38)` inside a
//!   `VARIANT` stays exact; a value with Snowflake's non-JSON tokens,
//!   `undefined` for a SQL NULL inside an array or object and `NaN` /
//!   `Infinity`, is refused: JSON can't carry it, and as text or `null`
//!   it would change), anything else (geospatial…) → text. The
//!   result's partitions are fetched one at a time, so memory stays at
//!   about one partition. `VECTOR`, `MAP` and structured `OBJECT` /
//!   `ARRAY` columns take their full declared type from `DESCRIBE`
//!   (`rowType` and INFORMATION_SCHEMA only give the bare name).
//! - Loading: the fast path of the other drivers (`PUT` a file to a stage,
//!   then `COPY INTO`) isn't possible here: the SQL API refuses `PUT` / `GET`
//!   (files go up only through a client that speaks Snowflake's own
//!   protocol and cloud storage upload), and this driver has no such
//!   client. So the load is `INSERT INTO t (…) SELECT <conversions> FROM
//!   VALUES (?, …), …` with every value bound as text: up to
//!   [`STMT_ROWS`] rows (Snowflake's limit for a `VALUES` list), a
//!   statement text of at most [`STMT_TEXT`] and a request of at most
//!   [`REQUEST_BYTES`]. The `SELECT` converts each value to the target
//!   column's type (`TO_BINARY(…, 'HEX')`, `PARSE_JSON`, `TO_DATE` & co.,
//!   casts), which a plain `INSERT … VALUES` can't. NULLs go as the `NULL`
//!   literal.
//! - Dates, times and timestamps are rewritten to one fixed text per
//!   target (always nine fractional digits, always an offset for `_TZ` /
//!   `_LTZ`, a date-time without one taken as UTC) and read with
//!   `TO_DATE` / `TO_TIME` / `TO_TIMESTAMP_*` and an explicit format, so an
//!   account's or user's `DATE_INPUT_FORMAT` & co. never come into play.
//! - All or nothing: every SQL API request is its own server session, so
//!   a transaction can't span requests (and bindings aren't allowed in
//!   multi-statement requests). So the `INSERT`s go into a transient
//!   staging table next to the target ([`staging_name`]: the target's name
//!   plus a random suffix, one per load, created empty from the target's
//!   columns and never replacing anything), and the target gets a single
//!   `INSERT … SELECT` from it at the end: one atomic statement. Two loads
//!   into the same table (other DBine windows, users or machines) never
//!   share a staging table. The staging table is dropped after (or on any
//!   error or cancel); a load's start drops DBine staging tables of the
//!   same target left by a killed process (untouched for a day). Every
//!   staging statement is smaller than the `commit_rows` /
//!   `commit_bytes` window; `progress` gets the staged total after each
//!   one and the final total at the end. Up to [`IN_FLIGHT`] statements
//!   run at once (Snowflake executes them in parallel on the warehouse).
//! - Nothing commits after `bulk_load` returns: statements are submitted
//!   with `async=true` so their handle comes back at once, and the submit
//!   runs on its own task, so a submit cut short by an error or a drop
//!   still gets its handle. On an error, clean-up waits for the submits
//!   still on their way, cancels (`/cancel`) every statement that may still
//!   run and drops the staging table, all before returning; when the
//!   future is dropped (a cancel), a guard does the same in the background.
//!   This covers the final statement too, and its handle is also the
//!   session's, so its interrupter cancels it. What's left is inherent to
//!   any engine: a cancel that reaches the server after the final
//!   statement committed.
//! - The load runs with `TIMEZONE = UTC` (one of the SQL API's accepted
//!   parameters); nothing depends on it, given the explicit offsets.
//! - HTTP 429 (too many requests: the statement wasn't taken) is retried
//!   with a backoff; other failures are not (an `INSERT` isn't idempotent).
//! - Memory: the statement being filled keeps its bindings as the
//!   request's own JSON bytes (never as `serde_json` values, which take
//!   several times their text), in buffers sized once to the request's
//!   limit; the request is then finished in place and handed to the HTTP
//!   client. At most [`REQUEST_BYTES`] per request, [`IN_FLIGHT`] of them
//!   plus the one being filled (and its statement text, [`STMT_TEXT`],
//!   twice while the request is finished): about 27 MiB per table.
//! - `table_lock` has no equivalent (Snowflake doesn't lock tables for
//!   inserts). Identity columns accept given values, so `keep_identity`
//!   needs nothing; without it, identity columns are left out and
//!   Snowflake generates them.

use super::{Api, SnowflakeSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Rows per `INSERT` (the most a `VALUES` list takes).
pub(crate) const STMT_ROWS: usize = 16_384;
/// Statement text per `INSERT` (placeholders only: the values are bound).
pub(crate) const STMT_TEXT: usize = 1024 * 1024;
/// JSON size of one request (statement and bindings), counted exactly.
pub(crate) const REQUEST_BYTES: usize = 5 * 1024 * 1024;
/// `INSERT`s running at once.
pub(crate) const IN_FLIGHT: usize = 4;

// ---------------------------------------------------------------- reading

/// How a result column is read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Read {
    Int,
    Decimal,
    Float,
    Bool,
    Date,
    Time,
    Ntz,
    Ltz,
    Tz,
    Bytes,
    Json,
    Text,
}

pub(crate) fn read_kind(col: &Json) -> Read {
    let ty = col.get("type").and_then(Json::as_str).unwrap_or("").to_ascii_lowercase();
    let scale = col.get("scale").and_then(Json::as_i64).unwrap_or(0);
    match ty.as_str() {
        "fixed" if scale == 0 => Read::Int,
        "fixed" => Read::Decimal,
        "real" | "float" | "double" => Read::Float,
        "boolean" => Read::Bool,
        "date" => Read::Date,
        "time" => Read::Time,
        "timestamp_ntz" => Read::Ntz,
        "timestamp_ltz" => Read::Ltz,
        "timestamp_tz" => Read::Tz,
        "binary" => Read::Bytes,
        "variant" | "object" | "array" | "vector" | "map" => Read::Json,
        _ => Read::Text,
    }
}

/// A `rowType` entry as a column (its type spelled as Snowflake declares it).
pub(crate) fn transfer_column(col: &Json) -> TransferColumn {
    let n = |k: &str| col.get(k).and_then(Json::as_i64);
    let ty = col.get("type").and_then(Json::as_str).unwrap_or("").to_ascii_uppercase();
    let type_name = match ty.as_str() {
        "FIXED" => format!("NUMBER({},{})", n("precision").unwrap_or(38), n("scale").unwrap_or(0)),
        "REAL" => "FLOAT".into(),
        "TEXT" => n("length").map_or("VARCHAR".into(), |l| format!("VARCHAR({l})")),
        "BINARY" => n("length").map_or("BINARY".into(), |l| format!("BINARY({l})")),
        "TIME" | "TIMESTAMP_NTZ" | "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" => format!("{ty}({})", n("scale").unwrap_or(9)),
        _ => ty,
    };
    TransferColumn {
        name: col.get("name").and_then(Json::as_str).unwrap_or("").to_string(),
        type_name,
        nullable: col.get("nullable").and_then(Json::as_bool).unwrap_or(true),
    }
}

/// `SELECT` of the read: only the given columns, only the filter's rows.
pub(crate) fn select_sql(spec: &ReadSpec) -> String {
    let cols = match &spec.columns {
        Some(c) if !c.is_empty() => c.iter().map(|c| qualified_name(Quote::Double, None, c)).collect::<Vec<_>>().join(", "),
        _ => "*".to_string(),
    };
    let mut sql = format!("SELECT {cols} FROM {}", qualified_name(Quote::Double, spec.table.schema(), &spec.table.name));
    if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    sql
}

/// `"1706708700.123000000"` → (seconds, nanoseconds), negative epochs
/// counting the fraction forward.
fn epoch(s: &str) -> Option<(i64, u32)> {
    let (sec, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = sec.parse().ok()?;
    let nanos: u32 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").get(..9)?.parse().ok()? };
    // `-0.5` is half a second before the epoch (the sign is on the text:
    // `-0` parses as 0).
    if sec.starts_with('-') && nanos > 0 {
        return Some((secs - 1, 1_000_000_000 - nanos));
    }
    Some((secs, nanos))
}

/// `.fffffffff` without trailing zeros (nothing for whole seconds).
fn fraction(nanos: u32) -> String {
    if nanos == 0 {
        return String::new();
    }
    let f = format!(".{nanos:09}");
    f.trim_end_matches('0').to_string()
}

fn date_time(secs: i64, nanos: u32) -> Option<chrono::NaiveDateTime> {
    chrono::DateTime::from_timestamp(secs, nanos).map(|t| t.naive_utc())
}

/// One `jsonv2` value (text or null) as a lossless cell; `Unsupported`
/// for a semi-structured value no cell can carry as it is.
pub(crate) fn read_cell(kind: Read, v: &Json) -> Result<Cell> {
    let Some(s) = v.as_str() else {
        return Ok(match v {
            Json::Null => Cell::Null,
            other => Cell::from_json(other),
        });
    };
    let text = || Cell::Text(s.to_string());
    if kind == Read::Json {
        return match compact_json(s) {
            Some(j) => Ok(Cell::Json(j)),
            None => match non_json_token(s) {
                Some(t) => Err(Error::Unsupported(format!(
                    "tiene un valor semiestructurado con «{t}» ({}), que JSON no puede representar; pasarlo como texto o como null \
                     cambiaría el dato, así que la tabla no se copia. Convertí esos valores en el origen (por ejemplo con un filtro) \
                     antes de copiarla",
                    if t == "undefined" { "un NULL de SQL dentro de un arreglo u objeto" } else { "un número no finito" }
                ))),
                None => Ok(text()),
            },
        };
    }
    Ok(match kind {
        Read::Int => s.parse::<i64>().map_or_else(|_| Cell::Decimal(s.to_string()), Cell::Int),
        Read::Decimal => Cell::Decimal(s.to_string()),
        Read::Float => s.parse::<f64>().map_or_else(|_| text(), Cell::Float),
        Read::Bool => match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Cell::Bool(true),
            "false" | "0" => Cell::Bool(false),
            _ => text(),
        },
        Read::Date => s
            .parse::<i64>()
            .ok()
            .and_then(|d| chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(d)))
            .map_or_else(text, |d| Cell::Date(d.format("%Y-%m-%d").to_string())),
        Read::Time => epoch(s)
            .map(|(secs, nanos)| {
                let t = secs.rem_euclid(86_400);
                Cell::Time(format!("{:02}:{:02}:{:02}{}", t / 3600, t / 60 % 60, t % 60, fraction(nanos)))
            })
            .unwrap_or_else(text),
        Read::Ntz => epoch(s)
            .and_then(|(secs, nanos)| date_time(secs, nanos))
            .map_or_else(text, |t| Cell::DateTime(format!("{}{}", t.format("%Y-%m-%d %H:%M:%S"), fraction(t.and_utc().timestamp_subsec_nanos())))),
        Read::Ltz => epoch(s)
            .and_then(|(secs, nanos)| date_time(secs, nanos))
            .map_or_else(text, |t| {
                Cell::DateTimeTz(format!("{}{}+00:00", t.format("%Y-%m-%d %H:%M:%S"), fraction(t.and_utc().timestamp_subsec_nanos())))
            }),
        Read::Tz => timestamp_tz(s).map_or_else(text, Cell::DateTimeTz),
        Read::Bytes => unhex(s).map_or_else(text, Cell::Bytes),
        Read::Json | Read::Text => text(),
    })
}

/// Snowflake's non-JSON token (`undefined`, `NaN`, `Infinity`) in a
/// semi-structured value that is JSON otherwise.
pub(crate) fn non_json_token(s: &str) -> Option<&'static str> {
    const TOKENS: [&str; 3] = ["undefined", "NaN", "Infinity"];
    let mut found = None;
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut esc) = (false, false);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String, found: &mut Option<&'static str>| {
        match TOKENS.iter().find(|t| **t == word.as_str()) {
            Some(t) => {
                found.get_or_insert(*t);
                out.push_str("null");
            }
            None => out.push_str(word),
        }
        word.clear();
    };
    for c in s.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c.is_ascii_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out, &mut found);
            in_str = c == '"';
            out.push(c);
        }
    }
    flush(&mut word, &mut out, &mut found);
    // `-Infinity` became `-null`, which isn't JSON: drop the sign before
    // checking (only a check: a string's text changing doesn't matter).
    let out = out.replace("-null", "null");
    found.filter(|_| serde_json::from_str::<serde::de::IgnoredAny>(&out).is_ok())
}

/// A JSON text without its layout whitespace, every token kept verbatim
/// (numbers aren't parsed into `f64`, so none loses digits); `None` when
/// it isn't JSON.
pub(crate) fn compact_json(s: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(s).ok()?;
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut esc) = (false, false);
    for c in s.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !matches!(c, ' ' | '\t' | '\n' | '\r') {
            out.push(c);
        }
    }
    Some(out)
}

/// Types whose bare name (all `rowType` and INFORMATION_SCHEMA give) isn't
/// the whole declared type: `VECTOR(FLOAT, 3)`, `MAP(VARCHAR, NUMBER)`,
/// structured `OBJECT(…)` / `ARRAY(…)`.
fn needs_declared(base: &str) -> bool {
    matches!(base.trim().to_ascii_uppercase().as_str(), "VECTOR" | "MAP" | "OBJECT" | "ARRAY")
}

/// The full type of `column`: the one `DESCRIBE` declared when there is
/// one, else the bare `base`. `VECTOR` and `MAP` aren't valid types
/// without their parameters, so those fail instead of passing on a type
/// that can't be created or cast to.
pub(crate) fn declared_type(column: &str, base: &str, described: Option<&str>) -> Result<String> {
    let b = base.trim().to_ascii_uppercase();
    match described.map(str::trim).filter(|d| d.to_ascii_uppercase().starts_with(&b)) {
        Some(d) if d.contains('(') || !matches!(b.as_str(), "VECTOR" | "MAP") => Ok(d.to_string()),
        _ if matches!(b.as_str(), "VECTOR" | "MAP") => Err(Error::Unsupported(format!(
            "no se pudo saber el tipo completo de la columna «{column}» ({b}: falta el tipo de sus elementos o su dimensión); \
             Snowflake no lo informa en el resultado y DESCRIBE no lo devolvió"
        ))),
        _ => Ok(b),
    }
}

/// Declared types by column name, from `DESCRIBE TABLE` (or `VIEW`).
async fn describe(s: &SnowflakeSession, table_sql: &str) -> Result<HashMap<String, String>> {
    let rows = match s.text_rows(&format!("DESCRIBE TABLE {table_sql}"), &[]).await {
        Ok(r) => r,
        Err(_) => s.text_rows(&format!("DESCRIBE VIEW {table_sql}"), &[]).await?,
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let mut r = r.into_iter();
            Some((r.next()??, r.next()??))
        })
        .collect())
}

/// `"<epoch> <offset minutes + 1440>"` → `YYYY-MM-DD HH:MM:SS[.f]±HH:MM`.
fn timestamp_tz(s: &str) -> Option<String> {
    let (e, off) = s.split_once(' ')?;
    let (secs, nanos) = epoch(e)?;
    let minutes: i32 = off.trim().parse::<i32>().ok()? - 1440;
    let tz = chrono::FixedOffset::east_opt(minutes * 60)?;
    let t = chrono::DateTime::from_timestamp(secs, nanos)?.with_timezone(&tz);
    Some(format!("{}{}{}", t.format("%Y-%m-%d %H:%M:%S"), fraction(nanos), t.format("%:z")))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn lock(sink: &BatchSinkRef) -> Result<std::sync::MutexGuard<'_, dyn dbine_driver::transfer::BatchSink + 'static>> {
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))
}

pub(crate) async fn read_batches(s: &SnowflakeSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let mut page = s.submit(&select_sql(spec), None, false).await?;
    let meta = page.get("resultSetMetaData").cloned().unwrap_or(Json::Null);
    let row_type = meta.get("rowType").and_then(Json::as_array).cloned().unwrap_or_default();
    let kinds: Vec<Read> = row_type.iter().map(read_kind).collect();
    let mut columns: Vec<TransferColumn> = row_type.iter().map(transfer_column).collect();
    if columns.iter().any(|c| needs_declared(&c.type_name)) {
        let table = qualified_name(Quote::Double, spec.table.schema(), &spec.table.name);
        // Without it the others keep their (valid) bare name, and VECTOR /
        // MAP fail with the reason (`declared_type`).
        let described = describe(s, &table).await.unwrap_or_else(|e| {
            tracing::debug!("snowflake describe {table}: {e}");
            HashMap::new()
        });
        for c in columns.iter_mut().filter(|c| needs_declared(&c.type_name)) {
            c.type_name = declared_type(&c.name, &c.type_name, described.get(&c.name).map(String::as_str))?;
        }
    }
    let partitions = meta.get("partitionInfo").and_then(Json::as_array).map_or(1, Vec::len).max(1);
    let handle = page.get("statementHandle").and_then(Json::as_str).unwrap_or_default().to_string();
    lock(&sink)?.begin(&columns)?;
    let mut builder = BatchBuilder::new();
    for p in 0..partitions {
        if p > 0 {
            page = s.api.get(&format!("/api/v2/statements/{handle}"), &[("partition", p.to_string())]).await?.1;
        }
        let data = match page.get_mut("data").map(Json::take) {
            Some(Json::Array(rows)) => rows,
            _ => Vec::new(),
        };
        // The sink may block (backpressure): never across an await.
        let mut g = lock(&sink)?;
        for row in data {
            let vals = row.as_array().map_or(&[][..], Vec::as_slice);
            let cells = kinds
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    read_cell(*k, vals.get(i).unwrap_or(&Json::Null)).map_err(|e| match e {
                        Error::Unsupported(m) => Error::Unsupported(format!("la columna «{}» {m}", columns[i].name)),
                        e => e,
                    })
                })
                .collect::<Result<Vec<Cell>>>()?;
            builder.push(cells, &mut *g)?;
        }
    }
    builder.flush(&mut *lock(&sink)?)?;
    Ok(builder.rows)
}

// ---------------------------------------------------------------- loading

/// How a value goes into a target column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Load {
    /// Character column: the text as is.
    Text,
    Number,
    Bool,
    /// Hex text through `TO_BINARY(…, 'HEX')`.
    Binary,
    /// JSON text through `PARSE_JSON`, then this cast (`OBJECT`, `ARRAY`,
    /// `VECTOR(…)`; none for `VARIANT`).
    Json(Option<String>),
    /// A date / time / timestamp column: the text rewritten to one fixed
    /// form ([`temporal_text`]) and read with an explicit format.
    Temporal(Temporal),
    /// Text cast to the column's type (geospatial…).
    Cast,
}

/// The temporal target families.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Temporal {
    Date,
    Time,
    /// `TIMESTAMP_NTZ` (wall clock, no zone).
    Ntz,
    /// `TIMESTAMP_LTZ` / `TIMESTAMP_TZ`: an instant (with an offset).
    Zoned,
}

const DATE_FMT: &str = "YYYY-MM-DD";
const TIME_FMT: &str = "HH24:MI:SS.FF9";
const NTZ_FMT: &str = "YYYY-MM-DD HH24:MI:SS.FF9";
const TZ_FMT: &str = "YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM";

/// A temporal text (`YYYY-MM-DD`, `HH:MM:SS[.f]`, `YYYY-MM-DD[ T]HH:MM:SS[.f]`
/// with an optional `Z` / `±HH[:MM]`) in the one form the target's
/// explicit format reads: nine fractional digits, and an offset for
/// `Zoned` (`+00:00` when there is none: a date-time without an offset is
/// UTC, the same way `TIMESTAMP_LTZ` is read). `None` when it isn't such a
/// text (it then goes as is, and Snowflake's own error names it).
pub(crate) fn temporal_text(s: &str, to: Temporal) -> Option<String> {
    use chrono::{NaiveDate, NaiveTime, Timelike};
    let s = s.trim();
    let time_of = |t: &str| NaiveTime::parse_from_str(t, "%H:%M:%S%.f").or_else(|_| NaiveTime::parse_from_str(t, "%H:%M")).ok();
    // Split off an offset: `Z`, or a sign after the time part.
    let (body, offset) = if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        (b, Some(0))
    } else {
        match s.get(10..).and_then(|rest| rest.rfind(['+', '-']).map(|i| i + 10)) {
            Some(i) => {
                let (b, o) = s.split_at(i);
                let sign = if o.starts_with('-') { -1 } else { 1 };
                let digits: String = o[1..].chars().filter(char::is_ascii_digit).collect();
                let (h, m) = match digits.len() {
                    2 => (digits.parse::<i32>().ok()?, 0),
                    4 => (digits[..2].parse::<i32>().ok()?, digits[2..].parse::<i32>().ok()?),
                    _ => return None,
                };
                (b.trim_end(), Some(sign * (h * 60 + m)))
            }
            None => (s, None),
        }
    };
    let (date, time) = match body.split_once([' ', 'T']) {
        Some((d, t)) => (Some(NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?), Some(time_of(t.trim())?)),
        None if body.len() >= 10 && body.as_bytes().get(4) == Some(&b'-') => (Some(NaiveDate::parse_from_str(body, "%Y-%m-%d").ok()?), None),
        None => (None, Some(time_of(body)?)),
    };
    let t = time.unwrap_or(NaiveTime::MIN);
    let hms = |t: NaiveTime| format!("{}.{:09}", t.format("%H:%M:%S"), t.nanosecond() % 1_000_000_000);
    Some(match to {
        Temporal::Date => date?.format("%Y-%m-%d").to_string(),
        Temporal::Time => hms(t),
        Temporal::Ntz => format!("{} {}", date?.format("%Y-%m-%d"), hms(t)),
        Temporal::Zoned => {
            let m = offset.unwrap_or(0);
            format!("{} {} {}{:02}:{:02}", date?.format("%Y-%m-%d"), hms(t), if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60)
        }
    })
}

/// A target column, from INFORMATION_SCHEMA.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    pub name: String,
    /// The declared type (`NUMBER(38,0)`, `TIMESTAMP_TZ`…).
    pub sql_type: String,
    pub load: Load,
    pub identity: bool,
}

impl Target {
    pub(crate) fn new(name: &str, data_type: &str, len: Option<&str>, precision: Option<&str>, scale: Option<&str>, identity: bool) -> Target {
        let t = data_type.trim().to_ascii_uppercase();
        let base = t.split('(').next().unwrap_or("").trim().to_string();
        // A parameterized type keeps its own spelling (structured field
        // names are case sensitive).
        let sql_type = if t.contains('(') {
            data_type.trim().to_string()
        } else {
            super::ddl::column_type(t.clone(), len.map(Into::into), precision.map(Into::into), scale.map(Into::into))
        };
        let load = match base.as_str() {
            "TEXT" | "VARCHAR" | "STRING" | "CHAR" | "CHARACTER" | "NCHAR" | "NVARCHAR" => Load::Text,
            "NUMBER" | "DECIMAL" | "NUMERIC" | "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" | "BYTEINT" | "FLOAT" | "FLOAT4"
            | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" | "REAL" => Load::Number,
            "BOOLEAN" => Load::Bool,
            "BINARY" | "VARBINARY" => Load::Binary,
            "VARIANT" => Load::Json(None),
            "OBJECT" | "ARRAY" | "MAP" => Load::Json(Some(sql_type.clone())),
            // A vector is built from an array.
            "VECTOR" => Load::Json(Some(format!("ARRAY::{sql_type}"))),
            "DATE" => Load::Temporal(Temporal::Date),
            "TIME" => Load::Temporal(Temporal::Time),
            "TIMESTAMP_NTZ" | "DATETIME" | "TIMESTAMP" => Load::Temporal(Temporal::Ntz),
            "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" => Load::Temporal(Temporal::Zoned),
            _ => Load::Cast,
        };
        Target { name: name.to_string(), sql_type, load, identity }
    }

    /// The select-list item converting `columnN` (the `VALUES` column).
    pub(crate) fn expr(&self, n: usize) -> String {
        let c = format!("column{n}");
        match &self.load {
            Load::Text => c,
            Load::Binary => format!("TO_BINARY({c}, 'HEX')"),
            Load::Json(None) => format!("PARSE_JSON({c})"),
            Load::Json(Some(cast)) => format!("PARSE_JSON({c})::{cast}"),
            Load::Temporal(Temporal::Date) => format!("TO_DATE({c}, '{DATE_FMT}')"),
            Load::Temporal(Temporal::Time) => format!("TO_TIME({c}, '{TIME_FMT}')::{}", self.sql_type),
            Load::Temporal(Temporal::Ntz) => format!("TO_TIMESTAMP_NTZ({c}, '{NTZ_FMT}')::{}", self.sql_type),
            // Through `TIMESTAMP_TZ` (the instant) also for `_LTZ`.
            Load::Temporal(Temporal::Zoned) => format!("TO_TIMESTAMP_TZ({c}, '{TZ_FMT}')::{}", self.sql_type),
            Load::Number | Load::Bool | Load::Cast => format!("{c}::{}", self.sql_type),
        }
    }
}

/// A cell as the text bound for a column of `load`; `None` for NULL.
pub(crate) fn bind_text(cell: Cell, load: &Load) -> Option<String> {
    let json = matches!(load, Load::Json(_));
    let quoted = |s: &str| Json::String(s.to_string()).to_string();
    let cell = match (cell, load) {
        (Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Text(s), Load::Temporal(to)) => {
            return Some(temporal_text(&s, *to).unwrap_or(s));
        }
        (cell, _) => cell,
    };
    Some(match cell {
        Cell::Null => return None,
        Cell::Bool(b) => match load {
            Load::Number => (if b { "1" } else { "0" }).into(),
            _ => b.to_string(),
        },
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if json && !f.is_finite() => quoted(&f.to_string()),
        Cell::Float(f) => f.to_string(),
        Cell::Decimal(s) => s,
        Cell::Bytes(b) if json => quoted(&hex(&b)),
        Cell::Bytes(b) => hex(&b),
        Cell::Json(s) => s,
        Cell::Text(s) => match load {
            Load::Binary => match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) if unhex(h).is_some() => h.to_string(),
                _ => hex(s.as_bytes()),
            },
            // A JSON document kept as text stays a document; any other
            // text is a string value.
            Load::Json(_) => {
                let t = s.trim_start();
                if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde::de::IgnoredAny>(&s).is_ok() {
                    s
                } else {
                    quoted(&s)
                }
            }
            _ => s,
        },
        Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => {
            if json {
                quoted(&s)
            } else {
                s
            }
        }
    })
}

/// Bytes of `s` as a JSON string, exactly as serde_json writes it: quotes,
/// two-byte escapes, `\u00XX` (six bytes) for the other control
/// characters.
pub(crate) fn json_len(s: &str) -> usize {
    2 + s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\t' | b'\r' | 0x08 | 0x0c => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

/// A binding's JSON besides its value: `"123456":{"type":"TEXT","value":…},`.
const BINDING: usize = 48;

/// A row's values as bound text (`None` for NULL) and their exact share of
/// the request.
pub(crate) struct Bound {
    vals: Vec<Option<String>>,
    bytes: usize,
}

/// Start of a request's JSON: the bindings go first, written as they come.
const BINDINGS_HEAD: &[u8] = b"{\"bindings\":{";
/// Room for the request's other fields (statement aside): context,
/// parameters, timeout.
const REQUEST_TAIL: usize = 16 * 1024;

/// One `INSERT … SELECT … FROM VALUES` being filled.
///
/// Its bindings are kept as the request's own JSON bytes (`"1":{"type":
/// "TEXT","value":"…"},…`), never as `serde_json` values, so what it holds
/// is what it sends; both buffers are sized once, on the first row, to the
/// request's limit, so they never grow past it by doubling.
pub(crate) struct Insert {
    /// `INSERT INTO t ("A", …) SELECT <exprs> FROM VALUES `
    head: String,
    /// `head`'s size as a JSON string.
    head_json: usize,
    targets: Vec<Target>,
    sql: String,
    /// `{"bindings":{` and the bindings so far.
    body: Vec<u8>,
    binds: usize,
    pub rows: usize,
    max_rows: usize,
    max_bytes: usize,
}

/// A filled statement, taken out of its [`Insert`].
pub(crate) struct Taken {
    pub sql: String,
    body: Vec<u8>,
    binds: usize,
    pub rows: usize,
}

impl Taken {
    /// The bindings as JSON (tests).
    #[cfg(test)]
    pub(crate) fn bindings(&self) -> Json {
        let mut b = self.body[BINDINGS_HEAD.len() - 1..].to_vec();
        b.push(b'}');
        serde_json::from_slice(&b).unwrap()
    }

    /// The whole request: `rest(statement)` gives its other fields (an
    /// object), appended in place after the bindings.
    pub(crate) fn request(self, rest: impl FnOnce(String) -> Json) -> Result<Vec<u8>> {
        let rest = serde_json::to_vec(&rest(self.sql))?;
        if self.binds == 0 {
            return Ok(rest);
        }
        let mut body = self.body;
        body.extend_from_slice(b"},");
        // `rest` without its `{`.
        body.extend_from_slice(rest.get(1..).unwrap_or_default());
        Ok(body)
    }
}

impl Insert {
    /// `max_rows` / `max_bytes`: the commit window (a statement never
    /// goes past it, nor past [`STMT_ROWS`] / [`REQUEST_BYTES`]).
    pub(crate) fn new(table: &str, targets: Vec<Target>, max_rows: u64, max_bytes: u64) -> Insert {
        let names: Vec<String> = targets.iter().map(|t| qualified_name(Quote::Double, None, &t.name)).collect();
        let exprs: Vec<String> = targets.iter().enumerate().map(|(i, t)| t.expr(i + 1)).collect();
        let head = format!("INSERT INTO {table} ({}) SELECT {} FROM VALUES ", names.join(", "), exprs.join(", "));
        Insert {
            sql: String::new(),
            head_json: json_len(&head),
            head,
            targets,
            body: Vec::new(),
            binds: 0,
            rows: 0,
            max_rows: (max_rows.max(1) as usize).min(STMT_ROWS),
            max_bytes: (max_bytes.max(1) as usize).min(REQUEST_BYTES),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// A row (its cells in the order of the targets) as bound text.
    pub(crate) fn bind(&self, row: Vec<Cell>) -> Bound {
        let vals: Vec<Option<String>> = row.into_iter().zip(&self.targets).map(|(c, t)| bind_text(c, &t.load)).collect();
        let bytes = vals.iter().flatten().map(|v| json_len(v) + BINDING).sum();
        Bound { vals, bytes }
    }

    /// The request's size so far: bindings plus the statement text as a
    /// JSON string (identifiers' quotes are escaped; what follows the head
    /// is only `(?, NULL, …)`, which JSON writes as is), without
    /// rescanning the text.
    pub(crate) fn request_bytes(&self) -> usize {
        self.body.len() + self.head_json + self.sql.len().saturating_sub(self.head.len()) + 512
    }

    /// Would `row` still fit? (An empty statement takes any row.)
    pub(crate) fn fits(&self, row: &Bound) -> bool {
        if self.rows == 0 {
            return true;
        }
        let text = self.targets.len() * 6 + 4;
        self.rows < self.max_rows && self.sql.len() + text <= STMT_TEXT && self.request_bytes() + row.bytes + 2 * text <= self.max_bytes
    }

    /// Add a row.
    pub(crate) fn push(&mut self, row: Bound) {
        if self.rows == 0 {
            // Sized once for the whole statement (a lone row larger than
            // a request still goes, and grows them).
            self.sql.reserve(self.head.len() + STMT_TEXT.min(self.max_bytes) + self.targets.len() * 6 + 4);
            self.sql.push_str(&self.head);
            self.body.reserve(self.max_bytes.max(row.bytes) + REQUEST_TAIL);
            self.body.extend_from_slice(BINDINGS_HEAD);
        } else {
            self.sql.push_str(", ");
        }
        self.sql.push('(');
        for (i, v) in row.vals.into_iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            match v {
                None => self.sql.push_str("NULL"),
                Some(v) => {
                    self.sql.push('?');
                    if self.binds > 0 {
                        self.body.push(b',');
                    }
                    self.binds += 1;
                    let n = self.binds;
                    self.body.extend_from_slice(format!("\"{n}\":{{\"type\":\"TEXT\",\"value\":").as_bytes());
                    // Writing a `&str` into a `Vec` can't fail.
                    let _ = serde_json::to_writer(&mut self.body, &v);
                    self.body.push(b'}');
                }
            }
        }
        self.sql.push(')');
        self.rows += 1;
    }

    /// The statement and its bindings; the insert starts over empty (its
    /// buffers go with the statement).
    pub(crate) fn take(&mut self) -> Taken {
        Taken {
            sql: std::mem::take(&mut self.sql),
            body: std::mem::take(&mut self.body),
            binds: std::mem::take(&mut self.binds),
            rows: std::mem::take(&mut self.rows),
        }
    }
}

/// Target columns in the order of `names`, and the positions of the
/// batches' cells that are loaded (identity columns left out without
/// `keep_identity`).
pub(crate) fn plan_targets(catalog: &[Target], names: &[String], keep_identity: bool) -> Result<(Vec<Target>, Vec<usize>)> {
    let mut targets = Vec::new();
    let mut keep = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let t = catalog
            .iter()
            .find(|t| &t.name == n)
            .or_else(|| catalog.iter().find(|t| t.name.eq_ignore_ascii_case(n)))
            .ok_or_else(|| Error::Query(format!("la columna «{n}» no existe en la tabla de destino")))?;
        if t.identity && !keep_identity {
            continue;
        }
        targets.push(t.clone());
        keep.push(i);
    }
    if targets.is_empty() {
        return Err(Error::Query("no hay columnas para cargar".into()));
    }
    Ok((targets, keep))
}

/// Parameters of every load statement. Only `TIMEZONE`, one of the SQL
/// API's accepted parameters; the temporal values carry their own format
/// and offset ([`temporal_text`]), so no `*_INPUT_FORMAT` is needed.
pub(crate) fn load_parameters() -> Json {
    json!({ "TIMEZONE": "UTC" })
}

/// Comment that marks a staging table as DBine's (only such a table is
/// ever dropped by a later load).
pub(crate) const STAGING_COMMENT: &str = "DBine: carga masiva en curso";
/// Marker between the target's name and a staging table's suffix.
const STAGING_MARK: &str = "__DBINE_CARGA_";
/// A staging table untouched this long is a leftover of a killed load.
const STAGING_STALE_HOURS: u32 = 24;

/// A fresh random suffix for one load's staging table (16 hex digits).
pub(crate) fn staging_suffix() -> String {
    use aws_lc_rs::rand::SecureRandom;
    let mut b = [0u8; 8];
    if aws_lc_rs::rand::SystemRandom::new().fill(&mut b).is_err() {
        // No system randomness: the clock and the process still tell
        // loads apart.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        b = (t ^ (u64::from(std::process::id()) << 40)).to_le_bytes();
    }
    hex(&b)
}

/// The staging table of one load into `table`: next to it, with the load's
/// own suffix, so two loads into the same table (any process or machine)
/// never share one.
pub(crate) fn staging_name(table: &str, suffix: &str) -> String {
    format!("{table}{STAGING_MARK}{suffix}")
}

/// Is `name` a staging table of a load into `table`?
pub(crate) fn is_staging_of(table: &str, name: &str) -> bool {
    name.strip_prefix(table)
        .and_then(|r| r.strip_prefix(STAGING_MARK))
        .is_some_and(|sfx| sfx.len() == 16 && sfx.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)))
}

/// Attempts on HTTP 429 before giving up.
const RETRIES_429: u32 = 8;

/// One API call; HTTP 429 (too many requests: the call wasn't taken) is
/// retried after `Retry-After` or a doubling wait.
async fn call(api: &Api, req: reqwest::RequestBuilder) -> Result<(u16, Json)> {
    let mut wait = Duration::from_millis(500);
    let mut attempt = 0;
    loop {
        let r = req.try_clone().ok_or_else(|| Error::State("pedido a Snowflake no repetible".into()))?;
        let (authz, kind) = api.auth.headers()?;
        let resp = r
            .header("Authorization", authz)
            .header("X-Snowflake-Authorization-Token-Type", kind)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 429 {
            attempt += 1;
            if attempt > RETRIES_429 {
                return Err(Error::Connect("Snowflake rechazó los pedidos por exceso de carga (HTTP 429)".into()));
            }
            let after = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
            tokio::time::sleep(after.map_or(wait, Duration::from_secs).min(Duration::from_secs(30))).await;
            wait = (wait * 2).min(Duration::from_secs(30));
            continue;
        }
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        let body: Json = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }));
        return match status {
            200 | 202 => Ok((status, body)),
            401 | 403 if body.get("sqlState").is_none() => Err(Error::AuthFailed(super::message(&body, status))),
            _ => Err(Error::Query(super::message(&body, status))),
        };
    }
}

/// The statements of one load that may still run: the handles known, and
/// how many submits are still on their way (their handle not back yet).
#[derive(Clone, Default)]
pub(crate) struct Inflight(Arc<InflightState>);

#[derive(Default)]
pub(crate) struct InflightState {
    handles: Mutex<HashSet<String>>,
    submits: std::sync::atomic::AtomicUsize,
}

/// One submit on its way (counted until dropped).
struct Submitting(Inflight);

impl Drop for Submitting {
    fn drop(&mut self) {
        self.0 .0.submits.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Inflight {
    fn add(&self, h: &str) {
        if let Ok(mut g) = self.0.handles.lock() {
            g.insert(h.to_string());
        }
    }
    fn remove(&self, h: &str) {
        if let Ok(mut g) = self.0.handles.lock() {
            g.remove(h);
        }
    }
    fn submitting(&self) -> Submitting {
        self.0.submits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Submitting(self.clone())
    }
    pub(crate) fn take(&self) -> Vec<String> {
        self.0.handles.lock().map(|mut g| g.drain().collect()).unwrap_or_default()
    }
    /// Wait (up to `limit`) until every submit got its answer, so every
    /// statement that reached the server has its handle here.
    async fn settle_submits(&self, limit: Duration) {
        let end = tokio::time::Instant::now() + limit;
        while self.0.submits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if tokio::time::Instant::now() >= end {
                tracing::warn!("snowflake: un envío de la carga no respondió; si llegó al servidor, no se pudo cancelar");
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Submit one statement, then wait for its end; the final response.
///
/// The submit (`async=true`: the handle comes back at once) runs on its
/// own task, so if this future is dropped mid-request the answer is still
/// read and its handle registered in `inflight` (and in `session`, for the
/// session's interrupter): the clean-up waits for that and cancels it.
/// The handle stays registered unless the statement finished well: when
/// in doubt it gets cancelled.
async fn run(api: Api, body: Vec<u8>, inflight: Inflight, session: Option<Arc<Mutex<Option<String>>>>) -> Result<Json> {
    let submit = {
        let (api, inflight) = (api.clone(), inflight.clone());
        let on_its_way = inflight.submitting();
        tokio::spawn(async move {
            let req = api
                .http
                .post(format!("{}/api/v2/statements", api.base))
                .query(&[("async", "true")])
                .header("Content-Type", "application/json")
                .body(body);
            let r = call(&api, req).await;
            if let Some(h) = r.as_ref().ok().and_then(|(_, b)| b.get("statementHandle")?.as_str()) {
                inflight.add(h);
                if let Some(Ok(mut g)) = session.as_ref().map(|s| s.lock()) {
                    *g = Some(h.to_string());
                }
            }
            drop(on_its_way);
            r
        })
    };
    let (mut status, mut resp) = submit.await.map_err(|e| Error::State(format!("envío interrumpido: {e}")))??;
    let handle = resp.get("statementHandle").and_then(Json::as_str).map(str::to_string);
    let mut delay = Duration::from_millis(100);
    while status == 202 {
        let Some(h) = &handle else { break };
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(1));
        (status, resp) = call(&api, api.http.get(format!("{}/api/v2/statements/{h}", api.base))).await?;
    }
    if let Some(h) = &handle {
        inflight.remove(h);
    }
    Ok(resp)
}

/// `number of rows inserted` of an `INSERT`'s result, or `rows`.
fn inserted(resp: &Json, rows: usize) -> u64 {
    resp.pointer("/data/0/0").and_then(Json::as_str).and_then(|n| n.parse::<u64>().ok()).unwrap_or(rows as u64)
}

/// How long clean-up waits for submits still on their way (a request's
/// own timeout, plus 429 retries).
const SUBMIT_WAIT: Duration = Duration::from_secs(180);

/// Wait for the submits on their way, cancel the statements that may
/// still run, then drop the staging table.
async fn clean_up(api: Api, inflight: Inflight, drop_body: Vec<u8>) {
    inflight.settle_submits(SUBMIT_WAIT).await;
    for h in inflight.take() {
        let req = api.http.post(format!("{}/api/v2/statements/{h}/cancel", api.base)).json(&json!({}));
        if let Err(e) = call(&api, req).await {
            tracing::debug!("snowflake cancel {h}: {e}");
        }
    }
    let req = api.http.post(format!("{}/api/v2/statements", api.base)).header("Content-Type", "application/json").body(drop_body);
    if let Err(e) = call(&api, req).await {
        tracing::warn!("snowflake: no se pudo borrar la tabla de carga: {e}");
    }
}

/// Cleans up a load that didn't finish: awaited on an error, in the
/// background when the load's future is dropped (a cancel).
struct Guard {
    api: Api,
    inflight: Inflight,
    drop_body: Option<Vec<u8>>,
}

impl Guard {
    async fn finish(mut self) {
        if let Some(b) = self.drop_body.take() {
            clean_up(self.api.clone(), self.inflight.clone(), b).await;
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(b) = self.drop_body.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(clean_up(self.api.clone(), self.inflight.clone(), b));
            }
            Err(_) => tracing::warn!("snowflake: carga cancelada fuera del runtime; la tabla de carga la borra una carga posterior"),
        }
    }
}

/// Drop the DBine staging tables of loads into `table` that a killed
/// process left behind (untouched for [`STAGING_STALE_HOURS`]); a failure
/// here doesn't stop the load.
async fn drop_leftovers(s: &SnowflakeSession, is: &str, schema: &str, table: &str, full: &(dyn Fn(&str) -> String + Sync)) {
    let sql = format!(
        "SELECT table_name FROM {is}.TABLES WHERE table_schema = ? AND STARTSWITH(table_name, ?) AND comment = ? \
         AND last_altered < DATEADD('hour', -{STAGING_STALE_HOURS}, CURRENT_TIMESTAMP())"
    );
    let prefix = format!("{table}{STAGING_MARK}");
    let rows = match s.text_rows(&sql, &[schema, &prefix, STAGING_COMMENT]).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("snowflake: tablas de carga viejas de {table}: {e}");
            return;
        }
    };
    for name in rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).filter(|n| is_staging_of(table, n)) {
        let mut b = s.body(&format!("DROP TABLE IF EXISTS {}", full(&name)), None, false);
        b["parameters"] = load_parameters();
        if let Err(e) = s.api.post("/api/v2/statements", &b).await {
            tracing::debug!("snowflake: no se pudo borrar la tabla de carga vieja {name}: {e}");
        }
    }
}

pub(crate) async fn bulk_load(s: &SnowflakeSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if spec.columns.is_empty() {
        return Err(Error::Query("no hay columnas para cargar".into()));
    }
    let db = s.database()?;
    let schema = spec.table.schema().map(str::to_string).or_else(|| s.ctx.schema.clone()).unwrap_or_else(|| "PUBLIC".into());
    let is = format!("{}.INFORMATION_SCHEMA", qualified_name(Quote::Double, None, &db));
    let rows = s
        .text_rows(
            &format!(
                "SELECT column_name, data_type, character_maximum_length, numeric_precision, numeric_scale, is_identity
                 FROM {is}.COLUMNS WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position"
            ),
            &[&schema, &spec.table.name],
        )
        .await?;
    if rows.is_empty() {
        return Err(Error::Query(format!("no se encontró la tabla de destino «{schema}.{}»", spec.table.name)));
    }
    let full = |name: &str| format!("{}.{}", qualified_name(Quote::Double, None, &db), qualified_name(Quote::Double, Some(&schema), name));
    let table = full(&spec.table.name);
    let described = if rows.iter().any(|r| r.get(1).cloned().flatten().is_some_and(|t| needs_declared(&t))) {
        describe(s, &table).await.unwrap_or_else(|e| {
            tracing::debug!("snowflake describe {table}: {e}");
            HashMap::new()
        })
    } else {
        HashMap::new()
    };
    let catalog: Vec<Target> = rows
        .iter()
        .map(|r| {
            let g = |i: usize| r.get(i).cloned().flatten();
            let name = g(0).unwrap_or_default();
            let mut ty = g(1).unwrap_or_default();
            if needs_declared(&ty) {
                ty = declared_type(&name, &ty, described.get(&name).map(String::as_str))?;
            }
            Ok(Target::new(&name, &ty, g(2).as_deref(), g(3).as_deref(), g(4).as_deref(), g(5).as_deref() == Some("YES")))
        })
        .collect::<Result<_>>()?;
    let (targets, keep) = plan_targets(&catalog, &spec.columns, spec.keep_identity)?;
    let all = keep.len() == spec.columns.len();
    let names = targets.iter().map(|t| qualified_name(Quote::Double, None, &t.name)).collect::<Vec<_>>().join(", ");

    let rest = |sql: String| -> Json {
        let mut b = s.body("", None, false);
        b["statement"] = Json::String(sql);
        b["parameters"] = load_parameters();
        b
    };
    let body = |sql: &str| -> Result<Vec<u8>> { Ok(serde_json::to_vec(&rest(sql.to_string()))?) };
    let inflight = Inflight::default();

    drop_leftovers(s, &is, &schema, &spec.table.name, &full).await;
    // This load's own staging table: a new name, never an existing table.
    let stage_name = staging_name(&spec.table.name, &staging_suffix());
    let stage = full(&stage_name);
    let guard = Guard { api: s.api.clone(), inflight: inflight.clone(), drop_body: Some(body(&format!("DROP TABLE IF EXISTS {stage}"))?) };
    let create = format!(
        "CREATE TRANSIENT TABLE {stage} DATA_RETENTION_TIME_IN_DAYS = 0 COMMENT = '{STAGING_COMMENT}' \
         AS SELECT {names} FROM {table} LIMIT 0"
    );
    if let Err(e) = run(s.api.clone(), body(&create)?, inflight.clone(), None).await {
        guard.finish().await;
        return Err(match e {
            Error::Query(m) => {
                let privilege = m.to_ascii_lowercase().contains("privilege");
                Error::Query(format!(
                    "no se pudo crear la tabla transitoria de carga «{schema}.{stage_name}» (la carga pasa por ella para que la tabla \
                     quede entera o vacía{}): {m}",
                    if privilege { "; hace falta permiso CREATE TABLE en el esquema" } else { "" }
                ))
            }
            e => e,
        });
    }

    let mut insert = Insert::new(&stage, targets, spec.commit_rows, spec.commit_bytes);
    let mut running = tokio::task::JoinSet::new();
    let mut done: u64 = 0;

    // Wait for the oldest-finishing statement while `at_most` are running.
    async fn settle(running: &mut tokio::task::JoinSet<Result<u64>>, at_most: usize, done: &mut u64, progress: Progress<'_>) -> Result<()> {
        while running.len() > at_most {
            match running.join_next().await {
                Some(Ok(Ok(n))) => {
                    *done += n;
                    progress(*done);
                }
                Some(Ok(Err(e))) => return Err(e),
                Some(Err(e)) => return Err(Error::State(format!("carga interrumpida: {e}"))),
                None => break,
            }
        }
        Ok(())
    }
    let spawn = |running: &mut tokio::task::JoinSet<Result<u64>>, insert: &mut Insert| -> Result<()> {
        let taken = insert.take();
        let n = taken.rows;
        let (api, b, inf) = (s.api.clone(), taken.request(rest)?, inflight.clone());
        running.spawn(async move { run(api, b, inf, None).await.map(|r| inserted(&r, n)) });
        Ok(())
    };

    let r: Result<u64> = async {
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                let row: Vec<Cell> = if all {
                    row
                } else {
                    let mut row: Vec<Option<Cell>> = row.into_iter().map(Some).collect();
                    keep.iter().map(|&i| row.get_mut(i).and_then(Option::take).unwrap_or(Cell::Null)).collect()
                };
                let row = insert.bind(row);
                if !insert.fits(&row) {
                    settle(&mut running, IN_FLIGHT - 1, &mut done, progress).await?;
                    spawn(&mut running, &mut insert)?;
                }
                insert.push(row);
            }
        }
        if !insert.is_empty() {
            settle(&mut running, IN_FLIGHT - 1, &mut done, progress).await?;
            spawn(&mut running, &mut insert)?;
        }
        settle(&mut running, 0, &mut done, progress).await?;
        // The one statement that writes the target: all rows or none.
        let last = run(
            s.api.clone(),
            body(&format!("INSERT INTO {table} ({names}) SELECT {names} FROM {stage}"))?,
            inflight.clone(),
            Some(s.handle.clone()),
        )
        .await;
        s.set_handle(None);
        Ok(inserted(&last?, done as usize))
    }
    .await;
    running.abort_all();
    guard.finish().await;
    let n = r?;
    progress(n);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn col(t: &str, scale: i64) -> Json {
        json!({ "name": "C", "type": t, "scale": scale, "precision": 38, "length": 16, "nullable": false })
    }

    #[test]
    fn reads_every_type_without_loss() {
        let r = |t: &str, scale: i64, v: Json| read_cell(read_kind(&col(t, scale)), &v).unwrap();
        assert_eq!(r("fixed", 0, json!("42")), Cell::Int(42));
        assert_eq!(r("fixed", 0, json!("-9223372036854775808")), Cell::Int(i64::MIN));
        assert_eq!(r("fixed", 0, json!("99999999999999999999999999999999999999")), Cell::Decimal("99999999999999999999999999999999999999".into()));
        assert_eq!(r("fixed", 4, json!("-12.3400")), Cell::Decimal("-12.3400".into()));
        assert_eq!(r("real", 0, json!("2.5")), Cell::Float(2.5));
        assert!(matches!(r("real", 0, json!("NaN")), Cell::Float(f) if f.is_nan()));
        assert_eq!(r("real", 0, json!("-inf")), Cell::Float(f64::NEG_INFINITY));
        assert_eq!(r("boolean", 0, json!("true")), Cell::Bool(true));
        assert_eq!(r("boolean", 0, json!("false")), Cell::Bool(false));
        assert_eq!(r("date", 0, json!("19753")), Cell::Date("2024-01-31".into()));
        assert_eq!(r("date", 0, json!("-1")), Cell::Date("1969-12-31".into()));
        assert_eq!(r("time", 9, json!("49500.250000000")), Cell::Time("13:45:00.25".into()));
        assert_eq!(r("time", 9, json!("49500.000000001")), Cell::Time("13:45:00.000000001".into()));
        assert_eq!(r("time", 0, json!("0")), Cell::Time("00:00:00".into()));
        assert_eq!(r("timestamp_ntz", 9, json!("1706708700.123456789")), Cell::DateTime("2024-01-31 13:45:00.123456789".into()));
        assert_eq!(r("timestamp_ntz", 9, json!("-0.500000000")), Cell::DateTime("1969-12-31 23:59:59.5".into()));
        assert_eq!(r("timestamp_ltz", 9, json!("1706708700.000000000")), Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()));
        assert_eq!(r("timestamp_tz", 9, json!("1706708700.100000000 1260")), Cell::DateTimeTz("2024-01-31 10:45:00.1-03:00".into()));
        assert_eq!(r("timestamp_tz", 9, json!("1706708700.000000000 1800")), Cell::DateTimeTz("2024-01-31 19:45:00+06:00".into()));
        assert_eq!(r("binary", 0, json!("CAFE00")), Cell::Bytes(vec![0xCA, 0xFE, 0]));
        assert_eq!(r("variant", 0, json!("{\n  \"a\": 1\n}")), Cell::Json("{\"a\":1}".into()));
        assert_eq!(r("array", 0, json!("[\n  1,\n  \"x\"\n]")), Cell::Json("[1,\"x\"]".into()));
        assert_eq!(r("variant", 0, json!("\"txt\"")), Cell::Json("\"txt\"".into()));
        assert_eq!(r("text", 0, json!("hola")), Cell::Text("hola".into()));
        assert_eq!(r("geography", 0, json!("{\"type\":\"Point\"}")), Cell::Text("{\"type\":\"Point\"}".into()));
        for t in ["fixed", "real", "text", "binary", "date", "variant", "timestamp_tz"] {
            assert_eq!(r(t, 0, Json::Null), Cell::Null);
        }
        // Unparseable values keep the server's text.
        assert_eq!(r("binary", 0, json!("XYZ")), Cell::Text("XYZ".into()));
    }

    #[test]
    fn large_binaries_read_whole() {
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(read_cell(Read::Bytes, &json!(hex(&big))).unwrap(), Cell::Bytes(big));
    }

    #[test]
    fn columns_spell_snowflake_types() {
        let c = |t: &str, scale: i64| transfer_column(&col(t, scale)).type_name;
        assert_eq!(c("fixed", 2), "NUMBER(38,2)");
        assert_eq!(c("text", 0), "VARCHAR(16)");
        assert_eq!(c("binary", 0), "BINARY(16)");
        assert_eq!(c("timestamp_tz", 3), "TIMESTAMP_TZ(3)");
        assert_eq!(c("real", 0), "FLOAT");
        assert_eq!(c("variant", 0), "VARIANT");
        let t = transfer_column(&col("date", 0));
        assert_eq!((t.name.as_str(), t.nullable), ("C", false));
    }

    #[test]
    fn select_reads_only_what_is_asked() {
        let table = ObjectRef { kind: "table".into(), schema: Some("S".into()), name: "T x".into() };
        let spec = ReadSpec { table: table.clone(), columns: None, filter: None };
        assert_eq!(select_sql(&spec), "SELECT * FROM \"S\".\"T x\"");
        let spec = ReadSpec { table, columns: Some(vec!["A".into(), "b\"c".into()]), filter: Some(" \"A\" > 5 ".into()) };
        assert_eq!(select_sql(&spec), "SELECT \"A\", \"b\"\"c\" FROM \"S\".\"T x\" WHERE \"A\" > 5");
    }

    fn targets() -> Vec<Target> {
        vec![
            Target::new("ID", "NUMBER", None, Some("38"), Some("0"), true),
            Target::new("NAME", "TEXT", Some("100"), None, None, false),
            Target::new("AMOUNT", "NUMBER", None, Some("20"), Some("4"), false),
            Target::new("BIN", "BINARY", Some("8388608"), None, None, false),
            Target::new("DOC", "VARIANT", None, None, None, false),
            Target::new("OBJ", "OBJECT", None, None, None, false),
            Target::new("TS", "TIMESTAMP_TZ", None, None, None, false),
            Target::new("OK", "BOOLEAN", None, None, None, false),
            Target::new("V", "VECTOR(FLOAT, 3)", None, None, None, false),
            Target::new("F", "FLOAT", None, None, None, false),
        ]
    }

    #[test]
    fn target_conversions() {
        let t = targets();
        let exprs: Vec<String> = t.iter().enumerate().map(|(i, t)| t.expr(i + 1)).collect();
        assert_eq!(
            exprs,
            vec![
                "column1::NUMBER(38,0)",
                "column2",
                "column3::NUMBER(20,4)",
                "TO_BINARY(column4, 'HEX')",
                "PARSE_JSON(column5)",
                "PARSE_JSON(column6)::OBJECT",
                "TO_TIMESTAMP_TZ(column7, 'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM')::TIMESTAMP_TZ",
                "column8::BOOLEAN",
                "PARSE_JSON(column9)::ARRAY::VECTOR(FLOAT, 3)",
                "column10::FLOAT",
            ]
        );
    }

    #[test]
    fn bound_text_per_target() {
        let b = |c: Cell, l: &Load| bind_text(c, l);
        assert_eq!(b(Cell::Null, &Load::Text), None);
        assert_eq!(b(Cell::Bool(true), &Load::Number).as_deref(), Some("1"));
        assert_eq!(b(Cell::Bool(false), &Load::Bool).as_deref(), Some("false"));
        assert_eq!(b(Cell::Int(-7), &Load::Number).as_deref(), Some("-7"));
        assert_eq!(b(Cell::UInt(u64::MAX), &Load::Number).as_deref(), Some("18446744073709551615"));
        assert_eq!(b(Cell::Float(0.1), &Load::Number).as_deref(), Some("0.1"));
        assert_eq!(b(Cell::Float(f64::NAN), &Load::Number).as_deref(), Some("NaN"));
        assert_eq!(b(Cell::Float(f64::INFINITY), &Load::Json(None)).as_deref(), Some("\"inf\""));
        assert_eq!(b(Cell::Decimal("123456789012345678901234.5678".into()), &Load::Number).as_deref(), Some("123456789012345678901234.5678"));
        assert_eq!(b(Cell::Bytes(vec![0, 0xAB]), &Load::Binary).as_deref(), Some("00AB"));
        assert_eq!(b(Cell::Bytes(vec![0xAB]), &Load::Json(None)).as_deref(), Some("\"AB\""));
        assert_eq!(b(Cell::Bytes(vec![0xAB]), &Load::Text).as_deref(), Some("AB"));
        assert_eq!(b(Cell::Text("0xCAFE".into()), &Load::Binary).as_deref(), Some("CAFE"));
        assert_eq!(b(Cell::Text("hi".into()), &Load::Binary).as_deref(), Some("6869"));
        assert_eq!(b(Cell::Text("{\"a\":1}".into()), &Load::Json(None)).as_deref(), Some("{\"a\":1}"));
        assert_eq!(b(Cell::Text("say \"hi\"".into()), &Load::Json(None)).as_deref(), Some("\"say \\\"hi\\\"\""));
        assert_eq!(b(Cell::Text("{broken".into()), &Load::Json(None)).as_deref(), Some("\"{broken\""));
        assert_eq!(b(Cell::Json("[1,2]".into()), &Load::Json(Some("ARRAY".into()))).as_deref(), Some("[1,2]"));
        assert_eq!(b(Cell::Json("{\"a\":1}".into()), &Load::Text).as_deref(), Some("{\"a\":1}"));
        assert_eq!(b(Cell::Date("2024-01-31".into()), &Load::Cast).as_deref(), Some("2024-01-31"));
        assert_eq!(b(Cell::DateTimeTz("2024-01-31 10:45:00-03:00".into()), &Load::Json(None)).as_deref(), Some("\"2024-01-31 10:45:00-03:00\""));
        assert_eq!(b(Cell::Uuid("6f1c…".into()), &Load::Text).as_deref(), Some("6f1c…"));
        assert_eq!(b(Cell::Text("línea 'a'\n".into()), &Load::Text).as_deref(), Some("línea 'a'\n"));
    }

    #[test]
    fn insert_statement_with_binds_and_nulls() {
        let t = vec![Target::new("A", "NUMBER", None, Some("38"), Some("0"), false), Target::new("B b", "BINARY", None, None, None, false)];
        let mut ins = Insert::new("\"DB\".\"S\".\"T\"", t, 100, 1 << 20);
        let r = ins.bind(vec![Cell::Int(1), Cell::Bytes(vec![1, 2])]);
        ins.push(r);
        let r = ins.bind(vec![Cell::Null, Cell::Null]);
        ins.push(r);
        let r = ins.bind(vec![Cell::Int(3), Cell::Null]);
        ins.push(r);
        let t = ins.take();
        let (sql, binds, rows) = (t.sql.clone(), t.bindings(), t.rows);
        assert_eq!(
            sql,
            "INSERT INTO \"DB\".\"S\".\"T\" (\"A\", \"B b\") SELECT column1::NUMBER(38,0), TO_BINARY(column2, 'HEX') FROM VALUES (?, ?), (NULL, NULL), (?, NULL)"
        );
        assert_eq!(rows, 3);
        assert_eq!(
            binds,
            json!({
                "1": { "type": "TEXT", "value": "1" },
                "2": { "type": "TEXT", "value": "0102" },
                "3": { "type": "TEXT", "value": "3" },
            })
        );
        // Taken: it starts over.
        assert!(ins.is_empty());
        let r = ins.bind(vec![Cell::Int(9), Cell::Null]);
        ins.push(r);
        let t = ins.take();
        let (sql, binds) = (t.sql.clone(), t.bindings());
        assert!(sql.ends_with("FROM VALUES (?, NULL)"), "{sql}");
        assert_eq!(binds, json!({ "1": { "type": "TEXT", "value": "9" } }));
    }

    #[test]
    fn statements_stay_within_their_limits() {
        let t = vec![Target::new("A", "NUMBER", None, Some("38"), Some("0"), false)];
        // By the commit window's rows.
        let mut ins = Insert::new("t", t.clone(), 10, u64::MAX);
        for i in 0..10 {
            assert!(ins.fits(&ins.bind(vec![Cell::Int(i)])));
            let r = ins.bind(vec![Cell::Int(i)]);
            ins.push(r);
        }
        assert!(!ins.fits(&ins.bind(vec![Cell::Int(10)])));
        // By the VALUES limit, whatever the window.
        let mut ins = Insert::new("t", t.clone(), u64::MAX, u64::MAX);
        let mut n = 0;
        while ins.fits(&ins.bind(vec![Cell::Int(1)])) {
            let r = ins.bind(vec![Cell::Int(1)]);
            ins.push(r);
            n += 1;
        }
        assert_eq!(n, STMT_ROWS);
        // By bytes: large binaries.
        let tb = vec![Target::new("B", "BINARY", None, None, None, false)];
        let mut ins = Insert::new("t", tb.clone(), u64::MAX, u64::MAX);
        let big = Cell::Bytes(vec![7; 1024 * 1024]);
        let mut n = 0;
        while ins.fits(&ins.bind(vec![big.clone()])) {
            let r = ins.bind(vec![big.clone()]);
            ins.push(r);
            n += 1;
        }
        assert!(n >= 2 && ins.request_bytes() <= REQUEST_BYTES, "{n} {}", ins.request_bytes());
        let body = ins.take().request(|sql| json!({ "statement": sql })).unwrap();
        assert!(body.len() <= REQUEST_BYTES, "{}", body.len());
        // A value larger than a request still goes (alone).
        let mut ins = Insert::new("t", tb, u64::MAX, 1024);
        assert!(ins.fits(&ins.bind(vec![big.clone()])));
        let r = ins.bind(vec![big.clone()]);
        ins.push(r);
        assert!(!ins.fits(&ins.bind(vec![big.clone()])));
        // By statement text: many columns.
        let wide: Vec<Target> = (0..400).map(|i| Target::new(&format!("C{i}"), "TEXT", None, None, None, false)).collect();
        let mut ins = Insert::new("t", wide, u64::MAX, u64::MAX);
        let row: Vec<Cell> = (0..400).map(|_| Cell::Null).collect();
        while ins.fits(&ins.bind(row.clone())) {
            let r = ins.bind(row.clone());
            ins.push(r);
        }
        let t = ins.take();
        let (sql, rows) = (t.sql, t.rows);
        assert!(sql.len() <= STMT_TEXT + 400 * 6 + 4 && rows < STMT_ROWS, "{} {rows}", sql.len());
    }

    #[test]
    fn identity_columns_and_names() {
        let catalog = targets();
        let names: Vec<String> = ["id", "NAME", "DOC"].iter().map(|s| s.to_string()).collect();
        let (t, keep) = plan_targets(&catalog, &names, false).unwrap();
        assert_eq!(t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["NAME", "DOC"]);
        assert_eq!(keep, vec![1, 2]);
        let (t, keep) = plan_targets(&catalog, &names, true).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(keep, vec![0, 1, 2]);
        assert!(matches!(plan_targets(&catalog, &["NOPE".to_string()], true), Err(Error::Query(m)) if m.contains("NOPE")));
        // An exact-case match wins over a case-insensitive one.
        let two = vec![Target::new("a", "TEXT", None, None, None, false), Target::new("A", "NUMBER", None, Some("38"), Some("0"), false)];
        let (t, _) = plan_targets(&two, &["A".to_string()], true).unwrap();
        assert_eq!(t[0].load, Load::Number);
    }

    #[test]
    fn variant_numbers_keep_every_digit() {
        let r = |v: &str| read_cell(Read::Json, &json!(v)).unwrap();
        assert_eq!(r("{\n  \"id\": 123456789012345678901\n}"), Cell::Json("{\"id\":123456789012345678901}".into()));
        assert_eq!(r("{\"p\": 0.12345678901234567890}"), Cell::Json("{\"p\":0.12345678901234567890}".into()));
        assert_eq!(r("[1e400, -0.0]"), Cell::Json("[1e400,-0.0]".into()));
        // Whitespace inside strings (and escaped quotes) stays.
        assert_eq!(r("{ \"a b\" : \"x \\\" y\" }"), Cell::Json("{\"a b\":\"x \\\" y\"}".into()));
        assert_eq!(r("{broken"), Cell::Text("{broken".into()));
    }

    #[test]
    fn unknown_boolean_text_is_kept() {
        let r = |v: &str| read_cell(Read::Bool, &json!(v)).unwrap();
        assert_eq!(r("True"), Cell::Bool(true));
        assert_eq!(r("0"), Cell::Bool(false));
        assert_eq!(r("FALSE"), Cell::Bool(false));
        assert_eq!(r("maybe"), Cell::Text("maybe".into()));
    }

    #[test]
    fn temporal_loads_never_depend_on_input_formats() {
        // Only TIMEZONE (an accepted SQL API parameter): no *_INPUT_FORMAT.
        assert_eq!(load_parameters(), json!({ "TIMEZONE": "UTC" }));
        let e = |ty: &str| Target::new("C", ty, None, None, None, false).expr(1);
        assert_eq!(e("DATE"), "TO_DATE(column1, 'YYYY-MM-DD')");
        assert_eq!(e("TIME(9)"), "TO_TIME(column1, 'HH24:MI:SS.FF9')::TIME(9)");
        assert_eq!(e("TIMESTAMP_NTZ(9)"), "TO_TIMESTAMP_NTZ(column1, 'YYYY-MM-DD HH24:MI:SS.FF9')::TIMESTAMP_NTZ(9)");
        assert_eq!(e("TIMESTAMP_LTZ(9)"), "TO_TIMESTAMP_TZ(column1, 'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM')::TIMESTAMP_LTZ(9)");
        assert_eq!(e("TIMESTAMP_TZ(9)"), "TO_TIMESTAMP_TZ(column1, 'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM')::TIMESTAMP_TZ(9)");
        // Every value in the one form its format reads.
        let b = |c: Cell, t: Temporal| bind_text(c, &Load::Temporal(t)).unwrap();
        assert_eq!(b(Cell::Date("2024-02-29".into()), Temporal::Date), "2024-02-29");
        assert_eq!(b(Cell::Date("2024-02-29".into()), Temporal::Ntz), "2024-02-29 00:00:00.000000000");
        assert_eq!(b(Cell::Time("13:45:00".into()), Temporal::Time), "13:45:00.000000000");
        assert_eq!(b(Cell::Time("13:45:00.123456789".into()), Temporal::Time), "13:45:00.123456789");
        assert_eq!(b(Cell::DateTime("2024-01-31 13:45:00.5".into()), Temporal::Ntz), "2024-01-31 13:45:00.500000000");
        assert_eq!(b(Cell::DateTime("2024-01-31T13:45:00".into()), Temporal::Date), "2024-01-31");
        assert_eq!(b(Cell::DateTime("2024-01-31 13:45:00.5".into()), Temporal::Time), "13:45:00.500000000");
        // No offset: UTC; an offset is kept as is.
        assert_eq!(b(Cell::DateTime("2024-01-31 13:45:00".into()), Temporal::Zoned), "2024-01-31 13:45:00.000000000 +00:00");
        assert_eq!(b(Cell::DateTimeTz("2024-01-31 10:45:00.25-03:00".into()), Temporal::Zoned), "2024-01-31 10:45:00.250000000 -03:00");
        assert_eq!(b(Cell::DateTimeTz("2024-01-31 19:45:00+0530".into()), Temporal::Zoned), "2024-01-31 19:45:00.000000000 +05:30");
        assert_eq!(b(Cell::Text("2024-01-31T13:45:00Z".into()), Temporal::Zoned), "2024-01-31 13:45:00.000000000 +00:00");
        assert_eq!(b(Cell::DateTimeTz("0001-01-01 00:00:00+00:00".into()), Temporal::Zoned), "0001-01-01 00:00:00.000000000 +00:00");
        // Not a temporal text: it goes as is, and Snowflake's error names it.
        assert_eq!(b(Cell::Text("no es fecha".into()), Temporal::Date), "no es fecha");
        assert_eq!(b(Cell::Time("13:45:00".into()), Temporal::Date), "13:45:00");
        assert_eq!(bind_text(Cell::Null, &Load::Temporal(Temporal::Date)), None);
    }

    #[test]
    fn variant_non_json_tokens_are_refused() {
        let r = |v: &str| read_cell(Read::Json, &json!(v));
        for (v, token) in [("[1, undefined, 3]", "undefined"), ("{\"a\": NaN}", "NaN"), ("[Infinity]", "Infinity"), ("[-Infinity, 1]", "Infinity")] {
            assert!(matches!(r(v), Err(Error::Unsupported(m)) if m.contains(token)), "{v}: {:?}", r(v));
        }
        // The same words inside strings are just text.
        assert_eq!(r("[\"undefined\", \"NaN\"]").unwrap(), Cell::Json("[\"undefined\",\"NaN\"]".into()));
        // Not even JSON with them: kept as text (never a JSON document).
        assert_eq!(r("undefinedx {").unwrap(), Cell::Text("undefinedx {".into()));
        assert_eq!(non_json_token("[1, null]"), None);
    }

    #[test]
    fn request_size_counts_control_characters() {
        let nasty: String = (0u8..0x20).map(char::from).chain("\"\\é".chars()).collect();
        assert_eq!(json_len(&nasty), Json::String(nasty.clone()).to_string().len());
        // A statement full of control characters stays within a request.
        let t = vec![Target::new("T", "TEXT", None, None, None, false), Target::new("J", "VARIANT", None, None, None, false)];
        let mut ins = Insert::new("t", t, u64::MAX, u64::MAX);
        let cell = Cell::Text("\u{1}".repeat(64 * 1024));
        let row = || vec![cell.clone(), cell.clone()];
        while ins.fits(&ins.bind(row())) {
            let r = ins.bind(row());
            ins.push(r);
        }
        let t = ins.take();
        let rows = t.rows;
        let body = t.request(|sql| json!({ "statement": sql, "timeout": 0 })).unwrap();
        serde_json::from_slice::<Json>(&body).unwrap();
        assert!(rows > 1 && body.len() <= REQUEST_BYTES, "{rows} {}", body.len());
    }

    #[test]
    fn parameterized_types_are_declared_whole() {
        assert_eq!(declared_type("V", "VECTOR", Some("VECTOR(FLOAT, 3)")).unwrap(), "VECTOR(FLOAT, 3)");
        assert_eq!(declared_type("M", "map", Some("MAP(VARCHAR(16777216), NUMBER(38,0))")).unwrap(), "MAP(VARCHAR(16777216), NUMBER(38,0))");
        assert_eq!(declared_type("O", "OBJECT", Some("OBJECT(city VARCHAR)")).unwrap(), "OBJECT(city VARCHAR)");
        assert_eq!(declared_type("O", "OBJECT", None).unwrap(), "OBJECT");
        // A bare VECTOR / MAP is no type: refused, never passed on.
        assert!(matches!(declared_type("V", "VECTOR", None), Err(Error::Unsupported(m)) if m.contains("«V»")));
        assert!(matches!(declared_type("V", "VECTOR", Some("VECTOR")), Err(Error::Unsupported(_))));
        // Loads cast to the whole type (structured names keep their case).
        let t = Target::new("M", "MAP(VARCHAR, NUMBER(38,0))", None, None, None, false);
        assert_eq!(t.expr(1), "PARSE_JSON(column1)::MAP(VARCHAR, NUMBER(38,0))");
        let t = Target::new("O", "OBJECT(city VARCHAR)", None, None, None, false);
        assert_eq!(t.expr(1), "PARSE_JSON(column1)::OBJECT(city VARCHAR)");
    }

    // ---- memory of the statement being filled

    /// Live heap bytes of this thread, and their peak (tests only).
    struct Counting;

    thread_local! {
        static HEAP: std::cell::Cell<(isize, isize)> = const { std::cell::Cell::new((0, 0)) };
    }

    fn heap_add(d: isize) {
        let _ = HEAP.try_with(|h| {
            let (live, peak) = h.get();
            h.set((live + d, peak.max(live + d)));
        });
    }

    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
            heap_add(l.size() as isize);
            unsafe { std::alloc::System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
            heap_add(-(l.size() as isize));
            unsafe { std::alloc::System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
            heap_add(new as isize - l.size() as isize);
            unsafe { std::alloc::System.realloc(p, l, new) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// Peak heap growth on this thread while `f` runs.
    fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let base = HEAP.with(|h| {
            let (live, _) = h.get();
            h.set((live, live));
            live
        });
        let r = f();
        let peak = HEAP.with(|h| h.get().1);
        (r, (peak - base).max(0) as usize)
    }

    #[test]
    fn a_statement_being_filled_holds_about_its_request() {
        // The verifier's case: 10 NUMBER columns of small ints, filled
        // until it's full (it held 52 MiB as serde_json values).
        let t: Vec<Target> = (0..10).map(|i| Target::new(&format!("C{i}"), "NUMBER", None, Some("38"), Some("0"), false)).collect();
        let ((rows, body), peak) = peak_of(|| {
            let mut ins = Insert::new("\"DB\".\"S\".\"T\"", t, u64::MAX, u64::MAX);
            let mut i = 0i64;
            loop {
                let row = ins.bind((0..10).map(|c| Cell::Int(i * 10 + c)).collect());
                if !ins.fits(&row) {
                    break;
                }
                ins.push(row);
                i += 1;
            }
            let taken = ins.take();
            let rows = taken.rows;
            (rows, taken.request(|sql| json!({ "statement": sql, "timeout": 0, "parameters": load_parameters() })).unwrap())
        });
        let v: Json = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["bindings"].as_object().unwrap().len(), rows * 10);
        assert_eq!(v["bindings"]["1"], json!({ "type": "TEXT", "value": "0" }));
        assert!(body.len() <= REQUEST_BYTES, "{}", body.len());
        // Held while filling: the request's buffer and the statement text
        // (both sized once), plus the statement's JSON while the request
        // is finished; never several times the request.
        let bound = REQUEST_BYTES + 2 * STMT_TEXT + 256 * 1024;
        assert!(peak <= bound, "pico {peak} > {bound} ({rows} filas, cuerpo {})", body.len());
        println!("{rows} filas, cuerpo {} B, pico {peak} B", body.len());
    }

    // ---- a fake SQL API, to see what a failed or cancelled load leaves

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const STAGING: &str = "\"DB\".\"PUBLIC\".\"T__DBINE_CARGA_";
    const FINAL: &str = "INSERT INTO \"DB\".\"PUBLIC\".\"T\" (";

    #[derive(Default)]
    struct Fake {
        /// `(method, path, statement)` of every call.
        calls: Mutex<Vec<(String, String, String)>>,
        inserts: AtomicUsize,
        /// The staging insert (1-based) whose poll fails; 0: none.
        fail_insert: usize,
        /// Staging inserts finish (else they run forever).
        finish: bool,
        /// The final statement's submit takes a while (handle `fin`).
        slow_final: bool,
        final_seen: tokio::sync::Notify,
        /// Tables the leftovers query finds.
        leftovers: Vec<&'static str>,
    }

    impl Fake {
        /// Status, body, and how long to wait before answering.
        fn answer(&self, method: &str, path: &str, body: &str) -> (u16, Json, u64) {
            let stmt = serde_json::from_str::<Json>(body).ok().and_then(|b| b["statement"].as_str().map(str::to_string)).unwrap_or_default();
            self.calls.lock().unwrap().push((method.into(), path.into(), stmt.clone()));
            let path = path.split('?').next().unwrap_or("");
            if method == "POST" && path.ends_with("/cancel") {
                return (200, json!({}), 0);
            }
            if method == "GET" {
                let h = path.rsplit('/').next().unwrap_or("");
                if h == format!("h{}", self.fail_insert) {
                    return (422, json!({ "message": "falló", "sqlState": "22018" }), 0);
                }
                if self.finish && h != "fin" {
                    return (200, json!({ "statementHandle": h, "data": [["10"]] }), 0);
                }
                // Still running.
                return (202, json!({ "statementHandle": h }), 0);
            }
            if stmt.starts_with("SELECT column_name") {
                return (200, json!({ "resultSetMetaData": { "rowType": [] }, "data": [["A", "NUMBER", null, "38", "0", "NO"]] }), 0);
            }
            if stmt.starts_with("SELECT table_name") {
                let data: Vec<Json> = self.leftovers.iter().map(|t| json!([t])).collect();
                return (200, json!({ "resultSetMetaData": { "rowType": [] }, "data": data }), 0);
            }
            if stmt.starts_with(&format!("INSERT INTO {STAGING}")) {
                let n = self.inserts.fetch_add(1, Ordering::SeqCst) + 1;
                return (202, json!({ "statementHandle": format!("h{n}") }), 0);
            }
            if self.slow_final && stmt.starts_with(FINAL) {
                self.final_seen.notify_one();
                return (202, json!({ "statementHandle": "fin" }), 1000);
            }
            (200, json!({ "statementHandle": "x", "data": [["1"]] }), 0)
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    async fn serve(fake: Arc<Fake>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let fake = fake.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string());
                    };
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                    let (status, json, wait) = fake.answer(method, path, &body);
                    if wait > 0 {
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                    }
                    let text = json.to_string();
                    let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn session(base: String) -> SnowflakeSession {
        SnowflakeSession {
            api: Api { http: reqwest::Client::new(), base, auth: super::super::Auth::Pat("t".into()), last_error: Default::default() },
            ctx: super::super::Context { database: Some("DB".into()), schema: Some("PUBLIC".into()), ..Default::default() },
            carry: Default::default(),
            handle: Arc::new(Mutex::new(None)),
            mon: Default::default(),
            profiler: None,
        }
    }

    struct Ints(i64);

    #[dbine_driver::async_trait]
    impl BatchSource for Ints {
        async fn next(&mut self) -> Option<dbine_driver::transfer::RowBatch> {
            if self.0 >= 200 {
                return None;
            }
            let rows: Vec<Vec<Cell>> = (self.0..self.0 + 50).map(|i| vec![Cell::Int(i)]).collect();
            self.0 += 50;
            Some(dbine_driver::transfer::RowBatch { bytes: rows.len() * 24, rows })
        }
    }

    fn spec() -> LoadSpec {
        LoadSpec {
            table: ObjectRef { kind: "table".into(), schema: Some("PUBLIC".into()), name: "T".into() },
            columns: vec!["A".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows: 10,
            commit_bytes: u64::MAX,
        }
    }

    /// The staging table a load's statement names (`"T__DBINE_CARGA_…"`).
    fn staging_in(stmt: &str) -> Option<String> {
        let i = stmt.find(STAGING)?;
        let rest = &stmt[i + STAGING.len()..];
        Some(format!("T__DBINE_CARGA_{}", &rest[..rest.find('"')?]))
    }

    /// What a load must have done when it ends without success: every
    /// staging insert that started got a cancel, the staging table was
    /// dropped, and the target was never written.
    fn assert_cleaned(calls: &[(String, String, String)]) {
        let started: Vec<String> = calls
            .iter()
            .filter(|c| c.2.starts_with(&format!("INSERT INTO {STAGING}")))
            .enumerate()
            .map(|(i, _)| format!("h{}", i + 1))
            .collect();
        assert!(!started.is_empty());
        for h in &started {
            let cancelled = calls.iter().any(|c| c.1.ends_with(&format!("/statements/{h}/cancel")));
            let failed = calls.iter().any(|c| c.0 == "GET" && c.1.ends_with(&format!("/{h}"))) && h == "h2";
            assert!(cancelled || failed, "{h} ni cancelada ni fallida: {calls:?}");
        }
        let created = calls.iter().find_map(|c| c.2.starts_with("CREATE").then(|| staging_in(&c.2)).flatten()).expect("CREATE");
        assert!(calls.iter().any(|c| c.2 == format!("DROP TABLE IF EXISTS \"DB\".\"PUBLIC\".\"{created}\"")), "{calls:?}");
        assert!(!calls.iter().any(|c| c.2.starts_with(FINAL)), "{calls:?}");
        // Every statement went async.
        assert!(calls.iter().filter(|c| c.2.starts_with("INSERT")).all(|c| c.1.contains("async=true")));
    }

    #[tokio::test]
    async fn failed_load_cancels_what_runs_and_never_touches_the_target() {
        let fake = Arc::new(Fake { fail_insert: 2, ..Default::default() });
        let s = session(serve(fake.clone()).await);
        let r = bulk_load(&s, &spec(), &mut Ints(0), &|_| {}).await;
        assert!(matches!(&r, Err(Error::Query(m)) if m.contains("falló")), "{r:?}");
        // All of it happened before returning.
        let calls = fake.calls();
        assert_cleaned(&calls);
        assert!(calls.iter().any(|c| c.1.ends_with("/statements/h1/cancel")));
    }

    #[tokio::test]
    async fn dropped_load_cancels_in_the_background() {
        let fake = Arc::new(Fake::default());
        let s = session(serve(fake.clone()).await);
        let r = tokio::time::timeout(Duration::from_millis(800), bulk_load(&s, &spec(), &mut Ints(0), &|_| {})).await;
        assert!(r.is_err(), "the fake never finishes a staging insert");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_cleaned(&fake.calls());
    }

    /// Problem: a drop during the final statement's submit, before its
    /// handle came back, left it running (and committing) after the load
    /// ended. The submit now finishes on its own task and gets cancelled.
    #[tokio::test]
    async fn a_load_dropped_during_the_final_submit_still_cancels_it() {
        let fake = Arc::new(Fake { finish: true, slow_final: true, ..Default::default() });
        let s = session(serve(fake.clone()).await);
        let (sp, mut src) = (spec(), Ints(0));
        tokio::select! {
            r = bulk_load(&s, &sp, &mut src, &|_| {}) => panic!("the final statement never ends: {r:?}"),
            _ = fake.final_seen.notified() => {}
        }
        // The load's future is gone mid-submit; the answer (1 s later)
        // still gets its cancel, and only then the staging table goes.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let calls = fake.calls();
        let cancel = calls.iter().position(|c| c.1.ends_with("/statements/fin/cancel")).unwrap_or_else(|| panic!("sin cancelar: {calls:?}"));
        let dropped = calls.iter().position(|c| c.2.starts_with("DROP TABLE IF EXISTS")).expect("DROP");
        assert!(cancel < dropped, "{calls:?}");
    }

    /// Problem: every load into a table used the same staging table, so two
    /// loads at once (other windows, users or machines) replaced and
    /// dropped each other's. Each load now has its own, never replaces a
    /// table, and only touches its own.
    #[tokio::test]
    async fn two_loads_into_one_table_never_share_a_staging_table() {
        let fake = Arc::new(Fake { finish: true, ..Default::default() });
        let base = serve(fake.clone()).await;
        let (s1, s2) = (session(base.clone()), session(base));
        let (sp, mut src1, mut src2) = (spec(), Ints(0), Ints(0));
        let (a, b) = tokio::join!(bulk_load(&s1, &sp, &mut src1, &|_| {}), bulk_load(&s2, &sp, &mut src2, &|_| {}));
        assert_eq!((a.unwrap(), b.unwrap()), (1, 1));
        let calls = fake.calls();
        let created: Vec<String> = calls.iter().filter(|c| c.2.starts_with("CREATE")).filter_map(|c| staging_in(&c.2)).collect();
        assert_eq!(created.len(), 2, "{calls:?}");
        assert_ne!(created[0], created[1]);
        assert!(created.iter().all(|t| is_staging_of("T", t)), "{created:?}");
        assert!(calls.iter().filter(|c| c.2.starts_with("CREATE")).all(|c| c.2.starts_with("CREATE TRANSIENT TABLE ")), "nunca OR REPLACE");
        // Each final statement reads one staging table, each one dropped once.
        let finals: Vec<String> = calls.iter().filter(|c| c.2.starts_with(FINAL)).filter_map(|c| staging_in(&c.2)).collect();
        let mut sorted = finals.clone();
        sorted.sort();
        let mut want = created.clone();
        want.sort();
        assert_eq!(sorted, want);
        for t in &created {
            assert_eq!(calls.iter().filter(|c| c.2 == format!("DROP TABLE IF EXISTS \"DB\".\"PUBLIC\".\"{t}\"")).count(), 1, "{t}");
        }
    }

    #[tokio::test]
    async fn only_stale_dbine_staging_tables_of_the_target_are_dropped() {
        let fake = Arc::new(Fake {
            finish: true,
            leftovers: vec!["T__DBINE_CARGA_0123456789ABCDEF", "T__DBINE_CARGA_otra", "TX__DBINE_CARGA_0123456789ABCDEF"],
            ..Default::default()
        });
        let s = session(serve(fake.clone()).await);
        bulk_load(&s, &spec(), &mut Ints(0), &|_| {}).await.unwrap();
        let calls = fake.calls();
        let query = calls.iter().find(|c| c.2.starts_with("SELECT table_name")).expect("leftovers query");
        assert!(query.2.contains("comment = ?") && query.2.contains("last_altered < DATEADD('hour', -24"), "{}", query.2);
        let drops: Vec<&str> = calls.iter().filter(|c| c.2.starts_with("DROP")).map(|c| c.2.as_str()).collect();
        assert!(drops.contains(&"DROP TABLE IF EXISTS \"DB\".\"PUBLIC\".\"T__DBINE_CARGA_0123456789ABCDEF\""), "{drops:?}");
        assert!(!drops.iter().any(|d| d.contains("otra") || d.contains("TX__")), "{drops:?}");
        assert_eq!(drops.len(), 2, "{drops:?}");
    }

    #[test]
    fn staging_names_are_unique_and_recognised() {
        let (a, b) = (staging_suffix(), staging_suffix());
        assert_ne!(a, b);
        assert!(is_staging_of("T", &staging_name("T", &a)));
        assert!(!is_staging_of("T", "T__DBINE_CARGA"));
        assert!(!is_staging_of("T", "T__DBINE_CARGA_0123456789abcdef"));
        assert!(!is_staging_of("T", &staging_name("TX", &a)));
    }
}

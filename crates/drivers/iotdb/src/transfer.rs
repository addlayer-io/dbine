//! Bulk transfer (see `dbine_driver::transfer`) for Apache IoTDB and
//! TimechoDB, through the REST API v2 (tree model: a device is the table,
//! its series the columns, `Time` the key). Same API in 1.x and 2.x; the
//! types STRING, BLOB, DATE and TIMESTAMP exist from 1.3.3 on.
//!
//! Reading: `SELECT <series> FROM <device> WHERE time > <last> LIMIT n`,
//! walking ahead by `Time` (unique in a device), the next page requested
//! while the current one is handed over. A page is sized by bytes
//! ([`PAGE_BYTES`], from the width of the page before; at most [`PAGE`]
//! rows). Every requested column must be a series of the device, and the
//! answer's columns are matched by name (IoTDB leaves out series it
//! doesn't find). Values
//! are typed by the answer's data types: `Time` and TIMESTAMP series as
//! date-time text with the server's precision (ms, µs or ns digits),
//! INT32/INT64 integers, FLOAT/DOUBLE, BOOLEAN, TEXT/STRING text, DATE as
//! a date (fractional numbers parsed from the answer's text bit for bit,
//! see [`quote_floats`]). BLOB comes as the bytes of the text the REST API answers (it
//! decodes a BLOB as UTF-8: bytes that aren't valid UTF-8 can't be read
//! back through REST).
//!
//! Loading: `insertTablet`, columnar tablets of at most [`TABLET`] rows
//! and [`TABLET_BYTES`], [`IN_FLIGHT`] at once, with each series' type
//! taken from the target (or, for series that don't exist yet, from the
//! source column's type, or else from its first value; a series whose
//! type isn't known yet stays out of the tablets until it is, so it's
//! never created with a guess) and the device's alignment. Before 1.3.3
//! (no TIMESTAMP, DATE nor BLOB) a new series for a source column of
//! those types is TEXT: the date-time or date text, the bytes as `0x…`
//! hex. Measurements go backquoted unless plain (`order-id`, `a.b`). NaN and
//! ±Infinity go as IoTDB takes them (`"NaN"`, `"Infinity"`). A timestamp
//! finer than the server's precision is an error (it's the key: two rows
//! would collapse into one). A tablet doesn't carry a BLOB that isn't
//! valid UTF-8 (the API takes BLOBs as text): those rows go as an
//! `INSERT … VALUES` with `X'…'` literals, within the same byte bound.
//!
//! Progress counts rows the server answered for. On an error the requests
//! already sent are awaited (and counted) before returning; a load dropped
//! mid-way (cancelled) waits for them in its drop (and reports them), so
//! nothing commits after it has returned.

use crate::{full_device, node, split_path, status, unquote, IotDbSession, Precision};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};
use serde_json::{json, Value as J};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Rows per read request, at most…
const PAGE: usize = 50_000;
/// …and in the first one (the rows' width isn't known yet).
const FIRST_PAGE: usize = 256;
/// Estimated memory of a read page (answer text plus parsed values): a
/// page and the one prefetched stay around twice this.
const PAGE_BYTES: usize = 8 << 20;
/// Rows per `insertTablet`, at most…
const TABLET: usize = 10_000;
/// …and its estimated memory (values being gathered; the request sent is
/// smaller). With [`IN_FLIGHT`] requests out, ~20 MiB per table.
const TABLET_BYTES: usize = 4 << 20;
/// Write requests in flight at once.
const IN_FLIGHT: usize = 4;
/// Memory of a parsed JSON value beyond its text.
const VALUE_OVERHEAD: usize = 48;
/// How long a dropped (cancelled) load waits for the requests it sent.
const SETTLE: Duration = Duration::from_secs(120);

/// The REST endpoint, detached from the session (for requests in flight).
#[derive(Clone)]
struct Rest {
    http: reqwest::Client,
    base: String,
    username: String,
    password: String,
}

impl Rest {
    /// A request whose JSON body is already serialized; the answer and the
    /// length of its text.
    async fn post_raw(&self, path: &str, body: Vec<u8>) -> Result<(J, usize)> {
        let resp = self
            .http
            .post(format!("{}/rest/v2/{path}", self.base))
            .basic_auth(&self.username, Some(&self.password))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Connect(format!("no se pudo conectar con IoTDB: {e}")))?;
        let code = resp.status();
        let text = resp.text().await.map_err(Error::connect)?;
        let len = text.len();
        let v: J = serde_json::from_str(&quote_floats(&text, 3)).unwrap_or(J::Null);
        if code.as_u16() == 401 || code.as_u16() == 403 {
            return Err(Error::AuthFailed(status(&v).map(|(_, m)| m).unwrap_or_else(|| text.trim().to_string())));
        }
        if v.is_null() {
            return Err(Error::Query(format!("HTTP {code}: {}", text.trim())));
        }
        if let Some((_, m)) = status(&v) {
            return Err(Error::Query(m));
        }
        Ok((v, len))
    }
}

/// Fractional JSON numbers at nesting `depth` (the answer's columns of
/// values) turned into strings, so they're parsed here bit for bit: the
/// JSON parser's best-effort float parsing can miss a double's last digit.
pub(crate) fn quote_floats(body: &str, depth: usize) -> String {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 256);
    let (mut level, mut i, mut from) = (0usize, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'[' | b'{' => level += 1,
            b']' | b'}' => level = level.saturating_sub(1),
            b'-' | b'0'..=b'9' if level == depth => {
                let start = i;
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    i += 1;
                }
                let token = &body[start..i];
                if token.contains(['.', 'e', 'E']) {
                    out.push_str(&body[from..start]);
                    out.push('"');
                    out.push_str(token);
                    out.push('"');
                    from = i;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push_str(&body[from..]);
    out
}

impl Precision {
    fn per_second(self) -> i64 {
        match self {
            Precision::Ms => 1_000,
            Precision::Us => 1_000_000,
            Precision::Ns => 1_000_000_000,
        }
    }
}

/// An instant as `YYYY-MM-DD HH:MM:SS.fff[fff[fff]]` (the precision's digits).
pub(crate) fn instant_text(t: i64, p: Precision) -> String {
    let (secs, frac) = (t.div_euclid(p.per_second()), t.rem_euclid(p.per_second()));
    let nanos = (frac * (1_000_000_000 / p.per_second())) as u32;
    let fmt = match p {
        Precision::Ms => "%Y-%m-%d %H:%M:%S%.3f",
        Precision::Us => "%Y-%m-%d %H:%M:%S%.6f",
        Precision::Ns => "%Y-%m-%d %H:%M:%S%.9f",
    };
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(d) => d.format(fmt).to_string(),
        None => t.to_string(),
    }
}

/// An instant (date-time text with or without offset, a date, or a
/// number) in the server's precision. One finer than that precision is an
/// error, not truncated: `Time` is the key, and IoTDB overwrites a row
/// with the same timestamp.
pub(crate) fn instant_of(c: &Cell, p: Precision) -> std::result::Result<i64, String> {
    let bad = || format!("la marca de tiempo {c:?} no es válida");
    let from = |d: chrono::DateTime<chrono::Utc>| -> std::result::Result<i64, String> {
        // In i128: exact for any date chrono takes (i64 nanoseconds only
        // reach 1677–2262), sub-second part included.
        let n = d.timestamp() as i128 * 1_000_000_000 + i128::from(d.timestamp_subsec_nanos());
        let per = 1_000_000_000 / p.per_second() as i128;
        if n.rem_euclid(per) != 0 {
            let unit = match p {
                Precision::Ms => "milisegundos",
                Precision::Us => "microsegundos",
                Precision::Ns => "nanosegundos",
            };
            return Err(format!(
                "la marca de tiempo {c:?} tiene más precisión que la del servidor ({unit}): se perdería, y filas distintas \
                 quedarían con la misma marca (IoTDB guarda una sola por marca de tiempo)"
            ));
        }
        i64::try_from(n / per).map_err(|_| bad())
    };
    match c {
        Cell::Int(i) => Ok(*i),
        Cell::UInt(u) => i64::try_from(*u).map_err(|_| bad()),
        Cell::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 => Ok(*f as i64),
        Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Date(s) | Cell::Text(s) | Cell::Decimal(s) => {
            let s = s.trim();
            if let Ok(i) = s.parse::<i64>() {
                return Ok(i);
            }
            let t = s.replacen('T', " ", 1);
            if let Ok(d) = chrono::DateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f%:z") {
                return from(d.to_utc());
            }
            if let Ok(d) = chrono::DateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f%z") {
                return from(d.to_utc());
            }
            let t = t.strip_suffix('Z').unwrap_or(&t);
            if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f") {
                return from(d.and_utc());
            }
            if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
                return from(d.and_hms_opt(0, 0, 0).ok_or_else(bad)?.and_utc());
            }
            Err(bad())
        }
        _ => Err(bad()),
    }
}

/// A REST value as a cell, given its IoTDB data type.
pub(crate) fn to_cell(v: &J, ty: &str, p: Precision) -> Cell {
    match (v, ty) {
        (J::Null, _) => Cell::Null,
        (J::Number(n), "TIMESTAMP") => n.as_i64().map_or_else(|| Cell::Text(n.to_string()), |t| Cell::DateTime(instant_text(t, p))),
        (J::Number(n), "DATE") => match n.as_i64() {
            Some(d) => Cell::Date(format!("{:04}-{:02}-{:02}", d / 10_000, d / 100 % 100, d % 100)),
            None => Cell::Text(n.to_string()),
        },
        (J::Number(n), "FLOAT" | "DOUBLE") => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        (J::Number(n), _) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Cell::Int(i),
            (None, Some(u)) => Cell::UInt(u),
            _ => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        (J::Bool(b), _) => Cell::Bool(*b),
        (J::String(s), "BOOLEAN") => Cell::Bool(s == "true"),
        (J::String(s), "FLOAT" | "DOUBLE") => Cell::Float(match s.as_str() {
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            other => other.parse().unwrap_or(f64::NAN),
        }),
        (J::String(s), "BLOB") => Cell::Bytes(s.clone().into_bytes()),
        (J::String(s), "DATE") => Cell::Date(s.clone()),
        (J::String(s), _) => Cell::Text(s.clone()),
        (other, _) => Cell::Json(other.to_string()),
    }
}

/// A cell as a tablet value of type `ty`; `Ok(None)` for a BLOB that
/// isn't valid UTF-8 (the row goes by SQL).
pub(crate) fn tablet_value(c: &Cell, ty: &str, p: Precision) -> std::result::Result<Option<J>, String> {
    let bad = || format!("el valor {c:?} no es un {ty}");
    let text = |c: &Cell| match c.to_json() {
        J::String(s) => s,
        other => other.to_string(),
    };
    if matches!(c, Cell::Null) {
        return Ok(Some(J::Null));
    }
    let v = match ty {
        "BOOLEAN" => match c {
            Cell::Bool(b) => J::Bool(*b),
            Cell::Int(i) => J::Bool(*i != 0),
            Cell::UInt(u) => J::Bool(*u != 0),
            Cell::Text(s) if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") => J::Bool(s.eq_ignore_ascii_case("true")),
            _ => return Err(bad()),
        },
        "INT32" | "INT64" => {
            let i = match c {
                Cell::Int(i) => Some(*i),
                Cell::UInt(u) => i64::try_from(*u).ok(),
                Cell::Bool(b) => Some(*b as i64),
                Cell::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
                Cell::Text(s) | Cell::Decimal(s) => s.trim().parse().ok(),
                _ => None,
            };
            let i = i.ok_or_else(bad)?;
            if ty == "INT32" && i32::try_from(i).is_err() {
                return Err(bad());
            }
            J::from(i)
        }
        "FLOAT" | "DOUBLE" => {
            let f = match c {
                Cell::Float(f) => Some(*f),
                Cell::Int(i) => Some(*i as f64),
                Cell::UInt(u) => Some(*u as f64),
                Cell::Text(s) | Cell::Decimal(s) => s.trim().parse().ok(),
                _ => None,
            };
            // IoTDB stores NaN and ±Infinity; JSON can't carry them as
            // numbers, and the REST API takes them as these strings.
            match f.ok_or_else(bad)? {
                f if f.is_finite() => J::from(f),
                f if f.is_nan() => J::from("NaN"),
                f if f > 0.0 => J::from("Infinity"),
                _ => J::from("-Infinity"),
            }
        }
        "TIMESTAMP" => J::from(instant_of(c, p)?),
        "DATE" => {
            let s = match c {
                Cell::Date(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Text(s) => s.trim().to_string(),
                Cell::Int(i) => return Ok(Some(J::from(*i))),
                _ => return Err(bad()),
            };
            // A time of day other than midnight (an Oracle DATE, a
            // date-time) doesn't fit a DATE: refused, never cut off.
            if !midnight(s.get(10..).unwrap_or("")) {
                return Err(format!("el valor {c:?} tiene hora y no entra en un DATE sin perderla"));
            }
            let d = chrono::NaiveDate::parse_from_str(s.get(..10).unwrap_or(&s), "%Y-%m-%d").map_err(|_| bad())?;
            J::from(d.format("%Y%m%d").to_string().parse::<i64>().map_err(|_| bad())?)
        }
        "BLOB" => match c {
            Cell::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => J::String(s.to_string()),
                Err(_) => return Ok(None),
            },
            other => J::String(text(other)),
        },
        _ => J::String(text(c)),
    };
    Ok(Some(v))
}

/// What follows a date's `YYYY-MM-DD` says midnight (or nothing): no time,
/// or `00:00[:00[.000…]]`, with at most a zero offset (`Z`, `+00:00`).
fn midnight(tail: &str) -> bool {
    let Some(t) = tail.strip_prefix([' ', 'T']) else { return tail.is_empty() };
    let t = t.strip_prefix("00:00").unwrap_or("x");
    let t = t.strip_prefix(":00").unwrap_or(t);
    let t = match t.strip_prefix('.') {
        Some(f) => f.trim_start_matches('0'),
        None => t,
    };
    matches!(t.trim(), "" | "Z" | "z" | "UTC" | "+00" | "-00" | "+0000" | "-0000" | "+00:00" | "-00:00")
}

/// A cell as an SQL literal of type `ty` (rows a tablet can't carry).
/// A value that isn't of the type is `Query`; one IoTDB's SQL can't write
/// (±Infinity) is `Unsupported`.
fn sql_literal(c: &Cell, ty: &str, p: Precision) -> Result<String> {
    if let (Cell::Bytes(b), "BLOB") = (c, ty) {
        return Ok(format!("X'{}'", b.iter().map(|x| format!("{x:02X}")).collect::<String>()));
    }
    Ok(match tablet_value(c, ty, p).map_err(Error::Query)?.unwrap_or(J::Null) {
        J::Null => "null".into(),
        // IoTDB's SQL has NaN but no literal for ±Infinity.
        J::String(s) if matches!(ty, "FLOAT" | "DOUBLE") && s == "NaN" => s,
        J::String(_) if matches!(ty, "FLOAT" | "DOUBLE") => {
            return Err(Error::Unsupported(format!(
                "el valor {c:?} no se puede escribir: la fila va por SQL (tiene un BLOB que no es UTF-8) y el SQL de IoTDB no tiene literal para ±Infinity"
            )))
        }
        J::String(s) => format!("'{}'", s.replace('\'', "''")),
        J::Number(n) if ty == "DATE" => {
            let d = n.as_i64().unwrap_or_default();
            format!("'{:04}-{:02}-{:02}'", d / 10_000, d / 100 % 100, d % 100)
        }
        v => v.to_string(),
    })
}

/// IoTDB data type for a series that doesn't exist yet, from the source
/// column's type (as its engine spells it), when there's a lossless one.
/// `modern`: the server has TIMESTAMP, DATE and BLOB (1.3.3 on); before
/// that those columns go as TEXT (the value's text, the bytes' `0x…` hex).
fn source_type(type_name: &str, modern: bool) -> Option<&'static str> {
    match source_type_modern(type_name)? {
        "TIMESTAMP" | "DATE" | "BLOB" if !modern => Some("TEXT"),
        t => Some(t),
    }
}

/// The server has the types TIMESTAMP, DATE and BLOB (IoTDB 1.3.3 on),
/// from `SHOW VERSION`'s text (like `1.3.2`, `2.0.5`); an unknown version
/// is taken as an old one (TEXT is valid everywhere).
fn has_new_types(version: &str) -> bool {
    let digits = version.trim_start_matches(|c: char| !c.is_ascii_digit());
    let v: Vec<u32> = digits.split(['.', '-', ' ']).map_while(|x| x.parse().ok()).take(3).collect();
    match v.as_slice() {
        [major, ..] if *major >= 2 => true,
        [1, minor, ..] if *minor >= 4 => true,
        [1, 3, patch] => *patch >= 3,
        _ => false,
    }
}

fn source_type_modern(type_name: &str) -> Option<&'static str> {
    let t = type_name.trim().to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    Some(match (t.as_str(), base) {
        ("boolean" | "bool", _) => "BOOLEAN",
        (_, "bigint" | "int8" | "int64" | "uint64") if t.contains("unsigned") || base == "uint64" => return None,
        (_, "int" | "integer" | "int4" | "mediumint" | "int32") if t.contains("unsigned") => "INT64",
        (_, "int32" | "int" | "integer" | "int4" | "smallint" | "int2" | "tinyint" | "mediumint" | "serial" | "smallserial" | "int16" | "uint8" | "uint16") => "INT32",
        (_, "int64" | "bigint" | "int8" | "bigserial" | "long" | "uint32") => "INT64",
        (_, "float4" | "real" | "float32" | "binary_float") => "FLOAT",
        (_, "double" | "float" | "float8" | "float64" | "binary_double") => "DOUBLE",
        (_, "text" | "string" | "varchar" | "char" | "character" | "nchar" | "nvarchar" | "varchar2" | "nvarchar2" | "clob" | "nclob" | "ntext"
        | "tinytext" | "mediumtext" | "longtext" | "uuid" | "json" | "jsonb" | "xml" | "decimal" | "numeric" | "number" | "money") => "TEXT",
        (_, "blob" | "bytea" | "binary" | "varbinary" | "longblob" | "mediumblob" | "tinyblob" | "image" | "raw") => "BLOB",
        ("date", _) => "DATE",
        (_, "timestamp" | "timestamptz" | "datetime" | "datetime2" | "datetimeoffset") => "TIMESTAMP",
        _ => return None,
    })
}

/// IoTDB data type for a series that doesn't exist yet and whose source
/// type says nothing: from its first value.
fn value_type(c: &Cell) -> &'static str {
    match c {
        Cell::Bool(_) => "BOOLEAN",
        Cell::Int(_) | Cell::UInt(_) => "INT64",
        Cell::Float(_) => "DOUBLE",
        _ => "TEXT",
    }
}

/// IoTDB data type for a new series from a source column typed DATE (on a
/// server with DATE and TIMESTAMP), from its first value: a date is DATE;
/// a date-time (Oracle's DATE holds a time of day) is TIMESTAMP, so its
/// time isn't lost; anything else as [`value_type`].
fn date_value_type(c: &Cell) -> &'static str {
    match c {
        Cell::Date(_) => "DATE",
        Cell::DateTime(_) | Cell::DateTimeTz(_) => "TIMESTAMP",
        _ => value_type(c),
    }
}

/// A measurement's name (as stored, unquoted) as a node in SQL and in a
/// tablet: bare when plain, else backquoted with its backquotes doubled.
/// Unlike `node`, a name already wrapped in backquotes is still a literal
/// name: `` `x` `` is the series `` `x` ``, never `x`, and nothing in a
/// name reaches the SQL unquoted.
fn measurement_node(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().all(|c| c.is_ascii_digit())
        && !matches!(name.to_ascii_lowercase().as_str(), "time" | "timestamp" | "root");
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

/// One tablet's worth of rows, as the request's columns.
struct Tablet {
    times: Vec<i64>,
    values: Vec<Vec<J>>,
    /// Rows that go by SQL (a BLOB that isn't UTF-8): the time and each
    /// series' literal (`None`: null in a series whose type isn't known yet).
    sql_rows: Vec<(i64, Vec<Option<String>>)>,
    /// Estimated memory.
    bytes: usize,
}

impl Tablet {
    fn new(cols: usize) -> Tablet {
        Tablet { times: Vec::new(), values: (0..cols).map(|_| Vec::new()).collect(), sql_rows: Vec::new(), bytes: 0 }
    }
    fn rows(&self) -> usize {
        self.times.len() + self.sql_rows.len()
    }
    fn full(&self) -> bool {
        self.rows() >= TABLET || self.bytes >= TABLET_BYTES
    }

    /// The requests that write it, with the rows each one commits: an
    /// `insertTablet` and, for the rows that go by SQL, an `INSERT`. Series
    /// whose type isn't known yet are left out (all their values here are
    /// null).
    fn requests(self, device: &str, aligned: bool, measurements: &[String], types: &[Option<String>]) -> Vec<(&'static str, Vec<u8>, u64)> {
        let keep: Vec<usize> = (0..measurements.len()).filter(|&k| types[k].is_some()).collect();
        let mut out = Vec::new();
        if keep.is_empty() {
            return out;
        }
        let Tablet { times, values, sql_rows, .. } = self;
        if !times.is_empty() {
            let n = times.len() as u64;
            let mut values: Vec<Option<Vec<J>>> = values.into_iter().map(Some).collect();
            let mut body = serde_json::Map::new();
            body.insert("device".into(), J::from(device));
            body.insert("is_aligned".into(), J::Bool(aligned));
            body.insert("timestamps".into(), J::Array(times.into_iter().map(J::from).collect()));
            // Quoted as in SQL: IoTDB refuses `order-id`, `first name` or
            // `a.b` bare ("not a legal path").
            body.insert("measurements".into(), keep.iter().map(|&k| J::from(measurement_node(&measurements[k]))).collect());
            body.insert("data_types".into(), keep.iter().map(|&k| J::from(types[k].as_deref().unwrap_or_default())).collect());
            body.insert("values".into(), keep.iter().map(|&k| J::Array(values[k].take().unwrap_or_default())).collect());
            drop(values);
            out.push(("insertTablet", J::Object(body).to_string().into_bytes(), n));
        }
        if !sql_rows.is_empty() {
            let n = sql_rows.len() as u64;
            let head = format!(
                "INSERT INTO {device}(timestamp, {}){} VALUES ",
                keep.iter().map(|&k| measurement_node(&measurements[k])).collect::<Vec<_>>().join(", "),
                if aligned { " ALIGNED" } else { "" }
            );
            let rows: Vec<String> = sql_rows
                .into_iter()
                .map(|(t, lits)| {
                    let lits = keep.iter().map(|&k| lits[k].as_deref().unwrap_or("null"));
                    format!("({})", std::iter::once(t.to_string().as_str()).chain(lits).collect::<Vec<_>>().join(", "))
                })
                .collect();
            out.push(("nonQuery", json!({ "sql": format!("{head}{}", rows.join(", ")) }).to_string().into_bytes(), n));
        }
        out
    }
}

/// Estimated memory of a value gathered into a tablet.
fn value_bytes(v: &J) -> usize {
    VALUE_OVERHEAD + if let J::String(s) = v { s.len() } else { 0 }
}

/// Rows the server answered for, reported every `every`.
struct Committed<'a> {
    done: u64,
    reported: u64,
    every: u64,
    progress: Progress<'a>,
}

impl Committed<'_> {
    fn add(&mut self, rows: u64) {
        self.done += rows;
        if self.done - self.reported >= self.every {
            self.reported = self.done;
            (self.progress)(self.done);
        }
    }
    fn finish(&mut self) {
        if self.done > self.reported {
            self.reported = self.done;
            (self.progress)(self.done);
        }
    }
}

/// A write request's task: the rows it got committed (all of a request,
/// or none) and how it ended.
type Sent = JoinHandle<(u64, Result<()>)>;

/// Write requests sent and not answered yet, oldest first, and the rows
/// committed so far. Dropped with some still out (the load was cancelled,
/// its future dropped), it waits for them: the server commits a request it
/// already received even if the client goes away, and that must not
/// happen after the load has returned; the rows they commit are reported
/// then. The requests aren't aborted (a `JoinHandle` dropped detaches its
/// task).
struct Inflight<'a> {
    sent: VecDeque<Sent>,
    committed: Committed<'a>,
}

impl<'a> Inflight<'a> {
    fn new(committed: Committed<'a>) -> Self {
        Inflight { sent: VecDeque::new(), committed }
    }
    fn len(&self) -> usize {
        self.sent.len()
    }
    fn push(&mut self, h: Sent) {
        self.sent.push_back(h);
    }
    /// The oldest request's outcome, its rows counted (it stays here until
    /// answered, so a cancel while waiting still finds it on drop).
    async fn next(&mut self) -> Option<Result<()>> {
        let r = self.sent.front_mut()?.await;
        self.sent.pop_front();
        let (n, r) = joined(r);
        self.committed.add(n);
        Some(r)
    }
    /// The oldest request's outcome, if it's already answered.
    async fn ready(&mut self) -> Option<Result<()>> {
        if self.sent.front()?.is_finished() {
            self.next().await
        } else {
            None
        }
    }
    /// Await every request, counting the rows that went through, and
    /// report them; the first error, if any.
    async fn settle(&mut self) -> Result<()> {
        let mut first = Ok(());
        while let Some(r) = self.next().await {
            if let (Err(e), true) = (r, first.is_ok()) {
                first = Err(e);
            }
        }
        self.committed.finish();
        first
    }
    fn answered(&self) -> bool {
        self.sent.iter().all(JoinHandle::is_finished)
    }
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        if !self.answered() {
            let wait = || {
                let until = Instant::now() + SETTLE;
                while !self.answered() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
                // The requests run on this very thread: waiting would never end.
                Ok(_) => {}
                Err(_) => wait(),
            }
        }
        // Count what the answered ones committed (a finished task's handle
        // is ready at the first poll).
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut left = 0;
        for mut h in self.sent.drain(..) {
            match std::pin::Pin::new(&mut h).poll(&mut cx) {
                std::task::Poll::Ready(r) => self.committed.add(joined(r).0),
                std::task::Poll::Pending => left += 1,
            }
        }
        self.committed.finish();
        if left > 0 {
            tracing::warn!("IoTDB: carga interrumpida con {left} envíos sin respuesta; el servidor todavía puede guardarlos");
        }
    }
}

/// A device path with every node as IoTDB takes it (backquoted unless
/// plain): a name like `order-items` or `d*` is one literal node, never
/// SQL nor a wildcard.
pub(crate) fn quoted_path(path: &str) -> String {
    split_path(path)
        .iter()
        .enumerate()
        .map(|(i, n)| if i == 0 && n == "root" { n.clone() } else { node(n) })
        .collect::<Vec<_>>()
        .join(".")
}

/// The measurement (last node, unquoted) of a full series path.
fn measurement_of(path: &str) -> String {
    unquote(split_path(path).last().map(String::as_str).unwrap_or(path))
}

impl IotDbSession {
    fn rest(&self) -> Rest {
        Rest { http: self.http.clone(), base: self.base.clone(), username: self.username.clone(), password: self.password.clone() }
    }

    /// The device's full path, every node quoted as needed.
    fn device_of(&self, obj: &ObjectRef) -> Result<String> {
        let path = if obj.schema().is_some() || obj.name.starts_with("root.") {
            full_device(obj.schema(), &obj.name)?
        } else {
            self.device_path(obj)
        };
        Ok(quoted_path(&path))
    }

    /// The device's series: (measurement, data type).
    async fn series(&self, device: &str) -> Result<Vec<(String, String)>> {
        let t = self.query(&format!("SHOW TIMESERIES {device}.*"), 1_000_000).await?;
        let name = t.columns.iter().position(|c| c.name == "Timeseries").unwrap_or(0);
        let ty = t.columns.iter().position(|c| c.name == "DataType").unwrap_or(3);
        Ok(t.rows
            .iter()
            .map(|r| {
                let full = r.get(name).and_then(J::as_str).unwrap_or_default();
                (measurement_of(full), r.get(ty).and_then(J::as_str).unwrap_or_default().to_string())
            })
            .collect())
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let device = self.device_of(&spec.table)?;
        let series = self.series(&device).await?;
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => std::iter::once("Time".to_string()).chain(series.iter().map(|s| s.0.clone())).collect(),
        };
        let measured: Vec<&String> = names.iter().filter(|n| !crate::is_time(n)).collect();
        if measured.is_empty() {
            return Err(Error::Query(format!("El dispositivo {device} no tiene series para leer.")));
        }
        if let Some(n) = measured.iter().find(|n| !series.iter().any(|s| s.0 == ***n)) {
            return Err(Error::Query(format!("La columna «{n}» no existe en el dispositivo {device}.")));
        }
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| match series.iter().find(|s| &s.0 == n) {
                _ if crate::is_time(n) => TransferColumn { name: n.clone(), type_name: "TIMESTAMP".into(), nullable: false },
                Some(s) => TransferColumn { name: n.clone(), type_name: s.1.clone(), nullable: true },
                None => TransferColumn { name: n.clone(), type_name: String::new(), nullable: true },
            })
            .collect();
        let select = format!("SELECT {} FROM {device}", measured.iter().map(|n| measurement_node(n)).collect::<Vec<_>>().join(", "));
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(|f| format!("({f})"));
        let stmt = |after: Option<i64>, limit: usize| {
            let conds: Vec<String> = filter.iter().cloned().chain(after.map(|t| format!("time > {t}"))).collect();
            let w = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
            format!("{select}{w} LIMIT {limit}")
        };
        let rest = self.rest();
        let fetch = |after: Option<i64>, limit: usize| -> JoinHandle<Result<(J, usize)>> {
            let (rest, sql) = (rest.clone(), stmt(after, limit));
            tokio::spawn(async move { rest.post_raw("query", json!({ "sql": sql, "row_limit": limit + 1 }).to_string().into_bytes()).await })
        };
        let p = self.precision;

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        let mut limit = FIRST_PAGE;
        let mut pending = Some(fetch(None, limit));
        // Dropped (cancelled or failed) with a page requested: stop it.
        struct Abort<'a>(&'a mut Option<JoinHandle<Result<(J, usize)>>>);
        impl Drop for Abort<'_> {
            fn drop(&mut self) {
                if let Some(h) = self.0.take() {
                    h.abort();
                }
            }
        }
        let pending = Abort(&mut pending);
        while let Some(h) = pending.0.as_mut() {
            let (page, text_len) = h.await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))??;
            *pending.0 = None;
            let times: Vec<i64> = page.get("timestamps").and_then(J::as_array).into_iter().flatten().filter_map(J::as_i64).collect();
            let empty = Vec::new();
            let values: Vec<&Vec<J>> =
                page.get("values").and_then(J::as_array).map(|v| v.iter().map(|c| c.as_array().unwrap_or(&empty)).collect()).unwrap_or_default();
            if times.len() >= limit {
                // Size the next page by this one's width, to ~PAGE_BYTES.
                let cells: usize = values.iter().map(|c| c.len()).sum();
                let per_row = (text_len + cells * VALUE_OVERHEAD).div_ceil(times.len()).max(1);
                limit = (PAGE_BYTES / per_row).clamp(1, PAGE);
                *pending.0 = Some(fetch(times.last().copied(), limit));
            }
            let types: Vec<&str> = page.get("data_types").and_then(J::as_array).map(|t| t.iter().map(|x| x.as_str().unwrap_or("")).collect()).unwrap_or_default();
            // The answer's columns by name: IoTDB leaves out what it doesn't find.
            let exprs: Vec<String> =
                page.get("expressions").and_then(J::as_array).map(|e| e.iter().map(|x| measurement_of(x.as_str().unwrap_or(""))).collect()).unwrap_or_default();
            let at: Vec<Option<usize>> = names.iter().map(|n| if crate::is_time(n) { None } else { exprs.iter().position(|e| e == n) }).collect();
            {
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                for (r, t) in times.iter().enumerate() {
                    let row: Vec<Cell> = names
                        .iter()
                        .zip(&at)
                        .map(|(n, a)| match a {
                            _ if crate::is_time(n) => Cell::DateTime(instant_text(*t, p)),
                            Some(m) => values.get(*m).and_then(|c| c.get(r)).map_or(Cell::Null, |v| to_cell(v, types.get(*m).copied().unwrap_or(""), p)),
                            None => Cell::Null,
                        })
                        .collect();
                    builder.push(row, &mut *s)?;
                }
            }
        }
        drop(pending);
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()));
        }
        let device = self.device_of(&spec.table)?;
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let Some(ti) = names.iter().position(|c| crate::is_time(c)) else {
            return Err(Error::Unsupported("IoTDB necesita la columna Time (marca de tiempo) para insertar filas.".into()));
        };
        let prefix = format!("{device}.");
        // A full path's last node is written as in SQL (unquoted here); a
        // bare column name is the measurement's literal name.
        let measured: Vec<(usize, String)> = (0..names.len())
            .filter(|&i| i != ti)
            .map(|i| (i, names[i].strip_prefix(&prefix).map_or_else(|| names[i].clone(), unquote)))
            .collect();
        let ms: Arc<Vec<String>> = Arc::new(measured.iter().map(|(_, m)| m.clone()).collect());
        let existing = self.series(&device).await.unwrap_or_default();
        let aligned = {
            let t = self.query(&format!("SHOW DEVICES {device}"), 10).await?;
            let a = t.columns.iter().position(|c| c.name == "IsAligned");
            t.rows.first().and_then(|r| r.get(a?)).and_then(J::as_str) == Some("true")
        };
        let p = self.precision;
        // TIMESTAMP, DATE and BLOB exist from 1.3.3 on: before, those
        // source columns go as TEXT (1.3.2 refuses the types in a tablet).
        let modern = match self.query("SHOW VERSION", 10).await {
            Ok(t) => crate::texts(&t, 0).first().is_some_and(|v| has_new_types(v)),
            Err(_) => false,
        };
        // Types: the target's; for a new series the source column's, or else
        // its first value's (until then it stays out of the requests).
        let source_col = |i: usize| -> Option<&TransferColumn> {
            if columns.len() == names.len() {
                columns.get(i)
            } else {
                columns.iter().find(|c| c.name == names[i])
            }
        };
        // A new series for a source column typed DATE is typed by its
        // values: Oracle's DATE (and SQLite's) hold a time of day too.
        let by_date_value: Vec<bool> = measured
            .iter()
            .map(|(i, m)| !existing.iter().any(|s| &s.0 == m) && source_col(*i).and_then(|c| source_type(&c.type_name, modern)) == Some("DATE"))
            .collect();
        let mut types: Vec<Option<String>> = measured
            .iter()
            .zip(&by_date_value)
            .map(|((i, m), by_value)| {
                existing
                    .iter()
                    .find(|s| &s.0 == m)
                    .map(|s| s.1.clone())
                    .or_else(|| source_col(*i).and_then(|c| source_type(&c.type_name, modern)).filter(|_| !by_value).map(str::to_string))
            })
            .collect();
        let rest = Arc::new(self.rest());
        let device = Arc::new(device);

        let mut inflight = Inflight::new(Committed { done: 0, reported: 0, every: spec.commit_rows.max(1), progress });
        let mut tablet = Tablet::new(measured.len());
        let send = |t: Tablet, types: &[Option<String>]| {
            let requests = t.requests(&device, aligned, &ms, types);
            let rest = rest.clone();
            tokio::spawn(async move {
                // Each request commits its rows whole: count the ones that
                // went through even if a later one fails.
                let mut done = 0;
                for (path, body, n) in requests {
                    if let Err(e) = rest.post_raw(path, body).await {
                        return (done, Err(e));
                    }
                    done += n;
                }
                (done, Ok(()))
            })
        };
        let gathered: Result<()> = async {
            while let Some(batch) = source.next().await {
                for row in &batch.rows {
                    if row.len() != names.len() {
                        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                    }
                    let time = instant_of(&row[ti], p).map_err(Error::Query)?;
                    let mut vals = Vec::with_capacity(measured.len());
                    let mut by_sql = false;
                    for (k, (i, m)) in measured.iter().enumerate() {
                        if types[k].is_none() && !matches!(row[*i], Cell::Null) {
                            let ty = if by_date_value[k] { date_value_type(&row[*i]) } else { value_type(&row[*i]) };
                            types[k] = Some(ty.to_string());
                        }
                        let v = match types[k].as_deref() {
                            Some(ty) => tablet_value(&row[*i], ty, p).map_err(|e| Error::Query(format!("{m}: {e}")))?,
                            None => Some(J::Null),
                        };
                        match v {
                            Some(v) => vals.push(v),
                            None => by_sql = true,
                        }
                    }
                    if by_sql {
                        let mut lits = Vec::with_capacity(measured.len());
                        for (k, (i, m)) in measured.iter().enumerate() {
                            let lit = match types[k].as_deref() {
                                Some(ty) => Some(sql_literal(&row[*i], ty, p).map_err(|e| match e {
                                    Error::Unsupported(e) => Error::Unsupported(format!("{m}: {e}")),
                                    Error::Query(e) => Error::Query(format!("{m}: {e}")),
                                    e => e,
                                })?),
                                None => None,
                            };
                            tablet.bytes += VALUE_OVERHEAD + lit.as_ref().map_or(0, String::len);
                            lits.push(lit);
                        }
                        tablet.sql_rows.push((time, lits));
                    } else {
                        tablet.times.push(time);
                        tablet.bytes += 8;
                        for (col, v) in tablet.values.iter_mut().zip(vals) {
                            tablet.bytes += value_bytes(&v);
                            col.push(v);
                        }
                    }
                    if tablet.full() {
                        if inflight.len() >= IN_FLIGHT {
                            if let Some(r) = inflight.next().await {
                                r?;
                            }
                        }
                        let t = std::mem::replace(&mut tablet, Tablet::new(measured.len()));
                        inflight.push(send(t, &types));
                    }
                }
                while let Some(r) = inflight.ready().await {
                    r?;
                }
            }
            if tablet.rows() > 0 {
                let t = std::mem::replace(&mut tablet, Tablet::new(measured.len()));
                inflight.push(send(t, &types));
            }
            Ok(())
        }
        .await;
        // Whatever happened, every request sent is answered (and counted)
        // before returning.
        let settled = inflight.settle().await;
        gathered?;
        settled?;
        if let Some(k) = types.iter().position(Option::is_none) {
            // Only nulls and a source type with no IoTDB equivalent: IoTDB
            // keeps no point for a null, so nothing was lost, but the series
            // wasn't created.
            tracing::warn!("IoTDB: la serie {} de {device} quedó sin crear (solo tenía valores nulos y su tipo de origen no se conoce)", ms[k]);
        }
        Ok(inflight.committed.done)
    }
}

fn joined(r: std::result::Result<(u64, Result<()>), tokio::task::JoinError>) -> (u64, Result<()>) {
    r.unwrap_or_else(|e| (0, Err(Error::State(format!("envío de insertTablet interrumpido: {e}")))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_keep_the_precision() {
        assert_eq!(instant_text(1_700_000_000_123, Precision::Ms), "2023-11-14 22:13:20.123");
        assert_eq!(instant_text(1_700_000_000_000, Precision::Ms), "2023-11-14 22:13:20.000");
        assert_eq!(instant_text(1_700_000_000_123_456, Precision::Us), "2023-11-14 22:13:20.123456");
        assert_eq!(instant_text(-1, Precision::Ns), "1969-12-31 23:59:59.999999999");
        let dt = |s: &str| Cell::DateTime(s.into());
        assert_eq!(instant_of(&dt("2023-11-14 22:13:20.123456"), Precision::Us), Ok(1_700_000_000_123_456));
        assert_eq!(instant_of(&dt("2023-11-14 22:13:20.123000"), Precision::Ms), Ok(1_700_000_000_123));
        assert_eq!(instant_of(&Cell::DateTimeTz("2023-11-14 19:13:20.5-03:00".into()), Precision::Ms), Ok(1_700_000_000_500));
        assert_eq!(instant_of(&dt("2023-11-14T22:13:20Z"), Precision::Ms), Ok(1_700_000_000_000));
        assert_eq!(instant_of(&Cell::Date("1970-01-02".into()), Precision::Ms), Ok(86_400_000));
        assert_eq!(instant_of(&Cell::Int(42), Precision::Ns), Ok(42));
        assert!(instant_of(&Cell::Text("ayer".into()), Precision::Ms).is_err());
        assert!(instant_of(&Cell::Float(1.5), Precision::Ms).is_err());
        assert!(instant_of(&dt("1969-12-31 23:59:59.9995"), Precision::Ms).is_err());
    }

    /// Two source rows 1 µs apart would collapse into one ms timestamp.
    #[test]
    fn finer_timestamps_are_refused() {
        let a = instant_of(&Cell::DateTime("2024-01-01 00:00:00.000001".into()), Precision::Ms).unwrap_err();
        assert!(a.contains("más precisión") && a.contains("milisegundos"), "{a}");
        assert!(tablet_value(&Cell::DateTime("2024-01-01 00:00:00.000001".into()), "TIMESTAMP", Precision::Ms).is_err());
        assert_eq!(instant_of(&Cell::DateTime("2024-01-01 00:00:00.000001".into()), Precision::Us), Ok(1_704_067_200_000_001));
        assert_eq!(instant_of(&Cell::DateTime("2024-01-01 00:00:00.000001".into()), Precision::Ns), Ok(1_704_067_200_000_001_000));
    }

    #[test]
    fn rest_values_become_cells() {
        let p = Precision::Ms;
        assert_eq!(to_cell(&json!(9007199254740993i64), "INT64", p), Cell::Int(9007199254740993));
        assert_eq!(to_cell(&json!(1.5), "FLOAT", p), Cell::Float(1.5));
        assert_eq!(to_cell(&json!(2), "DOUBLE", p), Cell::Float(2.0));
        assert_eq!(to_cell(&json!("90.33333333333333"), "DOUBLE", p), Cell::Float(271.0 / 3.0));
        let body = r#"{"expressions":["root.a.b"],"data_types":["DOUBLE"],"timestamps":[1,2],"values":[[90.33333333333333,-1.5E300]]}"#;
        let v: J = serde_json::from_str(&quote_floats(body, 3)).unwrap();
        assert_eq!(v["values"][0], json!(["90.33333333333333", "-1.5E300"]));
        assert_eq!(v["timestamps"], json!([1, 2]));
        assert_eq!(to_cell(&json!(true), "BOOLEAN", p), Cell::Bool(true));
        assert_eq!(to_cell(&json!("true"), "BOOLEAN", p), Cell::Bool(true));
        assert_eq!(to_cell(&json!("ñ"), "TEXT", p), Cell::Text("ñ".into()));
        assert_eq!(to_cell(&json!(20240131), "DATE", p), Cell::Date("2024-01-31".into()));
        assert_eq!(to_cell(&json!(1700000000000i64), "TIMESTAMP", p), Cell::DateTime("2023-11-14 22:13:20.000".into()));
        assert_eq!(to_cell(&json!("ab"), "BLOB", p), Cell::Bytes(b"ab".to_vec()));
        assert_eq!(to_cell(&J::Null, "INT32", p), Cell::Null);
    }

    #[test]
    fn cells_become_tablet_values() {
        let p = Precision::Ms;
        let v = |c: Cell, ty: &str| tablet_value(&c, ty, p).unwrap().unwrap();
        assert_eq!(v(Cell::Int(5), "INT32"), json!(5));
        assert!(tablet_value(&Cell::Int(1 << 40), "INT32", p).is_err());
        assert_eq!(v(Cell::Int(1 << 40), "INT64"), json!(1i64 << 40));
        assert_eq!(v(Cell::Decimal("2.5".into()), "DOUBLE"), json!(2.5));
        assert_eq!(v(Cell::Float(f64::NAN), "DOUBLE"), json!("NaN"));
        assert_eq!(v(Cell::Float(f64::INFINITY), "FLOAT"), json!("Infinity"));
        assert_eq!(v(Cell::Float(f64::NEG_INFINITY), "DOUBLE"), json!("-Infinity"));
        assert_eq!(sql_literal(&Cell::Float(f64::NAN), "DOUBLE", p).unwrap(), "NaN");
        match sql_literal(&Cell::Float(f64::NEG_INFINITY), "DOUBLE", p) {
            Err(Error::Unsupported(m)) => assert!(m.contains("Infinity"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(sql_literal(&Cell::Text("x".into()), "INT32", p), Err(Error::Query(_))));
        assert_eq!(v(Cell::Bool(true), "BOOLEAN"), json!(true));
        assert_eq!(v(Cell::Date("2024-01-31".into()), "DATE"), json!(20240131));
        assert_eq!(v(Cell::DateTime("1970-01-01 00:00:01.5".into()), "TIMESTAMP"), json!(1500));
        assert_eq!(v(Cell::Json("{\"a\":1}".into()), "TEXT"), json!("{\"a\":1}"));
        assert_eq!(v(Cell::Int(7), "STRING"), json!("7"));
        assert_eq!(v(Cell::Bytes(b"ok".to_vec()), "BLOB"), json!("ok"));
        assert_eq!(tablet_value(&Cell::Bytes(vec![0xCA, 0xFE]), "BLOB", p).unwrap(), None);
        assert_eq!(sql_literal(&Cell::Bytes(vec![0xCA, 0xFE]), "BLOB", p).unwrap(), "X'CAFE'");
        assert_eq!(sql_literal(&Cell::Text("O'B".into()), "TEXT", p).unwrap(), "'O''B'");
        assert_eq!(sql_literal(&Cell::Date("2024-01-31".into()), "DATE", p).unwrap(), "'2024-01-31'");
        assert_eq!(sql_literal(&Cell::Null, "INT32", p).unwrap(), "null");
        assert_eq!(value_type(&Cell::Float(1.0)), "DOUBLE");
        assert_eq!(value_type(&Cell::Text("x".into())), "TEXT");
    }

    /// A name wrapped in backquotes is a literal name, and nothing in a
    /// name reaches the SQL or the tablet unquoted.
    #[test]
    fn measurement_names_are_always_escaped() {
        assert_eq!(measurement_node("plain_1"), "plain_1");
        assert_eq!(measurement_node("order-id"), "`order-id`");
        assert_eq!(measurement_node("`x`"), "```x```");
        assert_eq!(measurement_node("`a`) VALUES(1,2) --`"), "```a``) VALUES(1,2) --```");
        assert_eq!(measurement_node("time"), "`time`");
        let mut t = Tablet::new(1);
        t.sql_rows.push((1, vec![Some("X'00'".into())]));
        t.times.push(2);
        t.values[0].push(J::from("v"));
        let reqs = t.requests("root.db.d", false, &["`a`) VALUES(1,2) --".to_string()], &[Some("BLOB".into())]);
        let tablet: J = serde_json::from_slice(&reqs[0].1).unwrap();
        assert_eq!(tablet["measurements"], json!(["```a``) VALUES(1,2) --`"]));
        let sql: J = serde_json::from_slice(&reqs[1].1).unwrap();
        assert_eq!(sql["sql"], json!("INSERT INTO root.db.d(timestamp, ```a``) VALUES(1,2) --`) VALUES (1, X'00')"));
    }

    /// A date-time into a DATE is refused unless it's midnight; a source
    /// DATE column's new series is typed by its values.
    #[test]
    fn dates_keep_their_time() {
        let p = Precision::Ms;
        for bad in ["2024-01-31 13:45:07", "2024-01-31T00:00:01", "2024-01-31 00:00:00.001", "2024-01-31 00:00:00+03:00"] {
            assert!(tablet_value(&Cell::DateTime(bad.into()), "DATE", p).is_err(), "{bad}");
            assert!(tablet_value(&Cell::Text(bad.into()), "DATE", p).is_err(), "{bad}");
        }
        for ok in ["2024-01-31", "2024-01-31 00:00:00", "2024-01-31T00:00", "2024-01-31 00:00:00.000", "2024-01-31 00:00:00+00:00"] {
            assert_eq!(tablet_value(&Cell::DateTime(ok.into()), "DATE", p).unwrap(), Some(json!(20240131)), "{ok}");
        }
        assert_eq!(date_value_type(&Cell::Date("2024-01-31".into())), "DATE");
        assert_eq!(date_value_type(&Cell::DateTime("2024-01-31 13:45:07".into())), "TIMESTAMP");
        assert_eq!(date_value_type(&Cell::Text("2024-01-31 13:45".into())), "TEXT");
    }

    #[test]
    fn source_types_map_to_iotdb() {
        for (src, ty) in [
            ("DOUBLE", Some("DOUBLE")),
            ("int32", Some("INT32")),
            ("INT64", Some("INT64")),
            ("double precision", Some("DOUBLE")),
            ("float8", Some("DOUBLE")),
            ("float", Some("DOUBLE")),
            ("real", Some("FLOAT")),
            ("bigint", Some("INT64")),
            ("int unsigned", Some("INT64")),
            ("bigint unsigned", None),
            ("character varying(20)", Some("TEXT")),
            ("numeric(10,2)", Some("TEXT")),
            ("bytea", Some("BLOB")),
            ("timestamp with time zone", Some("TIMESTAMP")),
            ("date", Some("DATE")),
            ("boolean", Some("BOOLEAN")),
            ("geometry", None),
            ("", None),
        ] {
            assert_eq!(source_type(src, true), ty, "{src}");
        }
        // Before 1.3.3 (no TIMESTAMP, DATE nor BLOB): TEXT.
        for (src, ty) in [
            ("bytea", Some("TEXT")),
            ("timestamp with time zone", Some("TEXT")),
            ("datetime", Some("TEXT")),
            ("date", Some("TEXT")),
            ("double precision", Some("DOUBLE")),
            ("geometry", None),
        ] {
            assert_eq!(source_type(src, false), ty, "{src}");
        }
    }

    #[test]
    fn server_versions_with_new_types() {
        for (v, new) in [
            ("1.3.2", false),
            ("1.3.3", true),
            ("1.3.4.1", true),
            ("1.2.2", false),
            ("0.13.4", false),
            ("2.0.5", true),
            ("2.0.1-beta", true),
            ("V1.3.3", true),
            ("1.3.2-SNAPSHOT", false),
            ("", false),
            ("desconocida", false),
        ] {
            assert_eq!(has_new_types(v), new, "{v}");
        }
    }

    /// Outside 1677–2262 (no i64 nanoseconds) the sub-second part is kept,
    /// or refused if finer than the server's precision.
    #[test]
    fn far_instants_keep_their_fraction() {
        let dt = |s: &str| Cell::DateTime(s.into());
        assert_eq!(instant_of(&dt("1600-01-01 00:00:00.123"), Precision::Ms), Ok(-11_676_096_000_000 + 123));
        assert!(instant_of(&dt("1600-01-01 00:00:00.1234"), Precision::Ms).unwrap_err().contains("más precisión"));
        assert_eq!(instant_of(&dt("2300-01-01 00:00:00.5"), Precision::Us), Ok(10_413_792_000_500_000));
        // Out of range for i64 nanoseconds: refused, not wrapped.
        assert!(instant_of(&dt("1600-01-01 00:00:00"), Precision::Ns).is_err());
    }

    #[test]
    fn device_nodes_are_quoted() {
        assert_eq!(quoted_path("root.rvy.order-items"), "root.rvy.`order-items`");
        assert_eq!(quoted_path("root.rvy.d*"), "root.rvy.`d*`");
        assert_eq!(quoted_path("root.rvy.`a.b`"), "root.rvy.`a.b`");
        assert_eq!(quoted_path("root.sg.d1"), "root.sg.d1");
        assert_eq!(quoted_path("root.sg.x`y"), "root.sg.`x``y`");
        assert_eq!(measurement_of("root.rvy.`order-items`.`my col`"), "my col");
        assert_eq!(measurement_of("root.sg.d1.a"), "a");
    }

    /// A series whose type isn't known yet stays out of the request; the
    /// rest go with their types.
    #[test]
    fn undecided_series_stay_out_of_tablets() {
        let mut t = Tablet::new(2);
        t.times = vec![1, 2];
        t.values = vec![vec![J::Null, J::Null], vec![json!(1.5), J::Null]];
        let ms = vec!["v".to_string(), "w".to_string()];
        let reqs = t.requests("root.a.d", false, &ms, &[None, Some("DOUBLE".into())]);
        assert_eq!(reqs.len(), 1);
        let body: J = serde_json::from_slice(&reqs[0].1).unwrap();
        assert_eq!(body["measurements"], json!(["w"]));
        assert_eq!(body["data_types"], json!(["DOUBLE"]));
        assert_eq!(body["values"], json!([[1.5, null]]));
        let mut t = Tablet::new(1);
        t.times = vec![1];
        t.values = vec![vec![J::Null]];
        assert!(t.requests("root.a.d", false, &ms[..1], &[None]).is_empty());
        let mut t = Tablet::new(2);
        t.sql_rows = vec![(7, vec![None, Some("X'CA'".into())])];
        let reqs = t.requests("root.a.`d-1`", true, &["v".into(), "h".into()], &[Some("INT64".into()), Some("BLOB".into())]);
        let body: J = serde_json::from_slice(&reqs[0].1).unwrap();
        assert_eq!(body["sql"], json!("INSERT INTO root.a.`d-1`(timestamp, v, h) ALIGNED VALUES (7, null, X'CA')"));
        assert_eq!(reqs[0].2, 1);
    }

    /// Measurements that aren't plain names go backquoted in the tablet,
    /// as in SQL (IoTDB refuses them bare).
    #[test]
    fn tablet_measurements_are_quoted() {
        let mut t = Tablet::new(4);
        t.times = vec![1, 2];
        t.values = vec![vec![json!(1), json!(2)]; 4];
        t.sql_rows = vec![(3, vec![Some("3".into()); 4])];
        let ms: Vec<String> = ["order-id", "first name", "a.b", "x`y"].iter().map(|s| s.to_string()).collect();
        let reqs = t.requests("root.a.d", false, &ms, &vec![Some("INT64".into()); 4]);
        let body: J = serde_json::from_slice(&reqs[0].1).unwrap();
        assert_eq!(body["measurements"], json!(["`order-id`", "`first name`", "`a.b`", "`x``y`"]));
        assert_eq!(reqs[0].2, 2);
        let body: J = serde_json::from_slice(&reqs[1].1).unwrap();
        assert_eq!(body["sql"], json!("INSERT INTO root.a.d(timestamp, `order-id`, `first name`, `a.b`, `x``y`) VALUES (3, 3, 3, 3, 3)"));
        assert_eq!(reqs[1].2, 1);
    }

    /// A load dropped with requests out reports the rows they committed.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_loads_report_what_committed() {
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |n: u64| seen.lock().unwrap().push(n);
        {
            let mut f = Inflight::new(Committed { done: 0, reported: 0, every: 1_000, progress: &progress });
            f.push(tokio::spawn(async { (10, Ok(())) }));
            f.push(tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                (20, Ok(()))
            }));
            f.push(tokio::spawn(async { (5, Err(Error::Query("x".into()))) }));
        }
        assert_eq!(*seen.lock().unwrap(), vec![35]);
    }
}

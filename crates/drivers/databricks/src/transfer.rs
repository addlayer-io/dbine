//! Bulk transfer (see `dbine_driver::transfer`) for Databricks SQL, over the
//! same Statement Execution API as the rest of the driver.
//!
//! Reading: one statement (`SELECT <columns> FROM t [WHERE filter]`) with
//! the `EXTERNAL_LINKS` disposition in `JSON_ARRAY` format, so the result
//! isn't capped at the inline 25 MiB: every chunk is a presigned URL to the
//! workspace's cloud storage, downloaded without the workspace's token (the
//! link's own headers only). [`IN_FLIGHT`] chunks are downloaded at once
//! and handed over in order; each download is streamed and split into
//! blocks of whole rows of about [`BLOCK_BYTES`], with at most
//! [`BLOCKS_QUEUED`] waiting per download, so the memory in flight doesn't
//! depend on the chunk size the server picks (rows are parsed one at a
//! time by the reader). A warehouse that refuses external links gets the
//! same statement `INLINE`, one chunk at a time (the whole inline answer
//! is at most 25 MiB).
//!
//! A statement's result through external links is capped at 100 GiB,
//! counted on Databricks' internal representation (not the table's files),
//! and a bigger one comes back `truncated`. So a table is read in parts of
//! its rows by hash, `pmod(xxhash64(<every column read>), N) = i`, each a
//! statement of its own: N planned up front from `DESCRIBE DETAIL`'s
//! `sizeInBytes` (one metadata statement; a table estimated under
//! [`PART_TARGET`] keeps its single statement), and any part Databricks
//! still cuts is thrown away unread and read again in [`SPLIT`] parts.
//! A Delta table's parts are all read `VERSION AS OF` one version, so they
//! are one snapshot; a view or a table without history is read live (a
//! table changing during the read may then show the change in some parts).
//! Each part's rows are checked against its manifest. Values are typed by the
//! catalog's column type: integers, FLOAT (exact, through `f32`), DOUBLE
//! (also NaN and infinities), DECIMAL as exact text, BOOLEAN, BINARY (base64
//! in the answer) as bytes, DATE, ARRAY/MAP/STRUCT/VARIANT as JSON.
//! TIMESTAMP is an instant: it's selected as
//! `date_format(c, 'yyyy-MM-dd HH:mm:ss.SSSSSSxxx')` (microseconds and the
//! offset, the answer's own format keeps only milliseconds) and
//! TIMESTAMP_NTZ as `CAST(c AS STRING)`. Nested values are selected through
//! `to_json` with microsecond timestamp formats (the same reason). Types
//! without a typed mapping (intervals and the like) are read as
//! `CAST(c AS STRING)` and written back with `CAST('…' AS <type>)`. A
//! truncated answer, or one whose row count doesn't match the manifest, is
//! an error, never a shorter table.
//!
//! Loading: `INSERT INTO t (cols) VALUES (…), (…)`, one statement per
//! commit window of the spec (`commit_rows` / `commit_bytes`), capped at
//! [`STMT_BYTES`] of SQL as escaped in the request's JSON, because the API
//! takes up to 16 MiB per statement: a window wider than that is split into
//! several commits. Each statement is a Delta commit and progress is
//! reported after each one. A statement is always followed to its end: a
//! status poll that fails is retried, and if it keeps failing the
//! statement is cancelled and followed until it ends, so no INSERT is left
//! running that could commit after `bulk_load` returns; when its end can't
//! be confirmed the error says so. The target must be a Delta table: if the
//! load fails after some commit, the table is put back with
//! `RESTORE TABLE … TO VERSION AS OF` the version it had before (only when
//! the later versions are exactly this load's writes, so no one else's
//! change is undone), so a failed `bulk_load` leaves no rows.
//! Values go as Spark literals typed for the target column (exact decimals,
//! `X'…'` binaries, doubles with an exponent, `CAST('…' AS <type>)` for
//! temporal and text values into other types, `from_json` in FAILFAST mode
//! / `parse_json` for nested ones, with `transform_keys` for a map whose
//! keys aren't strings); `${` is never sent inside a literal, so Spark's
//! variable substitution can't rewrite a value. Named parameters carry
//! every value as a string (binaries have no parameter form) and add a
//! JSON object per value to the request.
//! Staging files in a Unity Catalog volume and `COPY INTO` would need a
//! volume the connection doesn't configure; it's not used.

use crate::ddl::{lit, q};
use crate::{results, Api, DatabricksSession};
use base64::Engine as _;
use dbine_driver::transfer::{
    BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn,
};
use dbine_driver::{ColumnInfo, Error, ObjectRef, Result, Session};
use serde_json::{json, Value as Json};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Result chunks downloaded at once (external links).
pub(crate) const IN_FLIGHT: usize = 4;
/// Bytes of whole rows (raw JSON) a download hands over at once.
const BLOCK_BYTES: usize = 1024 * 1024;
/// Blocks a download may have waiting for the reader: with [`IN_FLIGHT`]
/// downloads, about 4 × (1 queued + 1 being filled) MiB of raw JSON are in
/// flight, plus a row wider than a block, which is kept whole.
const BLOCKS_QUEUED: usize = 1;
/// How long one network read of a download may take.
const READ_WAIT: Duration = Duration::from_secs(120);
/// What one part of a table read aims for, as estimated. Databricks gives
/// one statement at most 100 GiB of result through external links ("Result
/// sets larger than this limit will be truncated"), counted on its internal
/// representation, not on the table's files; aiming well under it leaves
/// room for an estimate that falls short.
const PART_TARGET: u64 = 40 << 30;
/// How much bigger than the table's files (`DESCRIBE DETAIL`'s
/// `sizeInBytes`, compressed Parquet) its read is taken to be: the rows
/// decompressed, timestamps read as text and nested values as JSON.
const INFLATE: u64 = 8;
/// A part Databricks still cuts is read again as this many.
const SPLIT: u64 = 4;
/// The finest division of a read (parts of `pmod(hash, MAX_MODULUS)`).
/// Each part scans the whole table, and rows that hash alike (identical
/// rows) can't be spread further.
const MAX_MODULUS: u64 = 1024;
/// Bytes one INSERT takes at most as escaped in the request's JSON (`\`
/// and `"` take two bytes, control characters up to six). The API's limit
/// is 16 MiB; whether it counts the statement's text or the request body
/// isn't documented, and capping the escaped size keeps both under it.
const STMT_BYTES: usize = 12 * 1024 * 1024;
/// Consecutive failed status polls of a load statement before it's
/// cancelled, and again after the cancel before its end is reported as
/// unknown.
const POLL_TRIES: u32 = 6;
/// The first and the longest wait between status polls.
#[cfg(not(test))]
const POLL_WAIT: (Duration, Duration) = (Duration::from_millis(250), Duration::from_secs(8));
#[cfg(test)]
const POLL_WAIT: (Duration, Duration) = (Duration::from_millis(1), Duration::from_millis(4));

/// A result chunk: rows of JSON_ARRAY text values.
type Rows = Vec<Vec<Option<String>>>;

/// How a column is selected, typed back and written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Int,
    /// FLOAT / REAL (32 bits).
    Real,
    Double,
    Decimal,
    Bool,
    Binary,
    Date,
    /// TIMESTAMP (an instant, shown in the session's zone).
    Timestamp,
    TimestampNtz,
    /// ARRAY, MAP, STRUCT.
    Nested,
    Variant,
    /// STRING, VARCHAR, CHAR.
    Text,
    /// Any other type (intervals, geometry…): read as its text, written
    /// with `CAST('…' AS <type>)`.
    Other,
}

/// A column's kind from its catalog type (`decimal(10,2)`, `array<int>`…)
/// or the answer's `type_name` (`LONG`, `SHORT`…).
pub(crate) fn kind_of(ty: &str) -> Kind {
    let t = ty.trim().to_ascii_lowercase();
    let base = t.split(['(', '<', ' ']).next().unwrap_or("").trim();
    match base {
        "tinyint" | "smallint" | "int" | "integer" | "bigint" | "byte" | "short" | "long" => {
            Kind::Int
        }
        "float" | "real" => Kind::Real,
        "double" => Kind::Double,
        "decimal" | "dec" | "numeric" => Kind::Decimal,
        "boolean" => Kind::Bool,
        "binary" => Kind::Binary,
        "date" => Kind::Date,
        "timestamp" | "timestamp_ltz" => Kind::Timestamp,
        "timestamp_ntz" => Kind::TimestampNtz,
        "array" | "map" | "struct" => Kind::Nested,
        "variant" => Kind::Variant,
        "string" | "varchar" | "char" => Kind::Text,
        _ => Kind::Other,
    }
}

/// The SELECT expression of a column read.
pub(crate) fn select_expr(name: &str, kind: Kind) -> String {
    let c = q(name);
    match kind {
        Kind::Timestamp => format!("date_format({c}, 'yyyy-MM-dd HH:mm:ss.SSSSSSxxx') AS {c}"),
        Kind::TimestampNtz | Kind::Other => format!("CAST({c} AS STRING) AS {c}"),
        // The answer's own JSON keeps only milliseconds of nested timestamps.
        Kind::Nested => format!(
            "to_json({c}, map('timestampFormat', {}, 'timestampNTZFormat', {})) AS {c}",
            lit("yyyy-MM-dd'T'HH:mm:ss.SSSSSSXXX"),
            lit("yyyy-MM-dd'T'HH:mm:ss.SSSSSS")
        ),
        _ => c,
    }
}

/// The read's statement.
pub(crate) fn read_sql(
    table: &str,
    names: &[String],
    kinds: &[Kind],
    filter: Option<&str>,
) -> String {
    let list: Vec<String> = names
        .iter()
        .zip(kinds)
        .map(|(n, k)| select_expr(n, *k))
        .collect();
    let mut sql = format!("SELECT {} FROM {table}", list.join(", "));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    sql
}

/// One part of a table read: the rows whose hash over every column read
/// leaves `residue` when divided by `modulus`. Every row has a hash (NULLs
/// are skipped, not propagated), so the parts of one modulus hold each row
/// exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Part {
    pub modulus: u64,
    pub residue: u64,
}

impl Part {
    /// The whole table, in one statement.
    pub(crate) const WHOLE: Part = Part {
        modulus: 1,
        residue: 0,
    };

    /// The table in `n` parts.
    pub(crate) fn all(n: u64) -> impl DoubleEndedIterator<Item = Part> {
        (0..n).map(move |residue| Part {
            modulus: n,
            residue,
        })
    }

    /// This part's rows in [`SPLIT`] parts: the residues of `modulus ×
    /// SPLIT` that leave this one's when divided by `modulus`.
    pub(crate) fn split(self) -> impl DoubleEndedIterator<Item = Part> {
        (0..SPLIT).map(move |j| Part {
            modulus: self.modulus * SPLIT,
            residue: self.residue + j * self.modulus,
        })
    }
}

/// The parts a read is planned in, from its table's files' size (`None`:
/// unknown, so one statement, split if Databricks cuts it).
pub(crate) fn planned_parts(size: Option<u64>) -> u64 {
    size.map_or(1, |b| {
        b.saturating_mul(INFLATE)
            .div_ceil(PART_TARGET)
            .clamp(1, MAX_MODULUS / SPLIT)
    })
}

/// A column as hashed to split a read: nested values as JSON (a map can't
/// be hashed), VARIANT and types without a mapping as text.
fn hash_expr(name: &str, kind: Kind) -> String {
    let c = q(name);
    match kind {
        Kind::Nested => format!("to_json({c})"),
        Kind::Variant | Kind::Other => format!("CAST({c} AS STRING)"),
        _ => c,
    }
}

/// One part's statement: [`read_sql`] at Delta `version` when given, with
/// only the part's rows. The filter goes first, on lines of its own, so a
/// trailing `--` comment in it can't swallow the part's condition.
pub(crate) fn part_sql(
    table: &str,
    version: Option<i64>,
    names: &[String],
    kinds: &[Kind],
    filter: Option<&str>,
    part: Part,
) -> String {
    let from = match version {
        Some(v) => format!("{table} VERSION AS OF {v}"),
        None => table.to_string(),
    };
    if part == Part::WHOLE {
        return read_sql(&from, names, kinds, filter);
    }
    let hashed: Vec<String> = names
        .iter()
        .zip(kinds)
        .map(|(n, k)| hash_expr(n, *k))
        .collect();
    let mut cond = format!(
        "pmod(xxhash64({}), {}) = {}",
        hashed.join(", "),
        part.modulus,
        part.residue
    );
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        cond = format!("(\n{f}\n) AND {cond}");
    }
    read_sql(&from, names, kinds, Some(&cond))
}

/// An answer value that doesn't read as its column's type: an error, never
/// a made-up value.
fn unreadable(s: &str, what: &str) -> Error {
    let shown: String = s.chars().take(60).collect();
    Error::Query(format!("Databricks devolvió «{shown}» en una columna {what} y no se pudo leer; no se copia un valor alterado."))
}

/// The special doubles' text, if `s` is one.
fn special_double(s: &str) -> Option<f64> {
    match s.trim() {
        "NaN" => Some(f64::NAN),
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

fn parse_double(s: &str) -> Result<f64> {
    special_double(s).map_or_else(|| s.trim().parse().map_err(|_| unreadable(s, "DOUBLE")), Ok)
}

/// `2024-01-31T13:45:00.123Z` / `… +00:00` as `YYYY-MM-DD HH:MM:SS[.f]±HH:MM`.
fn zoned(s: &str) -> String {
    let s = s.trim().replacen('T', " ", 1);
    match s.strip_suffix('Z') {
        Some(l) => format!("{l}+00:00"),
        None => s,
    }
}

/// A JSON_ARRAY value (text or NULL) as a cell, given its column's kind.
pub(crate) fn to_cell(v: Option<String>, kind: Kind) -> Result<Cell> {
    let Some(s) = v else { return Ok(Cell::Null) };
    Ok(match kind {
        Kind::Int => Cell::Int(
            s.trim()
                .parse::<i64>()
                .map_err(|_| unreadable(&s, "entera"))?,
        ),
        Kind::Real => Cell::Float(match special_double(&s) {
            Some(f) => f,
            // The float's own value, not the double nearest to its text.
            None => f64::from(
                s.trim()
                    .parse::<f32>()
                    .map_err(|_| unreadable(&s, "FLOAT"))?,
            ),
        }),
        Kind::Double => Cell::Float(parse_double(&s)?),
        Kind::Decimal => Cell::Decimal(s),
        Kind::Bool => match s.trim() {
            b if b.eq_ignore_ascii_case("true") => Cell::Bool(true),
            b if b.eq_ignore_ascii_case("false") => Cell::Bool(false),
            _ => return Err(unreadable(&s, "BOOLEAN")),
        },
        Kind::Binary => Cell::Bytes(
            base64::engine::general_purpose::STANDARD
                .decode(&s)
                .map_err(|_| unreadable(&s, "BINARY"))?,
        ),
        Kind::Date => Cell::Date(s),
        Kind::Timestamp => Cell::DateTimeTz(zoned(&s)),
        Kind::TimestampNtz => Cell::DateTime(s.replacen('T', " ", 1)),
        Kind::Nested | Kind::Variant => Cell::Json(s),
        Kind::Text | Kind::Other => Cell::Text(s),
    })
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// Digits with an optional sign and point: a Spark exact numeric literal.
fn plain_number(s: &str) -> Option<&str> {
    let t = s.trim();
    let t = t.strip_prefix('+').unwrap_or(t);
    let digits = t.strip_prefix('-').unwrap_or(t);
    let ok = !digits.is_empty()
        && digits.chars().any(|c| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1;
    ok.then_some(t)
}

/// A double literal (exponent notation is a DOUBLE in Spark; shortest
/// round-trip digits).
fn double_sql(f: f64) -> String {
    if f.is_nan() {
        "CAST('NaN' AS DOUBLE)".into()
    } else if f.is_infinite() {
        format!(
            "CAST('{}Infinity' AS DOUBLE)",
            if f < 0.0 { "-" } else { "" }
        )
    } else {
        format!("{f:e}")
    }
}

/// A cell's text, for text targets and casts.
fn cell_text(c: &Cell) -> Option<String> {
    Some(match c {
        Cell::Null => return None,
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_nan() => "NaN".into(),
        Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => format!("0x{}", hex(b)),
        },
        Cell::Decimal(s)
        | Cell::Text(s)
        | Cell::Date(s)
        | Cell::Time(s)
        | Cell::DateTime(s)
        | Cell::DateTimeTz(s)
        | Cell::Uuid(s)
        | Cell::Json(s) => s.clone(),
    })
}

/// A string literal that never holds `${`: Spark's variable substitution
/// rewrites `${key}` in the statement's text before parsing it, so the
/// literal is split there into adjacent literals (`'a$' '{b}'`), which
/// Spark concatenates.
pub(crate) fn slit(s: &str) -> String {
    if !s.contains("${") {
        return lit(s);
    }
    let parts: Vec<&str> = s.split("${").collect();
    let mut out = String::with_capacity(s.len() + 4 * parts.len());
    for (i, p) in parts.iter().enumerate() {
        let mut seg = String::with_capacity(p.len() + 2);
        if i > 0 {
            out.push(' ');
            seg.push('{');
        }
        seg.push_str(p);
        if i + 1 < parts.len() {
            seg.push('$');
        }
        out.push_str(&lit(&seg));
    }
    out
}

/// `0xCAFE` (how grid text shows binaries) as its bytes.
fn hex_text(s: &str) -> Option<Vec<u8>> {
    let h = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))?;
    if !h.len().is_multiple_of(2) || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok())
        .collect()
}

/// The key type of every `map<K, V>` in a type (`array<map<int,string>>`
/// gives `["int"]`), outermost first.
fn map_keys(ty: &str) -> Vec<String> {
    let b = ty.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let boundary = i == 0 || matches!(b[i - 1], b'<' | b',' | b':' | b' ');
        if boundary && b[i..i + 4].eq_ignore_ascii_case(b"map<") {
            let start = i + 4;
            let mut depth = 0i32;
            let mut j = start;
            while j < b.len() {
                match b[j] {
                    b'<' | b'(' => depth += 1,
                    b'>' | b')' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            keys.push(ty[start..j.min(b.len())].trim().to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    keys
}

fn is_string_type(ty: &str) -> bool {
    matches!(kind_of(ty), Kind::Text)
}

/// A map key type in `ty` that `to_json` doesn't write faithfully, if any.
/// Spark writes a JSON key with the key's internal `toString`: text,
/// numbers and booleans come out as themselves, but a binary key comes out
/// as a JVM object name, and dates and timestamps as day or microsecond
/// counts, so they can't be read back.
pub(crate) fn unreadable_map_key(ty: &str) -> Option<String> {
    map_keys(ty).into_iter().find(|k| {
        !matches!(
            kind_of(k),
            Kind::Text | Kind::Int | Kind::Bool | Kind::Decimal | Kind::Real | Kind::Double
        )
    })
}

/// Whether a key's JSON text converts to key type `key` without doubt:
/// a binary key has no single text form (UTF-8? hex? base64?), and a
/// nested or other type isn't a key `CAST` can build from text.
fn loadable_map_key(key: &str) -> bool {
    matches!(
        kind_of(key),
        Kind::Text
            | Kind::Int
            | Kind::Bool
            | Kind::Decimal
            | Kind::Real
            | Kind::Double
            | Kind::Date
            | Kind::Timestamp
            | Kind::TimestampNtz
    )
}

/// How a nested column's JSON is loaded: `from_json` only takes maps with
/// string keys, so a map with other keys is read as `map<string, V>` and
/// its keys cast back with `transform_keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonLoad {
    Plain,
    /// A top-level `map<key, value>` whose key isn't a string.
    Keys {
        key: String,
        value: String,
    },
}

/// A target column: its kind, catalog type and, if nested, how its JSON
/// is loaded.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub kind: Kind,
    pub ty: String,
    pub json: JsonLoad,
}

/// The target column of catalog type `ty` (`name` for the error).
pub(crate) fn target(name: &str, ty: &str) -> Result<Target> {
    let kind = kind_of(ty);
    let mut json = JsonLoad::Plain;
    if kind == Kind::Nested {
        let keys = map_keys(ty);
        let t = ty.trim();
        let top_map = t.len() > 5 && t[..4].eq_ignore_ascii_case("map<") && t.ends_with('>');
        let odd: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter(|(_, k)| !is_string_type(k))
            .map(|(i, _)| i)
            .collect();
        match odd.as_slice() {
            [] => {}
            [0] if top_map && !loadable_map_key(&keys[0]) => {
                return Err(Error::Unsupported(format!(
                    "La columna {name} ({ty}) es un mapa con claves {}: las claves de un JSON son texto y no hay una forma única de convertirlas a ese tipo sin alterarlas, así que no se carga en forma masiva.",
                    keys[0]
                )))
            }
            [0] if top_map => {
                let key = keys[0].clone();
                let inner = &t[4..t.len() - 1];
                let value = inner.trim_start()[key.len()..].trim_start().trim_start_matches(',').trim().to_string();
                json = JsonLoad::Keys { key, value };
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "La columna {name} ({ty}) tiene un mapa con claves que no son texto dentro de otro tipo anidado; Databricks solo lee JSON en mapas con claves de texto y DBine convierte las claves únicamente en un mapa del primer nivel, así que no se carga en forma masiva."
                )))
            }
        }
    }
    Ok(Target {
        kind,
        ty: ty.to_string(),
        json,
    })
}

/// A cell as a Spark literal for the target column `t`.
pub(crate) fn value_sql(c: &Cell, t: &Target) -> String {
    let (kind, ty) = (t.kind, t.ty.as_str());
    let cast = |s: &str| format!("CAST({} AS {ty})", slit(s));
    match (c, kind) {
        (Cell::Null, _) => "NULL".into(),
        (Cell::Bytes(b), Kind::Binary) => format!("X'{}'", hex(b)),
        (_, Kind::Binary) => {
            let s = cell_text(c).unwrap_or_default();
            format!("X'{}'", hex(&hex_text(&s).unwrap_or_else(|| s.into_bytes())))
        }
        (_, Kind::Text) => slit(&cell_text(c).unwrap_or_default()),
        (Cell::Bool(b), Kind::Bool) => if *b { "TRUE" } else { "FALSE" }.into(),
        (Cell::Int(i), Kind::Bool) => if *i != 0 { "TRUE" } else { "FALSE" }.into(),
        (Cell::UInt(u), Kind::Bool) => if *u != 0 { "TRUE" } else { "FALSE" }.into(),
        (Cell::Bool(b), Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) => if *b { "1" } else { "0" }.into(),
        (Cell::Int(i), Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) => i.to_string(),
        (Cell::UInt(u), Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) => u.to_string(),
        (Cell::Float(f), Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) => double_sql(*f),
        (Cell::Decimal(s) | Cell::Text(s), Kind::Int | Kind::Real | Kind::Double | Kind::Decimal) => match plain_number(s) {
            Some(n) => n.to_string(),
            None => cast(s),
        },
        // FAILFAST: malformed JSON or a value that doesn't fit the type is
        // an error, not a silent NULL.
        (Cell::Json(s) | Cell::Text(s), Kind::Nested) => match &t.json {
            JsonLoad::Plain => format!("from_json({}, {}, map('mode', 'FAILFAST'))", slit(s), lit(ty)),
            JsonLoad::Keys { key, value } => format!(
                "transform_keys(from_json({}, {}, map('mode', 'FAILFAST')), (k, v) -> CAST(k AS {key}))",
                slit(s),
                lit(&format!("map<string,{value}>"))
            ),
        },
        (Cell::Json(s) | Cell::Text(s), Kind::Variant) => format!("parse_json({})", slit(s)),
        (Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) | Cell::Bool(_), Kind::Variant) => {
            format!("parse_json({})", slit(&c.to_json().to_string()))
        }
        _ => cast(&cell_text(c).unwrap_or_default()),
    }
}

/// A JSON_ARRAY value as text (NULL as `None`).
fn json_text(v: Json) -> Option<String> {
    match v {
        Json::Null => None,
        Json::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

/// Rows a chunk's download hands over: raw JSON with each whole row's byte
/// range (external links, parsed a row at a time by the reader), or rows
/// already parsed (an inline chunk).
#[derive(Debug)]
pub(crate) enum Block {
    Raw(Vec<u8>, Vec<(usize, usize)>),
    Rows(Rows),
}

impl Block {
    /// Its rows in order, as text (`None`: NULL), parsed one at a time;
    /// `each` says whether to go on, and so does the answer.
    pub(crate) fn each_row(
        self,
        mut each: impl FnMut(Vec<Option<String>>) -> Result<bool>,
    ) -> Result<bool> {
        match self {
            Block::Rows(rows) => {
                for r in rows {
                    if !each(r)? {
                        return Ok(false);
                    }
                }
            }
            Block::Raw(data, ranges) => {
                for (a, b) in ranges {
                    let values: Vec<Json> = serde_json::from_slice(&data[a..b])
                        .map_err(|e| Error::Query(format!("bloque del resultado ilegible: {e}")))?;
                    if !each(values.into_iter().map(json_text).collect())? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

/// Splits a streamed JSON array of row arrays (`[[…],[…]]`) into blocks of
/// whole rows, without parsing the values: only strings and brackets are
/// followed.
#[derive(Default)]
pub(crate) struct Splitter {
    buf: Vec<u8>,
    scanned: usize,
    depth: u32,
    in_str: bool,
    esc: bool,
    row_start: usize,
    rows: Vec<(usize, usize)>,
    /// End of the last whole row in `buf`.
    done: usize,
    opened: bool,
    closed: bool,
}

fn not_rows() -> Error {
    Error::Query("bloque del resultado ilegible: no es un arreglo JSON de filas".into())
}

impl Splitter {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        for i in self.scanned..self.buf.len() {
            let b = self.buf[i];
            if self.in_str {
                if self.esc {
                    self.esc = false;
                } else if b == b'\\' {
                    self.esc = true;
                } else if b == b'"' {
                    self.in_str = false;
                }
                continue;
            }
            match b {
                b'[' => {
                    if self.depth == 0 {
                        if self.opened {
                            return Err(not_rows());
                        }
                        self.opened = true;
                    }
                    self.depth += 1;
                    if self.depth == 2 {
                        self.row_start = i;
                    }
                }
                b']' => {
                    match self.depth {
                        0 => return Err(not_rows()),
                        1 => self.closed = true,
                        2 => {
                            self.rows.push((self.row_start, i + 1));
                            self.done = i + 1;
                        }
                        _ => {}
                    }
                    self.depth -= 1;
                }
                b' ' | b'\n' | b'\r' | b'\t' => {}
                b',' if self.depth == 1 => {}
                b'"' if self.depth >= 2 => self.in_str = true,
                _ if self.depth >= 2 => {}
                _ => return Err(not_rows()),
            }
        }
        self.scanned = self.buf.len();
        Ok(())
    }

    /// The whole rows so far, once they take `min` bytes.
    pub(crate) fn block(&mut self, min: usize) -> Option<Block> {
        (self.done >= min && !self.rows.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> Block {
        let rest = self.buf.split_off(self.done);
        let data = std::mem::replace(&mut self.buf, rest);
        self.scanned -= self.done;
        if self.depth >= 2 {
            self.row_start -= self.done;
        }
        self.done = 0;
        Block::Raw(data, std::mem::take(&mut self.rows))
    }

    /// The last rows; an array that didn't close is an error (a cut
    /// download), never a shorter chunk.
    pub(crate) fn finish(mut self) -> Result<Option<Block>> {
        if !self.closed || self.in_str {
            return Err(not_rows());
        }
        Ok((!self.rows.is_empty()).then(|| self.take()))
    }
}

fn download_error(e: impl std::fmt::Display) -> Error {
    Error::Connect(format!("no se pudo descargar un bloque del resultado: {e}"))
}

/// A failed download, without its URL: a link carries a temporary credential
/// and this text reaches the screen and the logs.
fn link_error(e: reqwest::Error) -> Error {
    download_error(e.without_url())
}

/// Hand a chunk's rows over to `tx`: inline (`data_array`) or streamed from
/// its external links (fetched without the workspace token), in blocks.
/// Stops quietly if the reader is gone.
async fn send_chunk(
    http: &reqwest::Client,
    mut chunk: Json,
    tx: &mpsc::Sender<Result<Block>>,
) -> Result<()> {
    if let Some(Json::Array(rows)) = chunk.get_mut("data_array").map(Json::take) {
        let rows = rows
            .into_iter()
            .map(|r| match r {
                Json::Array(values) => values.into_iter().map(json_text).collect(),
                _ => Vec::new(),
            })
            .collect();
        let _ = tx.send(Ok(Block::Rows(rows))).await;
        return Ok(());
    }
    for link in chunk
        .get("external_links")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let Some(url) = link.get("external_link").and_then(Json::as_str) else {
            continue;
        };
        // The client's overall timeout would cut a download that waits for
        // the reader: only each network read is timed.
        let mut req = http.get(url).timeout(Duration::from_secs(7 * 24 * 3600));
        for (k, v) in link
            .get("http_headers")
            .and_then(Json::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(v) = v.as_str() {
                req = req.header(k.as_str(), v);
            }
        }
        let mut resp = tokio::time::timeout(READ_WAIT, req.send())
            .await
            .map_err(download_error)?
            .map_err(link_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Query(format!(
                "no se pudo descargar un bloque del resultado: HTTP {}",
                status.as_u16()
            )));
        }
        let mut split = Splitter::default();
        while let Some(piece) = tokio::time::timeout(READ_WAIT, resp.chunk())
            .await
            .map_err(download_error)?
            .map_err(link_error)?
        {
            // In parts, so a block ends near BLOCK_BYTES whatever the size
            // of the network's pieces.
            for part in piece.chunks(64 * 1024) {
                split.feed(part)?;
                if let Some(b) = split.block(BLOCK_BYTES) {
                    if tx.send(Ok(b)).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if let Some(b) = split.finish()? {
            if tx.send(Ok(b)).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A chunk's download: its blocks, in order, and its task, aborted when
/// the download is dropped (an error, a sink failure, or the read's future
/// dropped on a cancel): a dropped `JoinHandle` only detaches its task.
struct Download {
    rx: mpsc::Receiver<Result<Block>>,
    task: JoinHandle<()>,
}

impl Drop for Download {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start downloading chunk `i` of a finished statement (`first`: chunk 0,
/// already in the final status).
fn fetch(api: &Api, id: &str, i: u64, first: Option<Json>) -> Download {
    let (api, id) = (api.clone(), id.to_string());
    let (tx, rx) = mpsc::channel(BLOCKS_QUEUED);
    let task = tokio::spawn(async move {
        let r = async {
            let chunk = match first {
                Some(c) => c,
                None => {
                    api.get(&format!("/api/2.0/sql/statements/{id}/result/chunks/{i}"))
                        .await?
                }
            };
            send_chunk(&api.http, chunk, &tx).await
        }
        .await;
        if let Err(e) = r {
            let _ = tx.send(Err(e)).await;
        }
    });
    Download { rx, task }
}

/// Whether Databricks cut a finished statement's result short.
fn truncated(resp: &Json) -> bool {
    resp.pointer("/manifest/truncated").and_then(Json::as_bool) == Some(true)
}

/// The chunks to read and the rows the manifest promises, from a finished
/// statement's status. A truncated result or a manifest without its chunk
/// count is an error: the table would be read short (a comparison, a copy
/// or an export of part of it). `refused`: what the warehouse answered when
/// the read asked for external links, if it read inline instead. (A table
/// read never gets here with a result cut over external links: it reads
/// that part again in smaller ones.)
pub(crate) fn chunk_plan(resp: &Json, refused: Option<&str>) -> Result<(u64, Option<u64>)> {
    if truncated(resp) {
        return Err(match refused {
            Some(why) => inline_too_big(why),
            None => Error::Query(
                "Databricks cortó el resultado en su límite de 100 GiB por lectura (medidos en su formato interno, no en lo que ocupa la tabla); no se usa una lectura incompleta.".into(),
            ),
        });
    }
    chunk_count(resp).ok_or_else(|| Error::Query("La respuesta de Databricks no dice cuántos bloques tiene el resultado; no se usa una lectura que podría quedar incompleta.".into()))
}

/// A table that doesn't fit in Databricks' reads even in `parts` parts
/// (rows that hash alike, or a table far past what a read can bring).
fn too_big(parts: u64) -> Error {
    Error::Query(format!(
        "La tabla no entra en el límite de Databricks de 100 GiB por lectura (medidos en su formato interno, no en lo que ocupa la tabla) ni dividida en {parts} partes; no se usa una lectura incompleta."
    ))
}

/// What `DESCRIBE DETAIL` tells of a table, to plan its read.
#[derive(Debug, Clone, Copy)]
struct Detail {
    /// Its files' size (compressed Parquet).
    size: Option<u64>,
    /// Whether it's Delta (then its parts are read at one version).
    delta: bool,
}

/// How a table is read: what [`part_sql`] takes, and the parts planned.
pub(crate) struct TableRead<'a> {
    pub table: &'a str,
    pub names: &'a [String],
    pub kinds: &'a [Kind],
    pub filter: Option<&'a str>,
    pub parts: u64,
    /// The Delta version every part is read at (`None`: live).
    pub version: Option<i64>,
    /// Delta: a read that has to be split takes a version first.
    pub delta: bool,
}

/// A part's statement, finished.
enum Submitted {
    Ready(Ready),
    /// Cut by Databricks at its limit per read: nothing of it is used.
    Cut,
}

/// A finished part to read: its id, chunk 0 when the status brought it,
/// its chunks, the rows its manifest promises and whether it came by
/// external links.
struct Ready {
    id: String,
    first: Option<Json>,
    chunks: u64,
    expected: Option<u64>,
    external: bool,
}

/// A table over the inline 25 MiB, read inline because the warehouse
/// refused external links: says what it answered, which is what to fix.
fn inline_too_big(refused: &str) -> Error {
    Error::Query(format!(
        "La tabla pasa los 25 MiB que Databricks entrega en la respuesta, y Databricks rechazó entregarla por enlaces externos, que no tienen ese límite: {refused}. No se usa una lectura incompleta."
    ))
}

/// The chunks of a finished statement and the rows its manifest promises.
pub(crate) fn chunk_count(resp: &Json) -> Option<(u64, Option<u64>)> {
    let rows = resp
        .pointer("/manifest/total_row_count")
        .and_then(Json::as_u64);
    match (
        resp.pointer("/manifest/total_chunk_count")
            .and_then(Json::as_u64),
        rows,
    ) {
        (Some(c), _) => Some((c, rows)),
        (None, Some(0)) => Some((0, rows)),
        _ => None,
    }
}

/// Chunk 0 of a finished statement, when its final status brought it.
pub(crate) fn first_chunk(resp: &mut Json) -> Option<Json> {
    resp.get_mut("result")
        .map(Json::take)
        .filter(|r| r.get("chunk_index").and_then(Json::as_u64).unwrap_or(0) == 0)
}

/// The version of the latest Delta commit, from `DESCRIBE HISTORY`'s rows.
fn history_version(st: &crate::Statement) -> Option<i64> {
    let i = st
        .columns
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case("version"))?;
    json_i64(st.rows.first()?.get(i)?)
}

fn json_i64(v: &Json) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Whether the versions after `before` (`DESCRIBE HISTORY`'s rows, newest
/// first) can be undone: each is a write of `user`, and there are exactly
/// `sent` of them (the load's committed statements).
pub(crate) fn only_our_writes(st: &crate::Statement, before: i64, user: &str, sent: usize) -> bool {
    let col = |name: &str| {
        st.columns
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(name))
    };
    let (Some(v), Some(op), Some(who)) = (col("version"), col("operation"), col("userName")) else {
        return false;
    };
    let later: Vec<&Vec<Json>> = st
        .rows
        .iter()
        .filter(|r| r.get(v).and_then(json_i64).is_some_and(|n| n > before))
        .collect();
    later.len() == sent
        && later.iter().all(|r| {
            r.get(op).and_then(Json::as_str) == Some("WRITE")
                && r.get(who).and_then(Json::as_str).is_some_and(|u| u == user)
        })
}

/// The `DESCRIBE HISTORY` that `undo_load` reads and the rows it keeps:
/// every version the load committed plus the one before and one more, so a
/// load of thousands of statements is checked whole (not cut at a fixed cap).
pub(crate) fn undo_history_query(table: &str, committed: usize) -> (String, usize) {
    let n = committed.saturating_add(2);
    (format!("DESCRIBE HISTORY {table} LIMIT {n}"), n)
}

/// The table's columns, by name (case-insensitive), or an error naming
/// the missing one.
fn pick<'a>(described: &'a [ColumnInfo], table: &str, name: &str) -> Result<&'a ColumnInfo> {
    described
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::Query(format!("La tabla {table} no tiene la columna {name}.")))
}

impl DatabricksSession {
    /// Submit `sql` and wait for it (external links first, inline if the
    /// warehouse refuses them); gives its id, final status and, when it was
    /// read inline, what the warehouse answered to the links.
    async fn submit_read(&self, sql: &str) -> Result<(String, Json, Option<String>)> {
        let mut body = self.request(sql, None);
        body["disposition"] = json!("EXTERNAL_LINKS");
        // A workspace can have links disabled, on submitting or once
        // submitted. Were it a real sign-in or SQL problem, the inline read
        // gives it again, as the server says it.
        let refused = match self.api.post("/api/2.0/sql/statements", &body).await {
            Ok(r) => match self.follow(r).await {
                Ok((id, resp)) => return Ok((id, resp, None)),
                Err(Error::Query(m)) if results::names_links(&m) => m,
                Err(e) => return Err(e),
            },
            Err(Error::Query(m) | Error::AuthFailed(m)) => m,
            Err(e) => return Err(e),
        };
        tracing::warn!("databricks external links refused, reading inline: {refused}");
        let resp = self
            .api
            .post("/api/2.0/sql/statements", &self.request(sql, None))
            .await?;
        match self.follow(resp).await {
            Ok((id, resp)) => Ok((id, resp, Some(refused))),
            Err(Error::Query(m)) if results::over_inline_limit(&m) => Err(inline_too_big(&refused)),
            Err(e) => Err(e),
        }
    }

    /// A submitted statement, followed to its end: its id (now the running
    /// one) and its final status.
    pub(crate) async fn follow(&self, mut resp: Json) -> Result<(String, Json)> {
        let id = resp
            .get("statement_id")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        self.set_running(Some(id.clone()));
        let mut delay = Duration::from_millis(250);
        loop {
            match resp.pointer("/status/state").and_then(Json::as_str) {
                Some("SUCCEEDED") => return Ok((id, resp)),
                Some("FAILED") => {
                    let msg = resp
                        .pointer("/status/error/message")
                        .and_then(Json::as_str)
                        .unwrap_or("la sentencia falló");
                    return Err(Error::Query(msg.to_string()));
                }
                Some("CANCELED") | Some("CLOSED") => return Err(Error::Cancelled),
                _ => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                    resp = self
                        .api
                        .get(&format!("/api/2.0/sql/statements/{id}"))
                        .await?;
                }
            }
        }
    }

    pub(crate) async fn transfer_read(
        &mut self,
        spec: &ReadSpec,
        sink: BatchSinkRef,
    ) -> Result<u64> {
        let described = self.columns(&spec.table).await?;
        if described.is_empty() {
            return Err(Error::Query(format!(
                "No se encontró la tabla {}.",
                spec.table.name
            )));
        }
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => described.iter().map(|c| c.name.clone()).collect(),
        };
        let mut cols = Vec::with_capacity(names.len());
        for n in &names {
            let c = pick(&described, &spec.table.name, n)?;
            if kind_of(&c.data_type) == Kind::Nested {
                if let Some(k) = unreadable_map_key(&c.data_type) {
                    return Err(Error::Unsupported(format!(
                        "La columna {n} ({}) tiene un mapa con claves {k}: Databricks las escribe en JSON como su valor interno (un nombre de objeto para las binarias, un número de días o de microsegundos para fechas y marcas de tiempo), así que no vuelven a su valor y no se copia en forma masiva.",
                        c.data_type
                    )));
                }
            }
            cols.push(TransferColumn {
                name: n.clone(),
                type_name: c.data_type.clone(),
                nullable: c.nullable,
            });
        }
        let kinds: Vec<Kind> = cols.iter().map(|c| kind_of(&c.type_name)).collect();
        let table = self.fq(&spec.table)?;
        let r = self
            .read_rows(
                &spec.table,
                &table,
                &names,
                &kinds,
                spec.filter.as_deref(),
                &cols,
                sink,
            )
            .await;
        self.set_running(None);
        r
    }

    /// Plan a table's read and do it. A table (not a view) is first sized
    /// from its Delta log (`DESCRIBE DETAIL`, one metadata statement): one
    /// whose read is estimated past [`PART_TARGET`] is read in parts from
    /// the start, at one Delta version, instead of having Databricks
    /// compute up to 100 GiB of a result it would cut.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn read_rows(
        &self,
        obj: &ObjectRef,
        table: &str,
        names: &[String],
        kinds: &[Kind],
        filter: Option<&str>,
        cols: &[TransferColumn],
        sink: BatchSinkRef,
    ) -> Result<u64> {
        let detail = if obj.kind == dbine_driver::kinds::TABLE {
            self.table_detail(table).await?
        } else {
            None
        };
        let parts = planned_parts(detail.and_then(|d| d.size));
        let delta = detail.is_some_and(|d| d.delta);
        let version = if parts > 1 && delta {
            self.snapshot_version(table).await?
        } else {
            None
        };
        if parts > 1 {
            tracing::info!("databricks: reading {table} in {parts} parts (version {version:?})");
        }
        let read = TableRead {
            table,
            names,
            kinds,
            filter,
            parts,
            version,
            delta,
        };
        self.read_table(&read, cols, sink).await
    }

    /// The table's files' size and format (`DESCRIBE DETAIL`, from the
    /// Delta log); `None` when it doesn't answer (not Delta, no rights…),
    /// and the read goes in one statement.
    async fn table_detail(&self, table: &str) -> Result<Option<Detail>> {
        let st = match self.run(&format!("DESCRIBE DETAIL {table}"), 1, None).await {
            Ok(st) => st,
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                tracing::debug!("databricks: DESCRIBE DETAIL of {table} failed: {e}");
                return Ok(None);
            }
        };
        let col = |name: &str| {
            st.columns
                .iter()
                .position(|(n, _)| n.eq_ignore_ascii_case(name))
        };
        let Some(row) = st.rows.first() else {
            return Ok(None);
        };
        let format = col("format").and_then(|i| row.get(i)?.as_str().map(str::to_ascii_lowercase));
        let size = col("sizeInBytes")
            .and_then(|i| json_i64(row.get(i)?))
            .and_then(|n| u64::try_from(n).ok());
        Ok(Some(Detail {
            size,
            delta: format.as_deref() == Some("delta"),
        }))
    }

    /// The Delta version to read every part at, so the parts are one
    /// snapshot of the table; `None` (the parts read live) when its history
    /// can't be read (a shared table without history, no rights…).
    async fn snapshot_version(&self, table: &str) -> Result<Option<i64>> {
        match self
            .run(&format!("DESCRIBE HISTORY {table} LIMIT 1"), 1, None)
            .await
        {
            Ok(st) => Ok(history_version(&st)),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(e) => {
                tracing::warn!(
                    "databricks: Delta version of {table} unknown, parts read live: {e}"
                );
                Ok(None)
            }
        }
    }

    /// Read `read` into `sink`, part by part. A part Databricks cuts at its
    /// limit per read is never used: it's read again in [`SPLIT`] parts (at
    /// one Delta version from then on, when the table is Delta), down to
    /// [`MAX_MODULUS`]. Each part's rows are checked against its manifest,
    /// so what reaches `sink` is every row exactly once or an error.
    pub(crate) async fn read_table(
        &self,
        read: &TableRead<'_>,
        cols: &[TransferColumn],
        sink: BatchSinkRef,
    ) -> Result<u64> {
        sink.lock()
            .map_err(|_| Error::State("destino de lotes".into()))?
            .begin(cols)?;
        let mut builder = BatchBuilder::new();
        let mut version = read.version;
        let mut todo: VecDeque<Part> = Part::all(read.parts).collect();
        while let Some(part) = todo.pop_front() {
            let sql = part_sql(
                read.table,
                version,
                read.names,
                read.kinds,
                read.filter,
                part,
            );
            match self.submit_part(&sql).await? {
                Submitted::Ready(r) => self.read_part(r, read.kinds, &sink, &mut builder).await?,
                Submitted::Cut => {
                    if part.modulus * SPLIT > MAX_MODULUS {
                        return Err(too_big(part.modulus));
                    }
                    // Nothing was read yet: from here on, one snapshot.
                    if part == Part::WHOLE && read.delta {
                        version = self.snapshot_version(read.table).await?;
                    }
                    tracing::info!(
                        "databricks: part {}/{} of {} cut at the read limit, read in {SPLIT}",
                        part.residue,
                        part.modulus,
                        read.table
                    );
                    for p in part.split().rev() {
                        todo.push_front(p);
                    }
                }
            }
        }
        builder.flush(
            &mut *sink
                .lock()
                .map_err(|_| Error::State("destino de lotes".into()))?,
        )?;
        Ok(builder.rows)
    }

    /// Submit a part's statement and wait for it: ready to read, or cut by
    /// Databricks (over external links: the inline answer's limit is an
    /// error saying why the links were refused).
    async fn submit_part(&self, sql: &str) -> Result<Submitted> {
        let (id, mut resp, refused) = self.submit_read(sql).await?;
        if refused.is_none() && truncated(&resp) {
            return Ok(Submitted::Cut);
        }
        let (chunks, expected) = chunk_plan(&resp, refused.as_deref())?;
        Ok(Submitted::Ready(Ready {
            first: first_chunk(&mut resp),
            id,
            chunks,
            expected,
            external: refused.is_none(),
        }))
    }

    /// A part's rows into `builder` (and `sink`), checked against what its
    /// manifest promises.
    async fn read_part(
        &self,
        r: Ready,
        kinds: &[Kind],
        sink: &BatchSinkRef,
        builder: &mut BatchBuilder,
    ) -> Result<()> {
        let before = builder.rows;
        // An inline chunk comes parsed whole: one at a time.
        let in_flight = if r.external { IN_FLIGHT } else { 1 };
        let cells = |values: Vec<Option<String>>| {
            let mut values = values.into_iter();
            kinds
                .iter()
                .map(|k| to_cell(values.next().flatten(), *k))
                .collect::<Result<Vec<Cell>>>()
        };
        self.each_block(&r.id, r.first, r.chunks, in_flight, &mut |block| {
            let mut s = sink
                .lock()
                .map_err(|_| Error::State("destino de lotes".into()))?;
            block.each_row(|row| {
                builder.push(cells(row)?, &mut *s)?;
                Ok(true)
            })
        })
        .await?;
        let read = builder.rows - before;
        if let Some(n) = r.expected.filter(|n| *n != read) {
            return Err(Error::Query(format!(
                "Databricks anunció {n} filas y se leyeron {read}; no se usa una lectura incompleta."
            )));
        }
        Ok(())
    }

    /// A finished statement's chunks in order, up to `in_flight` of them
    /// downloading at once. `each` takes every block and says whether to go
    /// on; stopping drops the downloads under way.
    pub(crate) async fn each_block(
        &self,
        id: &str,
        mut first: Option<Json>,
        chunks: u64,
        in_flight: usize,
        each: &mut (dyn FnMut(Block) -> Result<bool> + Send),
    ) -> Result<()> {
        let mut pending: VecDeque<Download> = VecDeque::new();
        let mut next = 0u64;
        loop {
            while pending.len() < in_flight && next < chunks {
                pending.push_back(fetch(
                    &self.api,
                    id,
                    next,
                    if next == 0 { first.take() } else { None },
                ));
                next += 1;
            }
            let Some(mut d) = pending.pop_front() else {
                return Ok(());
            };
            while let Some(block) = d.rx.recv().await {
                let block = match block {
                    Ok(b) => b,
                    Err(e) => return Err(self.cancelled_or(id, e).await),
                };
                if !each(block)? {
                    return Ok(());
                }
            }
            // The channel closed: the download ended, or its task died.
            if let Err(e) = (&mut d.task).await {
                return Err(Error::State(format!("lectura interrumpida: {e}")));
            }
        }
    }

    /// A chunk that failed after the statement was cancelled is the cancel,
    /// not a download error.
    async fn cancelled_or(&self, id: &str, e: Error) -> Error {
        if matches!(e, Error::Cancelled) {
            return e;
        }
        match self.api.get(&format!("/api/2.0/sql/statements/{id}")).await {
            Ok(r) if r.pointer("/status/state").and_then(Json::as_str) == Some("CANCELED") => {
                Error::Cancelled
            }
            _ => e,
        }
    }

    /// The Delta version the target has now (`DESCRIBE HISTORY`); a table
    /// that isn't Delta can't be put back after a failed load, so it's
    /// refused.
    async fn delta_version(&self, table: &str, shown: &str) -> Result<i64> {
        let detail = self
            .run(&format!("DESCRIBE DETAIL {table}"), 1, None)
            .await?;
        let format = detail
            .columns
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case("format"))
            .and_then(|i| {
                detail
                    .rows
                    .first()?
                    .get(i)?
                    .as_str()
                    .map(str::to_ascii_lowercase)
            });
        if format.as_deref() != Some("delta") {
            return Err(Error::Unsupported(format!(
                "La tabla {shown} no es Delta (formato {}): la carga masiva de Databricks confirma por tandas y solo en una tabla Delta puede deshacerlas si la carga falla.",
                format.as_deref().unwrap_or("desconocido")
            )));
        }
        let history = self
            .run(&format!("DESCRIBE HISTORY {table} LIMIT 1"), 1, None)
            .await?;
        history_version(&history).ok_or_else(|| {
            Error::Query(format!(
                "No se pudo leer la versión Delta de la tabla {shown}."
            ))
        })
    }

    /// After a failed load that committed `committed` statements: put the
    /// table back at version `before`, if the later versions are exactly
    /// this load's writes. Gives the error to return.
    async fn undo_load(&self, table: &str, before: i64, committed: usize, e: Error) -> Error {
        let r: Result<()> = async {
            let (sql, max_rows) = undo_history_query(table, committed);
            let history = self.run(&sql, max_rows, None).await?;
            if history.more {
                return Err(Error::Query(format!(
                    "DESCRIBE HISTORY no devolvió las {committed} confirmaciones de la carga completas"
                )));
            }
            let Some(now) = history_version(&history) else {
                return Err(Error::Query("no se pudo leer la versión Delta actual de la tabla en DESCRIBE HISTORY".into()));
            };
            if now <= before {
                return Err(Error::Query(format!(
                    "DESCRIBE HISTORY no muestra las {committed} confirmaciones de la carga (versión {now}, antes {before})"
                )));
            }
            let me = self.run("SELECT current_user()", 1, None).await?;
            let user = me.rows.first().and_then(|r| r.first()).and_then(Json::as_str).unwrap_or_default().to_string();
            if !only_our_writes(&history, before, &user, committed) {
                return Err(Error::Query("la tabla tuvo otros cambios durante la carga y deshacerla los perdería".into()));
            }
            match self.exec(&format!("RESTORE TABLE {table} TO VERSION AS OF {before}")).await {
                Ok(()) => Ok(()),
                Err(Exec::NotApplied(e)) => Err(e),
                Err(Exec::Unknown(e)) => Err(Error::Query(format!("no se pudo confirmar si terminó el RESTORE: {e}"))),
            }
        }
        .await;
        match r {
            Ok(()) => e,
            Err(u) => Error::Query(format!(
                "{e}. Además, no se pudieron deshacer las filas ya confirmadas ({u}): pueden haber quedado filas de la carga; la versión Delta anterior a la carga es la {before} (RESTORE TABLE {table} TO VERSION AS OF {before})."
            )),
        }
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        let names: Vec<String> = if spec.columns.is_empty() {
            columns.iter().map(|c| c.name.clone()).collect()
        } else {
            spec.columns.clone()
        };
        let described = self.columns(&spec.table).await?;
        if described.is_empty() {
            return Err(Error::Query(format!(
                "No se encontró la tabla {}.",
                spec.table.name
            )));
        }
        let mut targets = Vec::with_capacity(names.len());
        for n in &names {
            let c = pick(&described, &spec.table.name, n)?;
            targets.push(target(n, &c.data_type)?);
        }
        let table = self.fq(&spec.table)?;
        let before = self.delta_version(&table, &spec.table.name).await?;
        let mut stmts = Statements::new(insert_head(&table, &names), spec);
        let mut committed = 0usize;
        match self.load_rows(&mut stmts, &targets, source, progress, &mut committed).await {
            Ok(n) => Ok(n),
            Err(Exec::NotApplied(e)) if committed == 0 => Err(e),
            Err(Exec::NotApplied(e)) => Err(self.undo_load(&table, before, committed, e).await),
            // Undoing now could be overtaken by the statement's own commit.
            Err(Exec::Unknown(e)) => Err(Error::Query(format!(
                "{e}. No se pudo confirmar si la última sentencia de la carga terminó: puede confirmarse igual y dejar filas en la tabla. Revisala; la versión Delta anterior a la carga es la {before} (RESTORE TABLE {table} TO VERSION AS OF {before})."
            ))),
        }
    }

    /// The load's rows, one INSERT per window; `committed` counts the
    /// statements that committed.
    async fn load_rows(
        &self,
        stmts: &mut Statements,
        targets: &[Target],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
        committed: &mut usize,
    ) -> std::result::Result<u64, Exec> {
        let mut done = 0u64;
        let mut seen = 0u64;
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                seen += 1;
                if row.len() != targets.len() {
                    return Err(Exec::NotApplied(Error::Query(format!(
                        "la fila tiene {} valores y la carga {} columnas",
                        row.len(),
                        targets.len()
                    ))));
                }
                for (sql, n) in stmts
                    .push(&tuple(&row, targets), seen)
                    .map_err(Exec::NotApplied)?
                {
                    self.exec(&sql).await?;
                    *committed += 1;
                    done += n as u64;
                    progress(done);
                }
            }
        }
        if let Some((sql, n)) = stmts.finish() {
            self.exec(&sql).await?;
            *committed += 1;
            done += n as u64;
            progress(done);
        }
        Ok(done)
    }

    /// Run a statement that writes, and follow it to its end: `Ok` when it
    /// succeeded. A failed status poll is retried; after [`POLL_TRIES`] in a
    /// row the statement is cancelled and followed again, so it can't
    /// commit after this returns an error, unless the error is
    /// [`Exec::Unknown`].
    pub(crate) async fn exec(&self, sql: &str) -> std::result::Result<(), Exec> {
        let mut body = self.request(sql, None);
        // Don't hold the request open: the statement's id comes back at once.
        body["wait_timeout"] = json!("0s");
        if let Some(o) = body.as_object_mut() {
            o.remove("on_wait_timeout");
        }
        let post = self
            .api
            .http
            .post(format!("{}/api/2.0/sql/statements", self.api.base))
            .timeout(Duration::from_secs(600))
            .json(&body);
        let resp = match call(&self.api, post).await {
            Ok(r) => r,
            // Refused before it ran (a 4xx answer, or never sent).
            Err(c) if !c.may_run() => return Err(Exec::NotApplied(c.err)),
            Err(c) => return Err(Exec::Unknown(c.err)),
        };
        let Some(id) = resp
            .get("statement_id")
            .and_then(Json::as_str)
            .map(str::to_string)
        else {
            return match statement_end(&resp) {
                Some(End::Succeeded) => Ok(()),
                Some(End::Failed(e)) => Err(Exec::NotApplied(e)),
                None => Err(Exec::Unknown(Error::Query(
                    "Databricks no devolvió el identificador de la sentencia".into(),
                ))),
            };
        };
        self.set_running(Some(id.clone()));
        let mut guard = CancelOnDrop {
            api: self.api.clone(),
            id: Some(id.clone()),
        };
        let r = follow(&self.api, &id, resp).await;
        if r.is_ok() {
            guard.id = None;
        }
        self.set_running(None);
        match r {
            Ok(End::Succeeded) => Ok(()),
            Ok(End::Failed(e)) => Err(Exec::NotApplied(e)),
            Err(e) => Err(Exec::Unknown(Error::Query(format!("sentencia {id}: {e}")))),
        }
    }
}

/// Why a statement that writes didn't succeed.
#[derive(Debug)]
pub(crate) enum Exec {
    /// It ended without committing, or never ran.
    NotApplied(Error),
    /// Its end couldn't be confirmed: it may still commit.
    Unknown(Error),
}

/// How a statement ended.
#[derive(Debug)]
pub(crate) enum End {
    Succeeded,
    Failed(Error),
}

/// A statement's end from its status, if it ended. `CLOSED` is a success
/// whose result was closed.
pub(crate) fn statement_end(resp: &Json) -> Option<End> {
    match resp.pointer("/status/state").and_then(Json::as_str)? {
        "SUCCEEDED" | "CLOSED" => Some(End::Succeeded),
        "FAILED" => {
            let msg = resp
                .pointer("/status/error/message")
                .and_then(Json::as_str)
                .unwrap_or("la sentencia falló");
            Some(End::Failed(Error::Query(msg.to_string())))
        }
        "CANCELED" => Some(End::Failed(Error::Cancelled)),
        _ => None,
    }
}

/// A failed API call: whether the request may have reached the server,
/// the HTTP status if there was an answer, and the error.
pub(crate) struct CallError {
    sent: bool,
    status: Option<u16>,
    err: Error,
}

impl CallError {
    /// Whether the statement a failed submit carried may run anyway: no
    /// answer after sending it, a timeout or a server error. A 4xx answer
    /// (bad request, rate limit, auth) means it wasn't accepted.
    fn may_run(&self) -> bool {
        self.sent && self.status.is_none_or(|s| s == 408 || s >= 500)
    }
}

/// An API call that keeps what `Api::send` drops: whether the request was
/// sent and the HTTP status.
async fn call(api: &Api, req: reqwest::RequestBuilder) -> std::result::Result<Json, CallError> {
    let auth = api.bearer().await.map_err(|err| CallError {
        sent: false,
        status: None,
        err,
    })?;
    let resp = req
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| CallError {
            sent: !e.is_connect(),
            status: None,
            err: Error::Connect(e.to_string()),
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| CallError {
        sent: true,
        status: None,
        err: Error::Connect(e.to_string()),
    })?;
    let body: Json = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }));
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let msg = crate::api_message(&body, status);
    let err = if matches!(status, 401 | 403) {
        Error::AuthFailed(msg)
    } else {
        Error::Query(msg)
    };
    Err(CallError {
        sent: true,
        status: Some(status),
        err,
    })
}

/// Poll statement `id` (`resp`: its last status) until it ends. After
/// [`POLL_TRIES`] failed polls in a row it's cancelled; after as many more
/// its end is unknown (the error).
async fn follow(api: &Api, id: &str, mut resp: Json) -> Result<End> {
    let (mut delay, max) = POLL_WAIT;
    let mut failed = 0u32;
    let mut cancelled_by_us: Option<Error> = None;
    loop {
        if let Some(end) = statement_end(&resp) {
            return Ok(match (end, cancelled_by_us) {
                // Our cancel, not the user's: the poll's error is the cause.
                (End::Failed(Error::Cancelled), Some(e)) => End::Failed(Error::Query(format!(
                    "no se pudo seguir el estado de la sentencia y se canceló: {e}"
                ))),
                (end, _) => end,
            });
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max);
        match call(
            api,
            api.http
                .get(format!("{}/api/2.0/sql/statements/{id}", api.base)),
        )
        .await
        {
            Ok(r) => {
                resp = r;
                failed = 0;
            }
            Err(c) => {
                failed += 1;
                tracing::debug!(
                    "databricks status poll of {id} failed ({failed}): {}",
                    c.err
                );
                if failed < POLL_TRIES {
                    continue;
                }
                if cancelled_by_us.is_some() {
                    return Err(c.err);
                }
                cancel(api, id).await;
                cancelled_by_us = Some(c.err);
                failed = 0;
            }
        }
    }
}

/// Ask for statement `id` to be cancelled (a few tries; its end is
/// checked by polling, not by this answer).
async fn cancel(api: &Api, id: &str) {
    for _ in 0..3 {
        let req = api
            .http
            .post(format!("{}/api/2.0/sql/statements/{id}/cancel", api.base))
            .json(&json!({}));
        match call(api, req).await {
            Ok(_) => return,
            Err(c) => tracing::debug!("databricks cancel of {id} failed: {}", c.err),
        }
        tokio::time::sleep(POLL_WAIT.0).await;
    }
}

/// Cancels a statement whose load was dropped mid-way (the run was
/// cancelled, or the read failed): it mustn't commit on its own later.
struct CancelOnDrop {
    api: Api,
    id: Option<String>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (Some(id), Ok(rt)) = (self.id.take(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let api = self.api.clone();
        rt.spawn(async move { cancel(&api, &id).await });
    }
}

/// `INSERT INTO t (cols) VALUES ` (the rows follow).
pub(crate) fn insert_head(table: &str, names: &[String]) -> String {
    format!(
        "INSERT INTO {table} ({}) VALUES ",
        names.iter().map(|n| q(n)).collect::<Vec<_>>().join(", ")
    )
}

/// A row's tuple: `(v1, v2, …)`.
pub(crate) fn tuple(row: &[Cell], targets: &[Target]) -> String {
    let mut s = String::from("(");
    for (i, (c, t)) in row.iter().zip(targets).enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&value_sql(c, t));
    }
    s.push(')');
    s
}

/// Bytes `s` takes inside a JSON string, escaped as serde_json does.
pub(crate) fn json_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

/// The load's INSERT statements, one per commit window: `commit_rows`
/// rows or `commit_bytes` of SQL, whichever comes first, never over
/// [`STMT_BYTES`] once escaped in the request (the API's limit).
pub(crate) struct Statements {
    head: String,
    head_cost: usize,
    stmt: String,
    /// `stmt`'s escaped size.
    cost: usize,
    rows: usize,
    max_rows: usize,
    max_bytes: usize,
}

impl Statements {
    pub(crate) fn new(head: String, spec: &LoadSpec) -> Self {
        let max_rows = usize::try_from(spec.commit_rows)
            .unwrap_or(usize::MAX)
            .max(1);
        let max_bytes = usize::try_from(spec.commit_bytes)
            .unwrap_or(usize::MAX)
            .clamp(1, STMT_BYTES);
        Statements {
            head_cost: json_len(&head),
            head,
            stmt: String::new(),
            cost: 0,
            rows: 0,
            max_rows,
            max_bytes,
        }
    }

    /// Add row number `n`'s tuple; gives the statements ready to run (with
    /// their rows), at most two: the one the row didn't fit in, and the
    /// one it closed.
    pub(crate) fn push(&mut self, tuple: &str, n: u64) -> Result<Vec<(String, usize)>> {
        let cost = json_len(tuple);
        if self.head_cost + cost > STMT_BYTES {
            return Err(Error::Unsupported(format!(
                "La fila {n} ocupa {:.1} MiB como SQL dentro del pedido a la API y Databricks acepta hasta 16 MiB por sentencia (DBine usa hasta {} MiB); la carga masiva de Databricks no puede enviarla.",
                cost as f64 / (1024.0 * 1024.0),
                STMT_BYTES / (1024 * 1024)
            )));
        }
        let mut ready = Vec::new();
        if self.rows > 0 && self.cost + 2 + cost > STMT_BYTES {
            ready.extend(self.finish());
        }
        self.cost += if self.rows == 0 { self.head_cost } else { 2 } + cost;
        self.stmt
            .push_str(if self.rows == 0 { &self.head } else { ", " });
        self.stmt.push_str(tuple);
        self.rows += 1;
        if self.rows >= self.max_rows || self.stmt.len() >= self.max_bytes {
            ready.extend(self.finish());
        }
        Ok(ready)
    }

    /// The statement being built, if it has rows.
    pub(crate) fn finish(&mut self) -> Option<(String, usize)> {
        self.cost = 0;
        (self.rows > 0).then(|| {
            (
                std::mem::take(&mut self.stmt),
                std::mem::replace(&mut self.rows, 0),
            )
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn kinds_by_catalog_and_answer_types() {
        assert_eq!(kind_of("bigint"), Kind::Int);
        assert_eq!(kind_of("LONG"), Kind::Int);
        assert_eq!(kind_of("SHORT"), Kind::Int);
        assert_eq!(kind_of("float"), Kind::Real);
        assert_eq!(kind_of("double"), Kind::Double);
        assert_eq!(kind_of("decimal(38,10)"), Kind::Decimal);
        assert_eq!(kind_of("DECIMAL"), Kind::Decimal);
        assert_eq!(kind_of("boolean"), Kind::Bool);
        assert_eq!(kind_of("binary"), Kind::Binary);
        assert_eq!(kind_of("date"), Kind::Date);
        assert_eq!(kind_of("timestamp"), Kind::Timestamp);
        assert_eq!(kind_of("timestamp_ntz"), Kind::TimestampNtz);
        assert_eq!(kind_of("array<struct<a:int>>"), Kind::Nested);
        assert_eq!(kind_of("map<string,int>"), Kind::Nested);
        assert_eq!(kind_of("variant"), Kind::Variant);
        assert_eq!(kind_of("varchar(10)"), Kind::Text);
        assert_eq!(kind_of("string"), Kind::Text);
        assert_eq!(kind_of("string collate utf8_lcase"), Kind::Text);
        assert_eq!(kind_of("char(3)"), Kind::Text);
        assert_eq!(kind_of("interval day to second"), Kind::Other);
        assert_eq!(kind_of("geometry(4326)"), Kind::Other);
    }

    #[test]
    fn read_statement() {
        let names = vec![
            "id".to_string(),
            "at".into(),
            "local".into(),
            "odd`name".into(),
        ];
        let kinds = [Kind::Int, Kind::Timestamp, Kind::TimestampNtz, Kind::Text];
        assert_eq!(
            read_sql("`c`.`s`.`t`", &names, &kinds, Some(" id > 5 ")),
            "SELECT `id`, date_format(`at`, 'yyyy-MM-dd HH:mm:ss.SSSSSSxxx') AS `at`, CAST(`local` AS STRING) AS `local`, `odd``name` FROM `c`.`s`.`t` WHERE id > 5"
        );
        assert_eq!(
            select_expr("iv", Kind::Other),
            "CAST(`iv` AS STRING) AS `iv`"
        );
        assert_eq!(
            select_expr("n", Kind::Nested),
            "to_json(`n`, map('timestampFormat', 'yyyy-MM-dd\\'T\\'HH:mm:ss.SSSSSSXXX', 'timestampNTZFormat', 'yyyy-MM-dd\\'T\\'HH:mm:ss.SSSSSS')) AS `n`"
        );
        assert_eq!(
            read_sql("t", &names[..1], &kinds[..1], Some("  ")),
            "SELECT `id` FROM t"
        );
    }

    #[test]
    fn answers_become_typed_cells() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(to_cell(None, Kind::Int).unwrap(), Cell::Null);
        assert_eq!(
            to_cell(s("9007199254740993"), Kind::Int).unwrap(),
            Cell::Int(9_007_199_254_740_993)
        );
        assert_eq!(
            to_cell(s("-9223372036854775808"), Kind::Int).unwrap(),
            Cell::Int(i64::MIN)
        );
        assert_eq!(
            to_cell(s("0.1"), Kind::Real).unwrap(),
            Cell::Float(f64::from(0.1f32))
        );
        assert_eq!(to_cell(s("0.1"), Kind::Double).unwrap(), Cell::Float(0.1));
        assert_eq!(
            to_cell(s("1.0E300"), Kind::Double).unwrap(),
            Cell::Float(1e300)
        );
        assert_eq!(
            to_cell(s("-Infinity"), Kind::Real).unwrap(),
            Cell::Float(f64::NEG_INFINITY)
        );
        assert!(matches!(to_cell(s("NaN"), Kind::Double).unwrap(), Cell::Float(f) if f.is_nan()));
        assert_eq!(
            to_cell(s("12345678901234567890.1234567890"), Kind::Decimal).unwrap(),
            Cell::Decimal("12345678901234567890.1234567890".into())
        );
        assert_eq!(to_cell(s("true"), Kind::Bool).unwrap(), Cell::Bool(true));
        assert_eq!(to_cell(s("false"), Kind::Bool).unwrap(), Cell::Bool(false));
        assert_eq!(
            to_cell(s("yv4A"), Kind::Binary).unwrap(),
            Cell::Bytes(vec![0xCA, 0xFE, 0])
        );
        assert_eq!(
            to_cell(s("2024-01-31"), Kind::Date).unwrap(),
            Cell::Date("2024-01-31".into())
        );
        assert_eq!(
            to_cell(s("2024-01-31 13:45:00.123456-03:00"), Kind::Timestamp).unwrap(),
            Cell::DateTimeTz("2024-01-31 13:45:00.123456-03:00".into())
        );
        assert_eq!(
            to_cell(s("2024-01-31T13:45:00.000Z"), Kind::Timestamp).unwrap(),
            Cell::DateTimeTz("2024-01-31 13:45:00.000+00:00".into())
        );
        assert_eq!(
            to_cell(s("2024-01-31 13:45:00.5"), Kind::TimestampNtz).unwrap(),
            Cell::DateTime("2024-01-31 13:45:00.5".into())
        );
        assert_eq!(
            to_cell(s("{\"a\":[1,2]}"), Kind::Nested).unwrap(),
            Cell::Json("{\"a\":[1,2]}".into())
        );
        assert_eq!(
            to_cell(s("1"), Kind::Variant).unwrap(),
            Cell::Json("1".into())
        );
        assert_eq!(
            to_cell(s("hola"), Kind::Text).unwrap(),
            Cell::Text("hola".into())
        );
    }

    #[test]
    fn literals_for_each_target() {
        assert_eq!(v(&Cell::Null, "int"), "NULL");
        assert_eq!(v(&Cell::Int(-7), "bigint"), "-7");
        assert_eq!(
            v(&Cell::UInt(u64::MAX), "decimal(20,0)"),
            "18446744073709551615"
        );
        assert_eq!(v(&Cell::Bool(true), "int"), "1");
        assert_eq!(v(&Cell::Int(0), "boolean"), "FALSE");
        assert_eq!(v(&Cell::Bool(true), "boolean"), "TRUE");
        assert_eq!(v(&Cell::Float(0.1), "double"), "1e-1");
        assert_eq!(
            v(&Cell::Float(90.33333333333333), "double"),
            "9.033333333333333e1"
        );
        assert_eq!(v(&Cell::Float(f64::NAN), "float"), "CAST('NaN' AS DOUBLE)");
        assert_eq!(
            v(&Cell::Float(f64::NEG_INFINITY), "double"),
            "CAST('-Infinity' AS DOUBLE)"
        );
        assert_eq!(
            v(&Cell::Decimal("-123.4500".into()), "decimal(10,4)"),
            "-123.4500"
        );
        assert_eq!(v(&Cell::Decimal("+.5".into()), "decimal(3,2)"), ".5");
        assert_eq!(
            v(&Cell::Text("1e5".into()), "double"),
            "CAST('1e5' AS double)"
        );
        assert_eq!(
            v(&Cell::Text("x'); DROP".into()), "int"),
            "CAST('x\\'); DROP' AS int)"
        );
        assert_eq!(v(&Cell::Bytes(vec![0xCA, 0xFE, 0]), "binary"), "X'CAFE00'");
        assert_eq!(v(&Cell::Text("ab".into()), "binary"), "X'6162'");
        assert_eq!(v(&Cell::Text("O'B\\n".into()), "string"), "'O\\'B\\\\n'");
        assert_eq!(v(&Cell::Int(5), "string"), "'5'");
        assert_eq!(v(&Cell::Bytes(b"hi".to_vec()), "string"), "'hi'");
        assert_eq!(v(&Cell::Bytes(vec![0xFF]), "string"), "'0xFF'");
        assert_eq!(v(&Cell::Uuid("7f1c…".into()), "string"), "'7f1c…'");
        assert_eq!(
            v(&Cell::Date("2024-01-31".into()), "date"),
            "CAST('2024-01-31' AS date)"
        );
        assert_eq!(
            v(
                &Cell::DateTimeTz("2024-01-31 13:45:00.123456-03:00".into()),
                "timestamp"
            ),
            "CAST('2024-01-31 13:45:00.123456-03:00' AS timestamp)"
        );
        assert_eq!(
            v(
                &Cell::DateTime("2024-01-31 13:45:00".into()),
                "timestamp_ntz"
            ),
            "CAST('2024-01-31 13:45:00' AS timestamp_ntz)"
        );
        assert_eq!(
            v(&Cell::Json("[1,2]".into()), "array<int>"),
            "from_json('[1,2]', 'array<int>', map('mode', 'FAILFAST'))"
        );
        assert_eq!(
            v(&Cell::Json("{\"a\":1}".into()), "variant"),
            "parse_json('{\"a\":1}')"
        );
        assert_eq!(v(&Cell::Int(3), "variant"), "parse_json('3')");
    }

    fn v(c: &Cell, ty: &str) -> String {
        value_sql(c, &target("c", ty).unwrap())
    }

    #[test]
    fn insert_statement() {
        let targets = vec![target("id", "int").unwrap(), target("b", "binary").unwrap()];
        let spec = LoadSpec {
            table: dbine_driver::ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "t".into(),
            },
            columns: vec![],
            table_lock: false,
            keep_identity: false,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let mut st = Statements::new(
            insert_head("`c`.`s`.`t`", &["id".into(), "b".into()]),
            &spec,
        );
        assert!(st
            .push(&tuple(&[Cell::Int(1), Cell::Bytes(vec![1])], &targets), 1)
            .unwrap()
            .is_empty());
        assert!(st
            .push(&tuple(&[Cell::Null, Cell::Null], &targets), 2)
            .unwrap()
            .is_empty());
        assert_eq!(
            st.finish(),
            Some((
                "INSERT INTO `c`.`s`.`t` (`id`, `b`) VALUES (1, X'01'), (NULL, NULL)".to_string(),
                2
            ))
        );
        assert_eq!(st.finish(), None);
    }

    #[test]
    fn numbers_and_zones() {
        assert_eq!(plain_number(" 12 "), Some("12"));
        assert_eq!(plain_number("-1.5"), Some("-1.5"));
        assert_eq!(plain_number("1.2.3"), None);
        assert_eq!(plain_number("."), None);
        assert_eq!(plain_number("1e5"), None);
        assert_eq!(plain_number(""), None);
        assert_eq!(
            zoned("2024-01-31 13:45:00+05:30"),
            "2024-01-31 13:45:00+05:30"
        );
    }

    #[tokio::test]
    async fn inline_chunks_become_rows() {
        let (tx, mut rx) = mpsc::channel(4);
        let chunk =
            json!({ "chunk_index": 0, "data_array": [["1", null, "yv4="], ["2", "x", null]] });
        send_chunk(&reqwest::Client::new(), chunk, &tx)
            .await
            .unwrap();
        match rx.recv().await {
            Some(Ok(Block::Rows(rows))) => {
                assert_eq!(
                    rows,
                    vec![
                        vec![Some("1".into()), None, Some("yv4=".into())],
                        vec![Some("2".into()), Some("x".into()), None]
                    ]
                )
            }
            other => panic!("{other:?}"),
        }
        send_chunk(&reqwest::Client::new(), json!({ "chunk_index": 3 }), &tx)
            .await
            .unwrap();
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    fn spec(rows: u64, bytes: u64) -> LoadSpec {
        LoadSpec {
            table: dbine_driver::ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "t".into(),
            },
            columns: vec![],
            table_lock: false,
            keep_identity: false,
            commit_rows: rows,
            commit_bytes: bytes,
        }
    }

    #[test]
    fn binary_grid_text_is_decoded() {
        // `read_via_execute` hands binaries over as `0x…` text.
        assert_eq!(v(&Cell::Text("0xCAFE".into()), "binary"), "X'CAFE'");
        assert_eq!(v(&Cell::Text("0XcafE00".into()), "binary"), "X'CAFE00'");
        assert_eq!(v(&Cell::Text("0x".into()), "binary"), "X''");
        // Not hex: the text's own bytes.
        assert_eq!(v(&Cell::Text("0xZZ".into()), "binary"), "X'30785A5A'");
        assert_eq!(v(&Cell::Text("0xABC".into()), "binary"), "X'3078414243'");
    }

    #[test]
    fn nested_json_fails_fast_and_maps_keep_their_keys() {
        assert_eq!(
            v(&Cell::Json("{\"a\":1}".into()), "struct<a:int>"),
            "from_json('{\"a\":1}', 'struct<a:int>', map('mode', 'FAILFAST'))"
        );
        assert_eq!(
            v(&Cell::Json("{\"k\":1}".into()), "map<string,int>"),
            "from_json('{\"k\":1}', 'map<string,int>', map('mode', 'FAILFAST'))"
        );
        assert_eq!(
            v(&Cell::Json("{\"1\":\"a\"}".into()), "map<int,string>"),
            "transform_keys(from_json('{\"1\":\"a\"}', 'map<string,string>', map('mode', 'FAILFAST')), (k, v) -> CAST(k AS int))"
        );
        assert_eq!(
            target("m", "MAP<DECIMAL(10,2), ARRAY<MAP<STRING,INT>>>")
                .unwrap()
                .json,
            JsonLoad::Keys {
                key: "DECIMAL(10,2)".into(),
                value: "ARRAY<MAP<STRING,INT>>".into()
            }
        );
        // A struct field named `map` isn't a map.
        assert_eq!(
            target("s", "struct<map:int,b:map<string,bigint>>")
                .unwrap()
                .json,
            JsonLoad::Plain
        );
        for ty in [
            "array<map<int,string>>",
            "map<int,map<bigint,string>>",
            "struct<a:map<date,int>>",
        ] {
            assert!(
                matches!(target("x", ty), Err(Error::Unsupported(_))),
                "{ty}"
            );
        }
        assert_eq!(
            map_keys("map<int,struct<a:map<string,int>>>"),
            vec!["int".to_string(), "string".into()]
        );
    }

    #[test]
    fn non_text_types_are_cast() {
        assert_eq!(
            v(
                &Cell::Text("INTERVAL '1 02:03:04' DAY TO SECOND".into()),
                "interval day to second"
            ),
            "CAST('INTERVAL \\'1 02:03:04\\' DAY TO SECOND' AS interval day to second)"
        );
        assert_eq!(v(&Cell::Text("abc".into()), "varchar(10)"), "'abc'");
        assert_eq!(v(&Cell::Text("abc".into()), "char(3)"), "'abc'");
    }

    #[test]
    fn literals_never_hold_a_substitution() {
        assert_eq!(slit("a${x}b"), "'a$' '{x}b'");
        assert_eq!(slit("${a}${b}"), "'$' '{a}$' '{b}'");
        assert_eq!(slit("$ { } $x"), "'$ { } $x'");
        assert_eq!(
            v(&Cell::Text("${spark.app.name}".into()), "string"),
            "'$' '{spark.app.name}'"
        );
        assert_eq!(
            v(&Cell::Json("{\"k\":\"${x}\"}".into()), "variant"),
            "parse_json('{\"k\":\"$' '{x}\"}')"
        );
    }

    #[test]
    fn statements_follow_the_commit_window() {
        let t = |i: i64| tuple(&[Cell::Int(i)], &[target("id", "bigint").unwrap()]);
        // By rows.
        let mut st = Statements::new(
            "INSERT INTO t (`id`) VALUES ".into(),
            &spec(3, LoadSpec::DEFAULT_COMMIT_BYTES),
        );
        assert!(st.push(&t(1), 1).unwrap().is_empty());
        assert!(st.push(&t(2), 2).unwrap().is_empty());
        assert_eq!(
            st.push(&t(3), 3).unwrap(),
            vec![("INSERT INTO t (`id`) VALUES (1), (2), (3)".to_string(), 3)]
        );
        assert!(st.push(&t(4), 4).unwrap().is_empty());
        assert_eq!(
            st.finish(),
            Some(("INSERT INTO t (`id`) VALUES (4)".to_string(), 1))
        );
        // The default window of 100k narrow rows is a single statement.
        let mut st = Statements::new(
            "INSERT INTO t (`id`) VALUES ".into(),
            &spec(
                LoadSpec::DEFAULT_COMMIT_ROWS,
                LoadSpec::DEFAULT_COMMIT_BYTES,
            ),
        );
        for i in 1..LoadSpec::DEFAULT_COMMIT_ROWS as i64 {
            assert!(st.push(&t(i), i as u64).unwrap().is_empty());
        }
        let ready = st.push(&t(0), LoadSpec::DEFAULT_COMMIT_ROWS).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1, LoadSpec::DEFAULT_COMMIT_ROWS as usize);
        // By bytes.
        let mut st = Statements::new("INSERT INTO t (`id`) VALUES ".into(), &spec(1_000, 34));
        assert!(st.push(&t(1), 1).unwrap().is_empty());
        assert_eq!(st.push(&t(2), 2).unwrap().len(), 1);
        // Never over the API's limit, whatever the window.
        let big = format!("('{}')", "x".repeat(7 * 1024 * 1024));
        let mut st = Statements::new("INSERT INTO t (`s`) VALUES ".into(), &spec(1_000, u64::MAX));
        assert!(st.push(&big, 1).unwrap().is_empty());
        let ready = st.push(&big, 2).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1, 1);
        assert!(ready[0].0.len() <= STMT_BYTES);
        assert_eq!(st.finish().map(|s| s.1), Some(1));
        // A row that can't fit in any statement: a clear refusal.
        let huge = format!("('{}')", "x".repeat(STMT_BYTES));
        match st.push(&huge, 7) {
            Err(Error::Unsupported(m)) => {
                assert!(m.contains("La fila 7") && m.contains("16 MiB"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn truncated_or_unsized_answers_are_errors() {
        let plan = |m: Json| chunk_plan(&json!({ "manifest": m }), None);
        assert_eq!(
            plan(json!({ "total_chunk_count": 3, "total_row_count": 10, "truncated": false }))
                .unwrap(),
            (3, Some(10))
        );
        assert_eq!(plan(json!({ "total_row_count": 0 })).unwrap(), (0, Some(0)));
        assert!(matches!(
            plan(json!({ "total_chunk_count": 1, "total_row_count": 10, "truncated": true })),
            Err(Error::Query(_))
        ));
        assert!(matches!(
            plan(json!({ "total_row_count": 10 })),
            Err(Error::Query(_))
        ));
        assert!(matches!(chunk_plan(&json!({}), None), Err(Error::Query(_))));
    }

    struct Discard;
    impl dbine_driver::transfer::BatchSink for Discard {
        fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
            Ok(())
        }
        fn batch(&mut self, _: dbine_driver::transfer::RowBatch) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A table read (Compare data, a copy) on a warehouse that refuses
    /// external links: as reported on 1.6.3, the error said only that the
    /// inline answer was truncated, that "no se copia una tabla incompleta"
    /// (in a comparison), and threw away what Databricks answered to the
    /// links, the one thing to fix. The refusal comes on submitting or once
    /// submitted, and the inline read is truncated or fails over its limit.
    #[tokio::test]
    async fn a_read_without_links_says_why_they_were_refused() {
        const REFUSED: &str = "EXTERNAL_LINKS is disabled for this workspace";
        let inline_truncated = json!({
            "statement_id": "s2",
            "status": { "state": "SUCCEEDED" },
            "manifest": { "total_chunk_count": 1, "total_row_count": 9, "truncated": true },
            "result": { "chunk_index": 0, "data_array": [["1"]] },
        })
        .to_string();
        let inline_over = json!({
            "statement_id": "s2",
            "status": { "state": "FAILED", "error": { "message": "Inline byte limit exceeded. Statements executed with disposition=INLINE can have a result size of at most 26214400 bytes." } },
        })
        .to_string();
        let links_failed = json!({
            "statement_id": "s1",
            "status": { "state": "FAILED", "error": { "message": REFUSED } },
        })
        .to_string();
        for (on_submit, inline) in [
            (true, inline_truncated.clone()),
            (false, inline_truncated),
            (true, inline_over.clone()),
            (false, inline_over),
        ] {
            let links_failed = links_failed.clone();
            let base = serve(Arc::new(move |m, p, body| {
                if (m, p) != ("POST", "/api/2.0/sql/statements") {
                    return (404, "{}".into());
                }
                let b: Json = serde_json::from_str(body).unwrap();
                if b["disposition"] == "EXTERNAL_LINKS" {
                    if on_submit {
                        (400, json!({ "message": REFUSED }).to_string())
                    } else {
                        (200, links_failed.clone())
                    }
                } else {
                    (200, inline.clone())
                }
            }))
            .await;
            let col = TransferColumn {
                name: "id".into(),
                type_name: "int".into(),
                nullable: true,
            };
            let sink: BatchSinkRef = Arc::new(std::sync::Mutex::new(Discard));
            let names = ["id".to_string()];
            let read = TableRead {
                table: "t",
                names: &names,
                kinds: &[kind_of("int")],
                filter: None,
                parts: 1,
                version: None,
                delta: false,
            };
            match session(base).read_table(&read, &[col], sink).await {
                Err(Error::Query(m)) => {
                    assert!(m.contains(REFUSED) && m.contains("25 MiB"), "{m}");
                    assert!(!m.contains("copia"), "{m}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    // --- Reads past Databricks' 100 GiB per statement, in parts.

    #[test]
    fn parts_are_planned_from_the_files_size() {
        const GIB: u64 = 1 << 30;
        assert_eq!(planned_parts(None), 1);
        assert_eq!(planned_parts(Some(0)), 1);
        // Up to 5 GiB of files: estimated within one part's target.
        assert_eq!(planned_parts(Some(5 * GIB)), 1);
        assert_eq!(planned_parts(Some(5 * GIB + 1)), 2);
        assert_eq!(planned_parts(Some(12 * GIB)), 3);
        // Far past anything: as many as leaves one split.
        assert_eq!(planned_parts(Some(u64::MAX)), MAX_MODULUS / SPLIT);
    }

    #[test]
    fn split_parts_hold_each_row_once() {
        // The residues of 3 parts, each split, cover those of 12 once.
        let mut residues: Vec<u64> = Part::all(3)
            .flat_map(Part::split)
            .inspect(|p| assert_eq!(p.modulus, 12))
            .map(|p| p.residue)
            .collect();
        residues.sort_unstable();
        assert_eq!(residues, (0..12).collect::<Vec<_>>());
        // A part's split keeps its rows: same residue modulo the part's.
        let p = Part {
            modulus: 3,
            residue: 1,
        };
        assert!(p.split().all(|q| q.residue % 3 == 1));
        assert_eq!(Part::all(1).collect::<Vec<_>>(), vec![Part::WHOLE]);
    }

    #[test]
    fn part_statements() {
        let names = vec!["id".to_string(), "m".into(), "v".into(), "at".into()];
        let kinds = [Kind::Int, Kind::Nested, Kind::Variant, Kind::Timestamp];
        let whole = part_sql(
            "`c`.`s`.`t`",
            None,
            &names,
            &kinds,
            Some("id > 5"),
            Part::WHOLE,
        );
        assert_eq!(
            whole,
            read_sql("`c`.`s`.`t`", &names, &kinds, Some("id > 5"))
        );
        let sql = part_sql(
            "`c`.`s`.`t`",
            Some(7),
            &names,
            &kinds,
            Some("id > 5 -- recent"),
            Part {
                modulus: 12,
                residue: 4,
            },
        );
        assert!(
            sql.contains(" FROM `c`.`s`.`t` VERSION AS OF 7 WHERE "),
            "{sql}"
        );
        // The filter on lines of its own: its comment ends at the newline.
        assert!(
            sql.ends_with("WHERE (\nid > 5 -- recent\n) AND pmod(xxhash64(`id`, to_json(`m`), CAST(`v` AS STRING), `at`), 12) = 4"),
            "{sql}"
        );
        let live = part_sql(
            "t",
            None,
            &names[..1],
            &kinds[..1],
            None,
            Part {
                modulus: 4,
                residue: 0,
            },
        );
        assert_eq!(live, "SELECT `id` FROM t WHERE pmod(xxhash64(`id`), 4) = 0");
    }

    /// Collects every row a read hands over.
    #[derive(Default)]
    struct Collect(Vec<Vec<Cell>>);
    impl dbine_driver::transfer::BatchSink for Collect {
        fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
            Ok(())
        }
        fn batch(&mut self, b: dbine_driver::transfer::RowBatch) -> std::io::Result<()> {
            self.0.extend(b.rows);
            Ok(())
        }
    }

    /// A finished statement's status: one column per name, these rows,
    /// cut or not.
    fn answer(names: &[&str], rows: &[Vec<String>], cut: bool) -> String {
        let columns: Vec<Json> = names
            .iter()
            .map(|n| json!({ "name": n, "type_name": "STRING" }))
            .collect();
        json!({
            "statement_id": "s",
            "status": { "state": "SUCCEEDED" },
            "manifest": {
                "schema": { "columns": columns },
                "total_chunk_count": 1,
                "total_row_count": rows.len(),
                "truncated": cut,
            },
            "result": { "chunk_index": 0, "data_array": rows },
        })
        .to_string()
    }

    /// What a test warehouse answers each SELECT: (rows, cut).
    type Selects = Arc<dyn Fn(&str) -> (u64, bool) + Send + Sync>;

    /// A warehouse for table reads: `DESCRIBE DETAIL` answers `detail`
    /// (format, sizeInBytes; `None`: fails, as for a view), `DESCRIBE
    /// HISTORY` version 7, and each SELECT `select(statement)`. Gives the
    /// base URL and the statements received.
    async fn warehouse(
        detail: Option<(&'static str, u64)>,
        select: Selects,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let base = serve(Arc::new(move |m, p, body| {
            if (m, p) != ("POST", "/api/2.0/sql/statements") {
                return (404, "{}".into());
            }
            let b: Json = serde_json::from_str(body).unwrap();
            let sql = b["statement"].as_str().unwrap_or("").to_string();
            log.lock().unwrap().push(sql.clone());
            if sql.starts_with("DESCRIBE DETAIL") {
                return match detail {
                    Some((format, size)) => (
                        200,
                        answer(
                            &["format", "sizeInBytes"],
                            &[vec![format.to_string(), size.to_string()]],
                            false,
                        ),
                    ),
                    None => (
                        400,
                        json!({ "message": "DESCRIBE DETAIL is only supported for tables" })
                            .to_string(),
                    ),
                };
            }
            if sql.starts_with("DESCRIBE HISTORY") {
                return (
                    200,
                    answer(
                        &["version", "operation"],
                        &[vec!["7".into(), "WRITE".into()]],
                        false,
                    ),
                );
            }
            assert_eq!(b["disposition"], "EXTERNAL_LINKS", "{sql}");
            let (n, cut) = select(&sql);
            let rows: Vec<Vec<String>> = (0..n).map(|i| vec![i.to_string()]).collect();
            (200, answer(&["id"], &rows, cut))
        }))
        .await;
        (base, seen)
    }

    /// Read table `t` (of kind `kind`) with one INT column `id`: the result
    /// and the rows that reached the sink.
    async fn read_t(base: String, kind: &str) -> (Result<u64>, usize) {
        let names = ["id".to_string()];
        let cols = [TransferColumn {
            name: "id".into(),
            type_name: "int".into(),
            nullable: false,
        }];
        let rows = Arc::new(std::sync::Mutex::new(Collect::default()));
        let sink: BatchSinkRef = rows.clone();
        let obj = ObjectRef {
            kind: kind.into(),
            schema: Some("s".into()),
            name: "t".into(),
        };
        let r = session(base)
            .read_rows(&obj, "`c`.`s`.`t`", &names, &[Kind::Int], None, &cols, sink)
            .await;
        let n = rows.lock().unwrap().0.len();
        (r, n)
    }

    fn selects(seen: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("SELECT"))
            .cloned()
            .collect()
    }

    /// The DBA's Compare data on 1.6.4-test.1: a Databricks table whose
    /// read passed the 100 GiB of one statement came back cut, and the
    /// compare stopped. A table sized past one part's target is read in
    /// parts from the start, all at one Delta version, each row once.
    #[tokio::test]
    async fn a_big_table_is_read_in_parts_of_one_snapshot() {
        // 12 GiB of files → 3 parts; part i answers 10 + i rows.
        let (base, seen) = warehouse(
            Some(("delta", 12 << 30)),
            Arc::new(|sql| {
                let r: u64 = sql.rsplit("= ").next().unwrap().trim().parse().unwrap();
                (10 + r, false)
            }),
        )
        .await;
        let (r, n) = read_t(base, "table").await;
        assert_eq!((r.unwrap(), n), (33, 33));
        let parts = selects(&seen);
        assert_eq!(parts.len(), 3, "{parts:?}");
        for (i, sql) in parts.iter().enumerate() {
            assert!(
                sql.contains("`c`.`s`.`t` VERSION AS OF 7 WHERE pmod(xxhash64(`id`), 3) = "),
                "{sql}"
            );
            assert!(sql.ends_with(&format!("= {i}")), "{sql}");
        }
    }

    #[tokio::test]
    async fn a_small_table_stays_one_statement() {
        let (base, seen) = warehouse(Some(("delta", 1 << 30)), Arc::new(|_| (5, false))).await;
        let (r, n) = read_t(base, "table").await;
        assert_eq!((r.unwrap(), n), (5, 5));
        let all = seen.lock().unwrap().clone();
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all[0].starts_with("DESCRIBE DETAIL `c`.`s`.`t`"));
        assert_eq!(all[1], "SELECT `id` FROM `c`.`s`.`t`");
    }

    /// A part Databricks still cuts is never used: its rows (999 here)
    /// don't reach the sink, and it's read again in four, at one version.
    #[tokio::test]
    async fn a_cut_part_is_read_again_in_smaller_ones() {
        // Sized as one statement, but the whole read and the part 2 of 4
        // come back cut.
        let (base, seen) = warehouse(
            Some(("delta", 1 << 30)),
            Arc::new(|sql| {
                if !sql.contains("pmod") || sql.ends_with("4) = 2") {
                    (999, true)
                } else {
                    (1, false)
                }
            }),
        )
        .await;
        let (r, n) = read_t(base, "table").await;
        // 3 of the 4 parts, plus the 4 parts of the cut one.
        assert_eq!((r.unwrap(), n), (7, 7));
        let all = seen.lock().unwrap().clone();
        assert!(
            all.iter().any(|s| s.starts_with("DESCRIBE HISTORY")),
            "{all:?}"
        );
        let parts = selects(&seen);
        let tails: Vec<&str> = parts
            .iter()
            .map(|s| s.rsplit("pmod").next().unwrap())
            .collect();
        assert_eq!(
            tails,
            [
                "SELECT `id` FROM `c`.`s`.`t`",
                "(xxhash64(`id`), 4) = 0",
                "(xxhash64(`id`), 4) = 1",
                "(xxhash64(`id`), 4) = 2",
                "(xxhash64(`id`), 16) = 2",
                "(xxhash64(`id`), 16) = 6",
                "(xxhash64(`id`), 16) = 10",
                "(xxhash64(`id`), 16) = 14",
                "(xxhash64(`id`), 4) = 3",
            ]
        );
        assert!(
            parts[1..].iter().all(|s| s.contains("VERSION AS OF 7")),
            "{parts:?}"
        );
    }

    /// A view has no Delta log: no size, read live, split the same.
    #[tokio::test]
    async fn a_view_is_read_live_in_parts_when_cut() {
        let (base, seen) = warehouse(
            None,
            Arc::new(|sql| {
                if sql.contains("pmod") {
                    (2, false)
                } else {
                    (999, true)
                }
            }),
        )
        .await;
        let (r, n) = read_t(base, "view").await;
        assert_eq!((r.unwrap(), n), (8, 8));
        let all = seen.lock().unwrap().clone();
        assert!(!all.iter().any(|s| s.starts_with("DESCRIBE")), "{all:?}");
        assert!(!all.iter().any(|s| s.contains("VERSION AS OF")), "{all:?}");
        assert_eq!(selects(&seen).len(), 5);
    }

    /// Rows that can't be spread (every part cut, down to the finest)
    /// end in an error that says it's Databricks' limit, not the table's
    /// size on disk, and nothing cut is used.
    #[tokio::test]
    async fn a_read_that_never_fits_says_so() {
        let (base, seen) = warehouse(None, Arc::new(|_| (999, true))).await;
        let (r, n) = read_t(base, "view").await;
        match r {
            Err(Error::Query(m)) => {
                assert!(
                    m.contains("100 GiB") && m.contains("formato interno"),
                    "{m}"
                );
                assert!(m.contains(&format!("{MAX_MODULUS} partes")), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(n, 0);
        // Depth first: 1, 4, 16, 64, 256, 1024.
        assert_eq!(selects(&seen).len(), 6);
    }

    #[test]
    fn unreadable_numbers_are_errors() {
        let s = |v: &str| Some(v.to_string());
        assert!(to_cell(s("1,5"), Kind::Double).is_err());
        assert!(to_cell(s("abc"), Kind::Real).is_err());
        assert!(to_cell(s("1.5"), Kind::Int).is_err());
        assert!(to_cell(s("yes"), Kind::Bool).is_err());
        assert!(to_cell(s("!!"), Kind::Binary).is_err());
        assert_eq!(to_cell(s("TRUE"), Kind::Bool).unwrap(), Cell::Bool(true));
    }

    #[tokio::test]
    async fn pending_downloads_are_aborted_on_drop() {
        let alive = std::sync::Arc::new(());
        let held = alive.clone();
        let h: JoinHandle<()> = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(std::sync::Arc::strong_count(&alive), 2);
        let (_tx, rx) = mpsc::channel::<Result<Block>>(1);
        drop(Download { rx, task: h });
        for _ in 0..100 {
            if std::sync::Arc::strong_count(&alive) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(std::sync::Arc::strong_count(&alive), 1);
    }

    #[test]
    fn restore_only_undoes_our_writes() {
        let hist = |rows: Vec<Vec<Json>>| crate::Statement {
            columns: ["version", "timestamp", "userId", "userName", "operation"]
                .iter()
                .map(|n| (n.to_string(), "STRING".into()))
                .collect(),
            rows,
            ..Default::default()
        };
        let row = |v: &str, user: &str, op: &str| {
            vec![json!(v), json!("t"), json!("1"), json!(user), json!(op)]
        };
        let ours = hist(vec![
            row("7", "yo@x.com", "WRITE"),
            row("6", "yo@x.com", "WRITE"),
            row("5", "otro", "WRITE"),
        ]);
        assert_eq!(history_version(&ours), Some(7));
        assert!(only_our_writes(&ours, 5, "yo@x.com", 2));
        assert!(!only_our_writes(&ours, 5, "yo@x.com", 1));
        assert!(!only_our_writes(&ours, 4, "yo@x.com", 3));
        let other_op = hist(vec![row("6", "yo@x.com", "OPTIMIZE")]);
        assert!(!only_our_writes(&other_op, 5, "yo@x.com", 2));
    }

    #[test]
    fn restore_needs_exactly_the_committed_writes() {
        let hist = crate::Statement {
            columns: ["version", "userName", "operation"]
                .iter()
                .map(|n| (n.to_string(), "STRING".into()))
                .collect(),
            rows: vec![
                vec![json!("7"), json!("yo"), json!("WRITE")],
                vec![json!("6"), json!("yo"), json!("WRITE")],
            ],
            ..Default::default()
        };
        assert!(only_our_writes(&hist, 5, "yo", 2));
        // One version more or less than the load committed isn't the load's.
        assert!(!only_our_writes(&hist, 5, "yo", 3));
        assert!(!only_our_writes(&hist, 5, "yo", 1));
    }

    #[test]
    fn undo_reads_every_committed_version() {
        // Regression: the history was capped at 1000 rows, so a failed load
        // past 1000 statements was refused as "otros cambios".
        let committed = 5_000;
        let (sql, max_rows) = undo_history_query("`c`.`s`.`t`", committed);
        assert_eq!(sql, "DESCRIBE HISTORY `c`.`s`.`t` LIMIT 5002");
        assert!(max_rows >= committed + 2);
        let hist = crate::Statement {
            columns: ["version", "userName", "operation"]
                .iter()
                .map(|n| (n.to_string(), "STRING".into()))
                .collect(),
            rows: (0..max_rows as i64)
                .rev()
                .map(|v| vec![json!(v.to_string()), json!("yo"), json!("WRITE")])
                .collect(),
            ..Default::default()
        };
        // Versions 5001 down to 0: the 5000 after version 1 are the load.
        assert!(only_our_writes(&hist, 1, "yo", committed));
        // The old cap kept the newest 1000 rows only, which can't match.
        let capped = crate::Statement {
            columns: hist.columns.clone(),
            rows: hist.rows[..1_000].to_vec(),
            ..Default::default()
        };
        assert!(!only_our_writes(&capped, 1, "yo", committed));
    }

    // --- Streaming reads (the memory in flight doesn't follow the chunk size).

    fn rows_of(b: &Block) -> Vec<Vec<Option<String>>> {
        match b {
            Block::Rows(r) => r.clone(),
            Block::Raw(data, ranges) => ranges
                .iter()
                .map(|(a, z)| {
                    serde_json::from_slice::<Vec<Json>>(&data[*a..*z])
                        .unwrap()
                        .into_iter()
                        .map(json_text)
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn splitter_finds_whole_rows_across_pieces() {
        let text = r#" [ ["1", null, "a]b[\"c\\"],
            ["2","x,y" ,"\\\"]"] ,["3",null,null] ] "#;
        let want: Vec<Vec<Option<String>>> = serde_json::from_str::<Vec<Vec<Json>>>(text)
            .unwrap()
            .into_iter()
            .map(|r| r.into_iter().map(json_text).collect())
            .collect();
        for step in 1..text.len() {
            let mut sp = Splitter::default();
            let mut got = Vec::new();
            for piece in text.as_bytes().chunks(step) {
                sp.feed(piece).unwrap();
                if let Some(b) = sp.block(1) {
                    got.extend(rows_of(&b));
                }
            }
            if let Some(b) = sp.finish().unwrap() {
                got.extend(rows_of(&b));
            }
            assert_eq!(got, want, "piezas de {step} bytes");
        }
    }

    #[test]
    fn splitter_refuses_cut_or_odd_answers() {
        for bad in [
            r#"[["1"],["2""#,
            r#"[["1"]"#,
            r#"[["1"]]]"#,
            r#"[["1"]] ["2"]"#,
            r#"{"a":1}"#,
            r#"["1"]"#,
            "",
        ] {
            let mut sp = Splitter::default();
            let r = sp
                .feed(bad.as_bytes())
                .and_then(|_| sp.finish().map(|_| ()));
            assert!(r.is_err(), "{bad}");
        }
        let mut sp = Splitter::default();
        sp.feed(b"[]").unwrap();
        assert!(sp.finish().unwrap().is_none());
    }

    /// A tiny HTTP server: one request per connection, answered by `h`
    /// (method, path, body) → (status, body).
    pub(crate) type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync>;
    use std::sync::Arc;

    pub(crate) async fn serve(h: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = vec![0u8; 1 << 16];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let (method, path) = (
                        first.next().unwrap_or("").to_string(),
                        first.next().unwrap_or("").to_string(),
                    );
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let (status, text) = h(&method, &path, &body);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        text.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(text.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn external_chunks_stream_in_bounded_blocks() {
        // ~6 MiB of rows in one link: handed over in blocks of about
        // BLOCK_BYTES, and a reader that doesn't take them stops the
        // download instead of buffering the chunk.
        let row = format!("[\"{}\",null]", "x".repeat(1000));
        let n = 6 * 1024;
        let body = format!("[{}]", vec![row; n].join(","));
        let base = serve(Arc::new(move |_, _, _| (200, body.clone()))).await;
        let chunk = json!({ "external_links": [{ "external_link": format!("{base}/chunk0"), "http_headers": {} }] });

        let (tx, mut rx) = mpsc::channel(BLOCKS_QUEUED);
        let http = reqwest::Client::new();
        let task = {
            let (http, chunk) = (http.clone(), chunk.clone());
            tokio::spawn(async move { send_chunk(&http, chunk, &tx).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "la descarga no espera al lector");
        let mut rows = 0;
        while let Some(b) = rx.recv().await {
            let b = b.unwrap();
            if let Block::Raw(data, _) = &b {
                assert!(
                    data.len() <= BLOCK_BYTES + 64 * 1024 + 2048,
                    "bloque de {} bytes",
                    data.len()
                );
            }
            rows += rows_of(&b).len();
        }
        task.await.unwrap().unwrap();
        assert_eq!(rows, n);
    }

    // --- Statements size by their escaped JSON.

    #[test]
    fn json_len_matches_serde() {
        for s in [
            "abc",
            "a\"b\\c",
            "\n\r\t\u{8}\u{c}",
            "\u{1}\u{1f}\u{7f}",
            "ñ€😀",
            "",
        ] {
            assert_eq!(
                json_len(s) + 2,
                serde_json::to_string(s).unwrap().len(),
                "{s:?}"
            );
        }
    }

    #[test]
    fn statements_are_capped_by_their_escaped_size() {
        // 4 MiB of backslashes as SQL: 8 MiB in the request's JSON, so two
        // such rows don't share a statement.
        let back = format!("('{}')", "\\".repeat(4 * 1024 * 1024));
        let mut st = Statements::new("INSERT INTO t (`s`) VALUES ".into(), &spec(1_000, u64::MAX));
        assert!(st.push(&back, 1).unwrap().is_empty());
        let ready = st.push(&back, 2).unwrap();
        assert_eq!(ready.len(), 1);
        assert!(json_len(&ready[0].0) <= STMT_BYTES);
        // 2.5 MiB of control characters: 15 MiB escaped, refused.
        let ctl = format!("('{}')", "\u{1}".repeat(5 * 512 * 1024));
        assert!(ctl.len() < STMT_BYTES);
        match st.push(&ctl, 3) {
            Err(Error::Unsupported(m)) => assert!(m.contains("La fila 3"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    // --- Map keys that don't survive JSON.

    #[test]
    fn map_keys_that_json_alters_are_refused() {
        for ty in [
            "map<binary,int>",
            "array<map<date,int>>",
            "map<timestamp,string>",
            "struct<a:map<timestamp_ntz,int>>",
        ] {
            assert!(unreadable_map_key(ty).is_some(), "{ty}");
        }
        for ty in [
            "map<string,int>",
            "map<int,map<string,bigint>>",
            "map<decimal(10,2),string>",
            "map<boolean,double>",
            "array<int>",
        ] {
            assert_eq!(unreadable_map_key(ty), None, "{ty}");
        }
        match target("m", "map<binary,int>") {
            Err(Error::Unsupported(m)) => assert!(m.contains("binary"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            target("m", "map<date,int>").unwrap().json,
            JsonLoad::Keys { .. }
        ));
    }

    // --- A load statement is followed to its end.

    pub(crate) fn session(base: String) -> DatabricksSession {
        DatabricksSession {
            api: Api {
                http: reqwest::Client::new(),
                base,
                auth: crate::Auth::Pat("t".into()),
                diag_op: Default::default(),
            },
            warehouse: "w".into(),
            catalog: None,
            schema: None,
            running: Arc::new(std::sync::Mutex::new(None)),
            profiler: None,
            uc_functions: None,
        }
    }

    /// A server whose statement `s1` is PENDING and whose status polls
    /// fail; once cancelled, the polls answer `after_cancel` (None: they
    /// keep failing). Gives the base URL and the cancels received.
    async fn flaky(
        after_cancel: Option<&'static str>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cancels = Arc::new(AtomicUsize::new(0));
        let c = cancels.clone();
        let base = serve(Arc::new(move |m, p, _| match (m, p) {
            ("POST", "/api/2.0/sql/statements") => (
                200,
                r#"{"statement_id":"s1","status":{"state":"PENDING"}}"#.into(),
            ),
            ("POST", "/api/2.0/sql/statements/s1/cancel") => {
                c.fetch_add(1, Ordering::SeqCst);
                (200, "{}".into())
            }
            ("GET", "/api/2.0/sql/statements/s1") => match after_cancel {
                Some(state) if c.load(Ordering::SeqCst) > 0 => (
                    200,
                    format!(r#"{{"statement_id":"s1","status":{{"state":"{state}"}}}}"#),
                ),
                _ => (503, r#"{"message":"temporarily unavailable"}"#.into()),
            },
            _ => (404, "{}".into()),
        }))
        .await;
        (base, cancels)
    }

    #[tokio::test]
    async fn a_statement_that_cant_be_followed_is_cancelled_before_failing() {
        let (base, cancels) = flaky(Some("CANCELED")).await;
        match session(base).exec("INSERT INTO t VALUES (1)").await {
            Err(Exec::NotApplied(Error::Query(m))) => {
                assert!(m.contains("se canceló") && m.contains("temporarily"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        assert!(cancels.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn a_statement_that_committed_before_the_cancel_counts_as_committed() {
        let (base, _) = flaky(Some("SUCCEEDED")).await;
        assert!(session(base).exec("INSERT INTO t VALUES (1)").await.is_ok());
    }

    #[tokio::test]
    async fn a_statement_whose_end_is_unknown_says_so() {
        let (base, cancels) = flaky(None).await;
        match session(base).exec("INSERT INTO t VALUES (1)").await {
            Err(Exec::Unknown(e)) => assert!(e.to_string().contains("s1"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(cancels.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn a_failed_submit_is_unknown_only_if_it_may_have_run() {
        let base = serve(Arc::new(|_, _, _| (500, r#"{"message":"boom"}"#.into()))).await;
        assert!(matches!(
            session(base).exec("INSERT INTO t VALUES (1)").await,
            Err(Exec::Unknown(_))
        ));
        let base = serve(Arc::new(|_, _, _| {
            (429, r#"{"message":"slow down"}"#.into())
        }))
        .await;
        assert!(matches!(
            session(base).exec("INSERT INTO t VALUES (1)").await,
            Err(Exec::NotApplied(_))
        ));
        let base = serve(Arc::new(|_, _, _| {
            (
                200,
                r#"{"statement_id":"s1","status":{"state":"FAILED","error":{"message":"bad"}}}"#
                    .into(),
            )
        }))
        .await;
        assert!(
            matches!(session(base).exec("INSERT INTO t VALUES (1)").await, Err(Exec::NotApplied(Error::Query(m))) if m == "bad")
        );
        let base = serve(Arc::new(|_, _, _| {
            (
                200,
                r#"{"statement_id":"s1","status":{"state":"CLOSED"}}"#.into(),
            )
        }))
        .await;
        assert!(session(base).exec("INSERT INTO t VALUES (1)").await.is_ok());
    }

    #[tokio::test]
    async fn an_unreadable_history_is_an_error_not_a_clean_table() {
        // DESCRIBE HISTORY answers without a `version` column.
        let base = serve(Arc::new(|_, _, _| {
            (
                200,
                r#"{"statement_id":"h","status":{"state":"SUCCEEDED"},"manifest":{"schema":{"columns":[{"name":"x","type_name":"STRING"}]},"total_row_count":1},"result":{"data_array":[["?"]]}}"#.into(),
            )
        }))
        .await;
        let e = session(base)
            .undo_load("`t`", 5, 2, Error::Query("falló la fila 30".into()))
            .await;
        let m = e.to_string();
        assert!(
            m.contains("falló la fila 30")
                && m.contains("no se pudieron deshacer")
                && m.contains("versión Delta"),
            "{m}"
        );
    }
}

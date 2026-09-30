//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! - **Reading** ([`read_batches`]): one streamed `SELECT`, rows turned into
//!   typed cells as they arrive (never the whole table in memory). MySQL,
//!   MariaDB and TiDB answer a prepared statement (binary protocol: typed
//!   integers, floats, dates and times); the other engines the text protocol,
//!   parsed by the column's type. Either way: `BIGINT UNSIGNED` → `UInt`,
//!   `DECIMAL` → exact text, binary strings, BLOBs and geometries → whole
//!   `Bytes` (a geometry is MySQL's value: 4-byte SRID + WKB), `BIT` →
//!   `UInt`, `JSON` → `Json`, ENUM/SET → text, a `TIME` out of 00–24 h
//!   (negative or over a day) → text. The session reads in UTC so a
//!   `TIMESTAMP` becomes a `DateTimeTz` with `+00:00`, exact. Manticore
//!   pages by `id` (a plain SELECT stops at `max_matches`).
//! - **Loading** ([`bulk_load`]):
//!   - MySQL, MariaDB, OceanBase, SingleStore (and Aurora, Cloud SQL):
//!     `LOAD DATA LOCAL INFILE` fed from memory, one statement per commit
//!     window, as escaped TSV (`\N` for NULL), sent in pieces well under
//!     `max_allowed_packet` (a big BLOB or text is cut mid-field too).
//!     Binary, BLOB and geometry columns travel as hex through `UNHEX(@v)`
//!     and `BIT` columns as a number through `CAST(@v AS UNSIGNED)`, so
//!     every byte comes back as it went. `LOCAL` turns bad values and
//!     duplicate keys into warnings, so a window with warnings (other than
//!     notes) or fewer rows than sent is rolled back and fails. When the
//!     server has `local_infile` disabled (MySQL 8's default), the load uses
//!     multi-row `INSERT`s as server-side prepared statements, as big as
//!     `max_allowed_packet` and the 65,535 placeholders allow.
//!   - TiDB: always the prepared `INSERT`s. Its `LOAD DATA` commits on its
//!     own whatever `autocommit` says (and commits the open transaction with
//!     it), so a failed window couldn't be rolled back.
//!   - Either way, bytes bound for a text column must be valid UTF-8 (the
//!     server would drop or replace the rest, sometimes without a warning),
//!     and a `FLOAT` column takes the `f32` a value stands for (the shortest
//!     text of `FLT_MAX`, 3.4028235e38, is above it as a double).
//!   - StarRocks, Doris (and VeloDB), Databend, GreptimeDB: multi-row
//!     `INSERT … VALUES` of up to 1 MiB or 4,000 rows per statement (text literals; these
//!     engines have no `LOAD DATA LOCAL` and not all of them prepare
//!     statements). Each statement is a load of its own and commits
//!     alone. Stream Load (StarRocks, Doris) would be faster but needs HTTP
//!     to the FE/BE ports, and this crate has no HTTP client.
//!   - Manticore: no bulk load (no `LOAD DATA`, no NULL); the migration
//!     writes its `insert_script`.
//!
//!   The load session (MySQL family) runs with autocommit off, commits each
//!   window, in UTC (`DateTimeTz` values are converted to UTC) and without
//!   `NO_BACKSLASH_ESCAPES`; `keep_identity` adds `NO_AUTO_VALUE_ON_ZERO`
//!   (explicit AUTO_INCREMENT values are always written, zeros too), and
//!   `table_lock` takes `LOCK TABLES … WRITE` on MySQL and MariaDB. Unique
//!   and foreign key checks stay on: turning them off can let in rows the
//!   engine would refuse, which changes the result. The session's settings
//!   are restored at the end.

use crate::cells::{type_name, value_text};
use crate::session::{lit, MySqlSession};
use crate::{err, Variant};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{Error, Result, Session};
use futures::channel::{mpsc, oneshot};
use futures::future::{select, Either};
use futures::SinkExt;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Column, Conn, InfileData, Params, QueryResult, Row, Value};
use std::io::{self, Write};

/// MySQL's charset number for binary strings (BLOB, BINARY, VARBINARY).
const BINARY_CHARSET: u16 = 63;
/// Rows per page when reading Manticore.
const MANTICORE_PAGE: usize = 10_000;
/// Bytes handed to `LOAD DATA` at a time.
const SEND_CHUNK: usize = 1024 * 1024;
/// A text `INSERT` closes at this many bytes.
const TEXT_STATEMENT: usize = 1024 * 1024;
/// …or at this many rows: StarRocks refuses more than 10,000 (its
/// `expr_children_limit`), and its FE plans VALUES rows one by one, with
/// memory to match (bigger statements aren't faster, and a 10,000-row one
/// can take down a small FE).
const TEXT_ROWS: u64 = 4_000;
/// Bound of a prepared `INSERT`'s parameters (well under the packet).
const PREPARED_STATEMENT: usize = 8 * 1024 * 1024;
/// A TiDB transaction is kept small (txn-total-size-limit is 100 MB).
const TIDB_WINDOW: u64 = 32 * 1024 * 1024;

// ---------------------------------------------------------------- reading

/// How a column's values become cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Int,
    /// `BIGINT UNSIGNED`.
    UInt,
    Float,
    Double,
    Decimal,
    Date,
    DateTime,
    /// Read in UTC: a `DateTimeTz` with `+00:00`.
    Timestamp,
    Time,
    Json,
    Bit,
    Bytes,
    Text,
}

pub(crate) fn kind(col: &Column, utc: bool) -> Kind {
    use ColumnType::*;
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    match col.column_type() {
        MYSQL_TYPE_LONGLONG if unsigned => Kind::UInt,
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => Kind::Int,
        MYSQL_TYPE_FLOAT => Kind::Float,
        MYSQL_TYPE_DOUBLE => Kind::Double,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Kind::Decimal,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Kind::Date,
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 if utc => Kind::Timestamp,
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => Kind::DateTime,
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Kind::Time,
        MYSQL_TYPE_JSON => Kind::Json,
        MYSQL_TYPE_BIT => Kind::Bit,
        MYSQL_TYPE_GEOMETRY => Kind::Bytes,
        _ if col.character_set() == BINARY_CHARSET => Kind::Bytes,
        _ => Kind::Text,
    }
}

/// Types some engines send as plain strings, known by the catalog:
/// StarRocks' `LARGEINT` (its catalog says `bigint unsigned`; a value past
/// 64 bits stays an exact decimal) and `JSON`.
fn refine(k: Kind, catalog_type: Option<&str>) -> Kind {
    let Some(t) = catalog_type.map(str::to_ascii_lowercase) else { return k };
    let int = ["tinyint", "smallint", "mediumint", "int", "bigint", "largeint"].iter().any(|p| t.starts_with(p));
    match k {
        Kind::Text if int => Kind::Int,
        Kind::Text if t.starts_with("decimal") => Kind::Decimal,
        Kind::Text if t.starts_with("json") => Kind::Json,
        k => k,
    }
}

fn frac(us: u32) -> String {
    if us == 0 {
        String::new()
    } else {
        format!(".{us:06}")
    }
}

/// Text that isn't a plain number stays exact: digits as a decimal,
/// anything else as text.
fn number_text(s: String) -> Cell {
    let digits = s.strip_prefix('-').unwrap_or(&s);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        Cell::Decimal(s)
    } else {
        Cell::Text(s)
    }
}

/// `HH:MM:SS[.f]` within a day is a `Time`; a negative or longer one text.
fn time_text(s: String) -> Cell {
    let hours = s.split(':').next().and_then(|h| h.parse::<u32>().ok());
    match hours {
        Some(h) if h < 24 => Cell::Time(s),
        _ => Cell::Text(s),
    }
}

pub(crate) fn to_cell(kind: Kind, v: Value) -> Cell {
    match v {
        Value::NULL => Cell::Null,
        Value::Int(i) => match kind {
            Kind::Float | Kind::Double => Cell::Float(i as f64),
            _ => Cell::Int(i),
        },
        Value::UInt(u) => match kind {
            Kind::UInt => Cell::UInt(u),
            Kind::Float | Kind::Double => Cell::Float(u as f64),
            _ => i64::try_from(u).map_or(Cell::UInt(u), Cell::Int),
        },
        Value::Float(f) => Cell::Float(f32_value(f.to_string().parse().unwrap_or(f64::from(f)))),
        Value::Double(f) => Cell::Float(f),
        Value::Date(y, mo, d, h, mi, s, us) => {
            if kind == Kind::Date && (h, mi, s, us) == (0, 0, 0, 0) {
                Cell::Date(format!("{y:04}-{mo:02}-{d:02}"))
            } else {
                let t = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{}", frac(us));
                if kind == Kind::Timestamp {
                    Cell::DateTimeTz(t + "+00:00")
                } else {
                    Cell::DateTime(t)
                }
            }
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            let t = format!("{}{hours:02}:{mi:02}:{s:02}{}", if neg { "-" } else { "" }, frac(us));
            if neg || hours >= 24 {
                Cell::Text(t)
            } else {
                Cell::Time(t)
            }
        }
        Value::Bytes(b) => bytes_cell(kind, b),
    }
}

/// A `FLOAT`'s value as a double: the shortest text of the `f32` (0.1, not
/// 0.10000000149011612) when it stays within the `f32` range, the exact
/// `f32` otherwise. The shortest text of `±FLT_MAX` (3.4028235e38) is above
/// `FLT_MAX` as a double, and MySQL refuses it for a `FLOAT` column.
fn f32_value(f: f64) -> f64 {
    let max = f64::from(f32::MAX);
    if f.abs() > max && (f as f32).is_finite() {
        f64::from(f as f32)
    } else {
        f
    }
}

/// A value the server sent as bytes (every value in the text protocol).
fn bytes_cell(kind: Kind, b: Vec<u8>) -> Cell {
    let text = |b: Vec<u8>| String::from_utf8(b).map_err(|e| e.into_bytes());
    match kind {
        Kind::Bytes => Cell::Bytes(b),
        Kind::Bit => {
            if b.len() <= 8 {
                Cell::UInt(b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x)))
            } else {
                Cell::Bytes(b)
            }
        }
        _ => match text(b) {
            Err(b) => Cell::Bytes(b),
            Ok(s) => match kind {
                Kind::Int => s.parse::<i64>().map_or_else(|_| s.parse::<u64>().map_or_else(|_| number_text(s), Cell::UInt), Cell::Int),
                Kind::UInt => s.parse::<u64>().map_or_else(|_| number_text(s), Cell::UInt),
                Kind::Float => s.parse::<f64>().map_or(Cell::Text(s), |f| Cell::Float(f32_value(f))),
                Kind::Double => s.parse::<f64>().map_or(Cell::Text(s), Cell::Float),
                Kind::Decimal => Cell::Decimal(s),
                Kind::Date => Cell::Date(s),
                Kind::DateTime => Cell::DateTime(s),
                Kind::Timestamp => Cell::DateTimeTz(s + "+00:00"),
                Kind::Time => time_text(s),
                Kind::Json => Cell::Json(s),
                _ => Cell::Text(s),
            },
        },
    }
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

/// Streams result rows into batches.
struct Reader {
    sink: BatchSinkRef,
    builder: BatchBuilder,
    began: bool,
    utc: bool,
    /// Catalog types (name, type, nullable) for the columns' descriptions.
    catalog: Vec<(String, String, bool)>,
    /// Manticore's cursor column, when read only to page.
    cursor: Option<Cursor>,
    last_id: Option<i64>,
}

#[derive(Clone, Copy)]
struct Cursor {
    index: usize,
    /// Added to the column list for paging: not handed over.
    hidden: bool,
}

impl Reader {
    async fn pump<P: Protocol>(&mut self, result: &mut QueryResult<'_, '_, P>) -> Result<usize> {
        let Some(cols) = result.columns() else {
            return Ok(0);
        };
        let kinds: Vec<Kind> = cols
            .iter()
            .map(|c| {
                let name = c.name_str();
                let catalog = self.catalog.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(&name)).map(|(_, t, _)| t.as_str());
                refine(kind(c, self.utc), catalog)
            })
            .collect();
        let hidden = self.cursor.filter(|c| c.hidden).map(|c| c.index);
        if !self.began {
            self.began = true;
            let described: Vec<TransferColumn> = cols
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != hidden)
                .map(|(_, c)| {
                    let name = c.name_str().into_owned();
                    match self.catalog.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(&name)) {
                        Some((_, t, nullable)) => TransferColumn { name, type_name: t.clone(), nullable: *nullable },
                        None => TransferColumn { name, type_name: type_name(c.column_type()), nullable: true },
                    }
                })
                .collect();
            self.sink.lock().map_err(lock_err)?.begin(&described)?;
        }
        let mut n = 0;
        while let Some(row) = result.next().await.map_err(err)? {
            n += 1;
            let values = row.unwrap();
            if let Some(c) = self.cursor {
                self.last_id = match values.get(c.index) {
                    Some(Value::Int(i)) => Some(*i),
                    Some(Value::UInt(u)) => i64::try_from(*u).ok(),
                    Some(Value::Bytes(b)) => std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()),
                    _ => self.last_id,
                };
            }
            let cells: Vec<Cell> = values
                .into_iter()
                .zip(&kinds)
                .enumerate()
                .filter(|(i, _)| Some(*i) != hidden)
                .map(|(_, (v, k))| to_cell(*k, v))
                .collect();
            let mut sink = self.sink.lock().map_err(lock_err)?;
            self.builder.push(cells, &mut *sink)?;
        }
        Ok(n)
    }

    fn finish(&mut self) -> Result<u64> {
        let mut sink = self.sink.lock().map_err(lock_err)?;
        if !self.began {
            sink.begin(&[])?;
        }
        self.builder.flush(&mut *sink)?;
        Ok(self.builder.rows)
    }
}

/// Read `spec`'s table in typed batches (see the module docs).
pub(crate) async fn read_batches(s: &mut MySqlSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let v = s.variant;
    let catalog: Vec<(String, String, bool)> =
        s.columns(&spec.table).await.unwrap_or_default().into_iter().map(|c| (c.name, c.data_type, c.nullable)).collect();
    // TIMESTAMPs in UTC; the session's zone comes back afterwards.
    let zone = if v == Variant::Manticore { None } else { switch_zone(&mut s.conn).await };
    let mut reader =
        Reader { sink, builder: BatchBuilder::new(), began: false, utc: zone.is_some(), catalog, cursor: None, last_id: None };
    let r = if v == Variant::Manticore { read_manticore(s, spec, &mut reader).await } else { read_plain(s, spec, &mut reader).await };
    if let Some(z) = zone {
        if let Err(e) = s.conn.query_drop(format!("SET time_zone = {}", lit(&z))).await {
            tracing::debug!("mysql: restoring time_zone: {e}");
        }
    }
    r?;
    reader.finish()
}

/// `SET time_zone = '+00:00'`, returning the previous zone.
async fn switch_zone(conn: &mut Conn) -> Option<String> {
    let row: Row = conn.query_first("SELECT @@SESSION.time_zone").await.ok().flatten()?;
    let zone = row.as_ref(0).and_then(value_text)?;
    conn.query_drop("SET time_zone = '+00:00'").await.ok()?;
    Some(zone)
}

fn column_list(columns: &Option<Vec<String>>) -> String {
    match columns {
        Some(c) if !c.is_empty() => c.iter().map(|c| quote_ident(Quote::Backtick, c)).collect::<Vec<_>>().join(", "),
        _ => "*".into(),
    }
}

async fn read_plain(s: &mut MySqlSession, spec: &ReadSpec, reader: &mut Reader) -> Result<()> {
    let table = qualified_name(Quote::Backtick, spec.table.schema(), &spec.table.name);
    let mut sql = format!("SELECT {} FROM {table}", column_list(&spec.columns));
    if let Some(f) = spec.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    if matches!(s.variant, Variant::MySql | Variant::MariaDb | Variant::TiDb) {
        // Binary protocol: typed values, no text to parse.
        let mut result = s.conn.exec_iter(sql.as_str(), ()).await.map_err(err)?;
        reader.pump(&mut result).await?;
        result.drop_result().await.map_err(err)
    } else {
        let mut result = s.conn.query_iter(sql.as_str()).await.map_err(err)?;
        reader.pump(&mut result).await?;
        result.drop_result().await.map_err(err)
    }
}

/// Manticore returns at most `max_matches` rows per query: pages by `id`.
async fn read_manticore(s: &mut MySqlSession, spec: &ReadSpec, reader: &mut Reader) -> Result<()> {
    let table = quote_ident(Quote::Backtick, &spec.table.name);
    let (list, hidden) = match &spec.columns {
        Some(c) if !c.is_empty() => {
            let has_id = c.iter().any(|c| c.eq_ignore_ascii_case("id"));
            let list = column_list(&spec.columns);
            if has_id {
                (list, false)
            } else {
                (format!("{list}, id"), true)
            }
        }
        _ => ("*".to_string(), false),
    };
    let filter = spec.filter.as_deref().filter(|f| !f.trim().is_empty());
    loop {
        let mut conds: Vec<String> = filter.map(|f| vec![format!("({f})")]).unwrap_or_default();
        if let Some(id) = reader.last_id {
            conds.push(format!("id > {id}"));
        }
        let wher = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
        let sql = format!("SELECT {list} FROM {table}{wher} ORDER BY id ASC LIMIT {MANTICORE_PAGE} OPTION max_matches={MANTICORE_PAGE}");
        let mut result = s.conn.query_iter(sql.as_str()).await.map_err(err)?;
        if reader.cursor.is_none() {
            let cols = result.columns().unwrap_or_else(|| Vec::new().into());
            let index = cols.iter().rposition(|c| c.name_str().eq_ignore_ascii_case("id")).ok_or_else(|| {
                Error::Query("Manticore: la tabla no devolvió la columna id".into())
            })?;
            reader.cursor = Some(Cursor { index, hidden });
        }
        let n = reader.pump(&mut result).await?;
        result.drop_result().await.map_err(err)?;
        if n < MANTICORE_PAGE {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------- loading

/// How a target column takes its values in `LOAD DATA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Escaped text.
    Plain,
    /// Hex, through `UNHEX(@v)` (binary strings, BLOBs, geometries).
    Hex,
    /// An AUTO_INCREMENT column: escaped text, through a variable when the
    /// load has a SET clause (see `load_local`).
    Auto,
    /// A number, through `CAST(@v AS UNSIGNED)` (`BIT`).
    Bit,
    /// Escaped text, a double brought into the `f32` range (`FLOAT`).
    Float,
}

pub(crate) fn mode_of(data_type: &str) -> Mode {
    match data_type.to_ascii_lowercase().as_str() {
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "geometry" | "point" | "linestring"
        | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" | "geomcollection" => Mode::Hex,
        "bit" => Mode::Bit,
        "float" => Mode::Float,
        _ => Mode::Plain,
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn put_hex(out: &mut Vec<u8>, b: &[u8]) {
    out.reserve(b.len() * 2);
    for x in b {
        out.push(HEX[usize::from(x >> 4)]);
        out.push(HEX[usize::from(x & 15)]);
    }
}

/// Bytes as `LOAD DATA` reads them with `ESCAPED BY '\\'`.
pub(crate) fn put_escaped(out: &mut Vec<u8>, b: &[u8]) {
    let mut rest = b;
    while let Some(i) = rest.iter().position(|c| matches!(c, b'\\' | b'\t' | b'\n' | b'\r' | 0)) {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(match rest[i] {
            b'\\' => b"\\\\",
            b'\t' => b"\\t",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            _ => b"\\0",
        });
        rest = &rest[i + 1..];
    }
    out.extend_from_slice(rest);
}

fn float_text(f: f64) -> Result<String> {
    if f.is_finite() {
        // Debug: shortest round-trip text, exponent for very large / small.
        Ok(format!("{f:?}"))
    } else {
        Err(Error::Query(format!("MySQL no guarda el valor {f}")))
    }
}

/// The 16 bytes of a UUID's text (for a `BINARY(16)` column).
fn uuid_bytes(s: &str) -> Option<Vec<u8>> {
    let hex: Vec<u8> = s.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    hex.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()
}

/// A cell's text, as the server parses it (`None` for NULL, and for bytes,
/// which the caller writes as they are).
fn cell_text(c: &Cell) -> Result<Option<std::borrow::Cow<'_, str>>> {
    use std::borrow::Cow;
    Ok(Some(match c {
        Cell::Null | Cell::Bytes(_) => return Ok(None),
        Cell::Bool(b) => Cow::Borrowed(if *b { "1" } else { "0" }),
        Cell::Int(i) => Cow::Owned(i.to_string()),
        Cell::UInt(u) => Cow::Owned(u.to_string()),
        Cell::Float(f) => Cow::Owned(float_text(*f)?),
        Cell::DateTimeTz(s) => Cow::Owned(to_utc(s).unwrap_or_else(|| s.clone())),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::Uuid(s) | Cell::Json(s) => {
            Cow::Borrowed(s.as_str())
        }
    }))
}

/// Bytes bound for a text column: only valid UTF-8 (the load's character
/// set) goes through. The server would drop the rest, or replace it with
/// `?`, and MySQL 8 does that for a prepared statement's parameter
/// without even a warning.
fn utf8_text(b: &[u8]) -> Result<&[u8]> {
    match std::str::from_utf8(b) {
        Ok(_) => Ok(b),
        Err(_) => Err(Error::Query(
            "un valor binario que no es texto UTF-8 válido no se puede cargar en una columna de texto sin perder bytes".into(),
        )),
    }
}

/// A double for a `FLOAT` column (see [`f32_value`]).
fn float_for(f: f64, mode: Mode) -> f64 {
    if mode == Mode::Float {
        f32_value(f)
    } else {
        f
    }
}

/// One field of a `LOAD DATA` line.
pub(crate) fn put_field(out: &mut Vec<u8>, c: &Cell, mode: Mode) -> Result<()> {
    if matches!(c, Cell::Null) {
        out.extend_from_slice(b"\\N");
        return Ok(());
    }
    match (mode, c) {
        (Mode::Hex, Cell::Bytes(b)) => put_hex(out, b),
        (Mode::Hex, Cell::Uuid(s)) => match uuid_bytes(s) {
            Some(b) => put_hex(out, &b),
            None => put_hex(out, s.as_bytes()),
        },
        (Mode::Hex, c) => put_hex(out, cell_text(c)?.unwrap_or_default().as_bytes()),
        (Mode::Bit, Cell::Bytes(b)) if b.len() <= 8 => {
            write!(out, "{}", b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x))).map_err(Error::from)?
        }
        (_, Cell::Bytes(b)) => put_escaped(out, utf8_text(b)?),
        (Mode::Float, Cell::Float(f)) => put_escaped(out, float_text(f32_value(*f))?.as_bytes()),
        (_, c) => put_escaped(out, cell_text(c)?.unwrap_or_default().as_bytes()),
    }
    Ok(())
}

/// The values [`put_field`] would refuse, checked before a row is sent:
/// a stream aborted with an error closes the connection, while one that
/// just ends leaves whole rows that the ROLLBACK takes back.
fn check_row(row: &[Cell], modes: &[Mode]) -> Result<()> {
    for (i, c) in row.iter().enumerate() {
        match (modes.get(i).copied().unwrap_or(Mode::Plain), c) {
            (_, Cell::Float(f)) => {
                float_text(*f)?;
            }
            (Mode::Hex, Cell::Bytes(_)) => {}
            (Mode::Bit, Cell::Bytes(b)) if b.len() <= 8 => {}
            (_, Cell::Bytes(b)) => {
                utf8_text(b)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A field too big to encode in one piece: its bytes, and whether they go
/// as hex. `None` for the rest (at most `limit` bytes before encoding).
fn big_field(c: &Cell, mode: Mode, limit: usize) -> Result<Option<(&[u8], bool)>> {
    let (b, hex) = match (mode, c) {
        (Mode::Hex, Cell::Bytes(b)) => (b.as_slice(), true),
        (Mode::Hex, Cell::Text(s) | Cell::Json(s)) => (s.as_bytes(), true),
        (Mode::Bit, Cell::Bytes(_)) => return Ok(None),
        (_, Cell::Bytes(b)) => (b.as_slice(), false),
        (_, Cell::Text(s) | Cell::Json(s)) => (s.as_bytes(), false),
        _ => return Ok(None),
    };
    if b.len() <= limit {
        return Ok(None);
    }
    if !hex && matches!(c, Cell::Bytes(_)) {
        utf8_text(b)?;
    }
    Ok(Some((b, hex)))
}

/// A row as a `LOAD DATA` line (in one piece; the load uses
/// [`put_row_chunked`]).
#[cfg(test)]
pub(crate) fn put_row(out: &mut Vec<u8>, row: &[Cell], modes: &[Mode]) -> Result<()> {
    for (i, c) in row.iter().enumerate() {
        if i > 0 {
            out.push(b'\t');
        }
        put_field(out, c, modes.get(i).copied().unwrap_or(Mode::Plain))?;
    }
    out.push(b'\n');
    Ok(())
}

/// A cell as a prepared statement's parameter.
fn param(c: &Cell, mode: Mode) -> Result<Value> {
    Ok(match (mode, c) {
        (_, Cell::Null) => Value::NULL,
        (_, Cell::Bytes(b)) if mode == Mode::Bit && b.len() <= 8 => Value::UInt(b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x))),
        (Mode::Hex, Cell::Bytes(b)) => Value::Bytes(b.clone()),
        (_, Cell::Bytes(b)) => Value::Bytes(utf8_text(b)?.to_vec()),
        (Mode::Hex, Cell::Uuid(s)) => Value::Bytes(uuid_bytes(s).unwrap_or_else(|| s.as_bytes().to_vec())),
        (_, Cell::Bool(b)) => Value::Int(i64::from(*b)),
        (_, Cell::Int(i)) => Value::Int(*i),
        (_, Cell::UInt(u)) => Value::UInt(*u),
        (_, Cell::Float(f)) => {
            float_text(*f)?;
            Value::Double(float_for(*f, mode))
        }
        (_, c) => Value::Bytes(cell_text(c)?.unwrap_or_default().into_owned().into_bytes()),
    })
}

/// A cell as a SQL literal (text `INSERT`s; backslashes are escapes).
pub(crate) fn literal(c: &Cell) -> Result<String> {
    Ok(match c {
        Cell::Null => "NULL".into(),
        Cell::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => float_text(*f)?,
        Cell::Decimal(s) => s.clone(),
        Cell::Bytes(b) => {
            let mut out = Vec::with_capacity(3 + b.len() * 2);
            out.extend_from_slice(b"X'");
            put_hex(&mut out, b);
            out.push(b'\'');
            String::from_utf8(out).unwrap_or_default()
        }
        c => lit(&cell_text(c)?.unwrap_or_default()),
    })
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.f]±HH:MM` (or `Z`) as UTC without a zone.
pub(crate) fn to_utc(s: &str) -> Option<String> {
    let s = s.trim();
    let (base, sign, oh, om) = if let Some(b) = s.strip_suffix('Z') {
        (b, 1, 0, 0)
    } else {
        let i = s.get(10..)?.rfind(['+', '-'])? + 10;
        let (base, off) = s.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let off = &off[1..];
        let (h, m) = off.split_once(':').unwrap_or((off.get(..2)?, off.get(2..).unwrap_or("0")));
        (base, sign, h.parse::<i64>().ok()?, if m.is_empty() { 0 } else { m.parse::<i64>().ok()? })
    };
    let (date, time) = base.split_once([' ', 'T'])?;
    let mut d = date.splitn(3, '-');
    let (y, mo, da): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (hms, fraction) = match time.split_once('.') {
        Some((a, b)) => (a, format!(".{b}")),
        None => (time, String::new()),
    };
    let mut t = hms.splitn(3, ':');
    let (h, mi, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next().unwrap_or("0").parse().ok()?);
    let minutes = days_from_civil(y, mo, da) * 1440 + h * 60 + mi - sign * (oh * 60 + om);
    let (days, m) = (minutes.div_euclid(1440), minutes.rem_euclid(1440));
    let (y, mo, da) = civil_from_days(days);
    Some(format!("{y:04}-{mo:02}-{da:02} {:02}:{:02}:{sec:02}{fraction}", m / 60, m % 60))
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// How an engine takes a bulk load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// `LOAD DATA LOCAL INFILE` (prepared `INSERT`s when it's disabled).
    LocalInfile,
    /// Multi-row text `INSERT`s.
    TextInserts,
}

fn path(v: Variant) -> Option<Path> {
    match v.base() {
        Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase | Variant::SingleStore => Some(Path::LocalInfile),
        Variant::StarRocks | Variant::Doris | Variant::Databend | Variant::GreptimeDb => Some(Path::TextInserts),
        _ => None,
    }
}

/// The variant has a bulk load here (Manticore doesn't).
pub(crate) fn supported(v: Variant) -> bool {
    path(v).is_some()
}

/// The commit window's bounds.
struct Window {
    rows: u64,
    bytes: u64,
}

impl Window {
    fn new(v: Variant, spec: &LoadSpec) -> Self {
        let mut bytes = spec.commit_bytes.max(1);
        if v.base() == Variant::TiDb {
            bytes = bytes.min(TIDB_WINDOW);
        }
        Window { rows: spec.commit_rows.max(1), bytes }
    }
    fn full(&self, rows: u64, bytes: u64) -> bool {
        rows >= self.rows || bytes >= self.bytes
    }
}

/// Bulk load `source` into `spec.table` (see the module docs).
pub(crate) async fn bulk_load(s: &mut MySqlSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    let v = s.variant;
    let Some(p) = path(v) else {
        return Err(Error::Unsupported("Manticore Search no tiene carga masiva: se usan INSERT".into()));
    };
    let table = qualified_name(Quote::Backtick, spec.table.schema(), &spec.table.name);
    let window = Window::new(v, spec);
    if p == Path::TextInserts {
        return load_text(&mut s.conn, &table, spec, &window, source, progress).await;
    }
    let modes = target_modes(&mut s.conn, spec).await?;
    let saved = prepare_session(&mut s.conn, v, spec, &table).await?;
    let r = if v.base() == Variant::TiDb {
        // TiDB's LOAD DATA commits by itself (autocommit off or not, and the
        // open transaction with it): a failed window would stay loaded.
        load_prepared(&mut s.conn, &table, spec, &modes, &window, None, source, progress, 0).await
    } else {
        load_local(&mut s.conn, &table, spec, &modes, &window, source, progress).await
    };
    if r.is_err() {
        if let Err(e) = s.conn.query_drop("ROLLBACK").await {
            // `SET autocommit = 1` would commit the failed window: the
            // session keeps autocommit off (and the connection, if alive,
            // its transaction open) rather than that.
            tracing::debug!("mysql: ROLLBACK after a failed load: {e}");
            return r;
        }
    }
    restore_session(&mut s.conn, saved).await;
    r
}

/// Each load column's mode, from the target's catalog.
async fn target_modes(conn: &mut Conn, spec: &LoadSpec) -> Result<Vec<Mode>> {
    let schema = spec.table.schema().map_or_else(|| "DATABASE()".to_string(), lit);
    let sql = format!(
        "SELECT COLUMN_NAME, DATA_TYPE, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {}",
        lit(&spec.table.name)
    );
    let rows: Vec<Row> = conn.query(sql).await.map_err(err)?;
    let types: Vec<(String, Mode)> = rows
        .iter()
        .filter_map(|r| {
            let extra = r.as_ref(2).and_then(value_text).unwrap_or_default().to_ascii_lowercase();
            let mode = if extra.contains("auto_increment") { Mode::Auto } else { mode_of(&r.as_ref(1).and_then(value_text)?) };
            Some((r.as_ref(0).and_then(value_text)?, mode))
        })
        .collect();
    Ok(spec
        .columns
        .iter()
        .map(|c| types.iter().find(|(n, _)| n.eq_ignore_ascii_case(c)).map_or(Mode::Plain, |(_, m)| *m))
        .collect())
}

/// The session's settings before the load.
struct Saved {
    sql_mode: String,
    time_zone: String,
    autocommit: String,
    locked: bool,
}

async fn prepare_session(conn: &mut Conn, v: Variant, spec: &LoadSpec, table: &str) -> Result<Saved> {
    let row: Row = conn
        .query_first("SELECT @@SESSION.sql_mode, @@SESSION.time_zone, @@SESSION.autocommit")
        .await
        .map_err(err)?
        .ok_or_else(|| Error::Query("no se pudo leer la configuración de la sesión".into()))?;
    let get = |i| row.as_ref(i).and_then(value_text).unwrap_or_default();
    let (sql_mode, time_zone, autocommit) = (get(0), get(1), get(2));
    let mut modes: Vec<&str> =
        sql_mode.split(',').filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")).collect();
    if spec.keep_identity && !modes.iter().any(|m| m.eq_ignore_ascii_case("NO_AUTO_VALUE_ON_ZERO")) {
        modes.push("NO_AUTO_VALUE_ON_ZERO");
    }
    // sql_mode first: the literals after it depend on backslash escapes.
    conn.query_drop(format!("SET SESSION sql_mode = '{}'", modes.join(",").replace('\'', "''"))).await.map_err(err)?;
    conn.query_drop("SET time_zone = '+00:00', autocommit = 0").await.map_err(err)?;
    let mut saved = Saved { sql_mode, time_zone, autocommit, locked: false };
    if spec.table_lock && v.is_mysql_server() {
        match conn.query_drop(format!("LOCK TABLES {table} WRITE")).await {
            Ok(()) => saved.locked = true,
            Err(e) => {
                restore_session(conn, saved).await;
                return Err(err(e));
            }
        }
    }
    Ok(saved)
}

async fn restore_session(conn: &mut Conn, saved: Saved) {
    let mut stmts = Vec::new();
    if saved.locked {
        stmts.push("UNLOCK TABLES".to_string());
    }
    stmts.push(format!(
        "SET autocommit = {}, time_zone = {}",
        if saved.autocommit == "0" { 0 } else { 1 },
        lit(&saved.time_zone)
    ));
    stmts.push(format!("SET SESSION sql_mode = {}", lit(&saved.sql_mode)));
    for sql in stmts {
        if let Err(e) = conn.query_drop(&sql).await {
            tracing::debug!("mysql: {sql}: {e}");
        }
    }
}

/// A statement the server accepted with warnings (LOAD DATA LOCAL turns
/// bad values into warnings) fails, notes aside, and warnings about a
/// column not in `loaded`: one the server computes itself (a STORED
/// generated column rounded to its type), as it would for any INSERT.
async fn check_warnings(conn: &mut Conn, loaded: &[String]) -> Result<()> {
    if conn.get_warnings() == 0 {
        return Ok(());
    }
    // No LIMIT: TiDB doesn't take one here (a syntax error).
    let rows: Vec<Row> = conn.query("SHOW WARNINGS").await.map_err(err)?;
    let found: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            let level = r.as_ref(0).and_then(value_text)?;
            let message = r.as_ref(2).and_then(value_text).unwrap_or_else(|| level.clone());
            let computed = warned_column(&message).is_some_and(|c| !loaded.iter().any(|l| l.eq_ignore_ascii_case(&c)));
            (!level.eq_ignore_ascii_case("note") && !computed).then_some(message)
        })
        .take(5)
        .collect();
    if found.is_empty() {
        Ok(())
    } else {
        Err(Error::Query(format!("el servidor rechazó filas de la carga: {}", found.join("; "))))
    }
}

/// The column a warning names (`… for column 'x' at row 1`).
fn warned_column(message: &str) -> Option<String> {
    let at = message.find("column '")? + "column '".len();
    let rest = &message[at..];
    Some(rest[..rest.rfind('\'')?].split("' at row").next()?.to_string())
}

/// `LOAD DATA` answered that local files are off (MySQL's 3948,
/// MariaDB's 1148) or doesn't know the statement (1064, 1235).
fn local_infile_refused(e: &mysql_async::Error) -> bool {
    matches!(e, mysql_async::Error::Server(s) if matches!(s.code, 3948 | 1148 | 1064 | 1235))
}

/// The item type of [`InfileData`] (`bytes::Bytes`), named without a
/// dependency on the `bytes` crate.
trait OkOf {
    type Ok;
}
impl<T, E> OkOf for std::result::Result<T, E> {
    type Ok = T;
}
type Chunk = <<InfileData as futures::Stream>::Item as OkOf>::Ok;

type Tx = mpsc::Sender<io::Result<Chunk>>;

async fn send(tx: &mut Tx, item: io::Result<Chunk>) -> Result<()> {
    tx.send(item).await.map_err(|_| Error::Connect("la conexión dejó de recibir la carga".into()))
}

/// Hands `buf` to the stream once it holds `chunk` bytes.
async fn send_full(tx: &mut Tx, buf: &mut Vec<u8>, chunk: usize, sent: &mut u64) -> Result<()> {
    if buf.len() >= chunk {
        *sent += buf.len() as u64;
        let full = std::mem::replace(buf, Vec::with_capacity(chunk * 2));
        send(tx, Ok(Chunk::from(full))).await?;
    }
    Ok(())
}

/// One row as a `LOAD DATA` line, sent in pieces of about `chunk` bytes: a
/// field over `chunk / 4` bytes is encoded (and sent) a slice at a time,
/// and the buffer is flushed after every field, so no packet grows with
/// the row. A field or slice of at most `chunk / 4` bytes encodes to at
/// most `chunk / 2` (hex or escapes double it), and the buffer is under
/// `chunk` before it: each packet stays under `2 * chunk`.
async fn put_row_chunked(tx: &mut Tx, buf: &mut Vec<u8>, row: &[Cell], modes: &[Mode], chunk: usize, sent: &mut u64) -> Result<()> {
    let piece = (chunk / 4).max(1);
    for (i, c) in row.iter().enumerate() {
        if i > 0 {
            buf.push(b'\t');
        }
        let mode = modes.get(i).copied().unwrap_or(Mode::Plain);
        match big_field(c, mode, piece)? {
            None => {
                put_field(buf, c, mode)?;
                send_full(tx, buf, chunk, sent).await?;
            }
            Some((data, hex)) => {
                for part in data.chunks(piece) {
                    if hex {
                        put_hex(buf, part);
                    } else {
                        put_escaped(buf, part);
                    }
                    send_full(tx, buf, chunk, sent).await?;
                }
            }
        }
    }
    buf.push(b'\n');
    Ok(())
}

/// One window's rows, encoded and handed to the `LOAD DATA` stream once
/// the server asks for the file.
async fn produce(
    started: oneshot::Receiver<()>,
    mut tx: Tx,
    pending: &mut Option<RowBatch>,
    source: &mut dyn BatchSource,
    modes: &[Mode],
    window: &Window,
    chunk: usize,
) -> Result<(u64, bool)> {
    if started.await.is_err() {
        // The statement failed before asking for the data.
        return Ok((0, false));
    }
    let (mut rows, mut bytes, mut ended) = (0u64, 0u64, false);
    let mut buf = Vec::with_capacity(chunk * 2);
    loop {
        let mut batch = match pending.take() {
            Some(b) => b,
            None => match source.next().await {
                Some(b) => b,
                None => {
                    ended = true;
                    break;
                }
            },
        };
        let mut cut = None;
        for (i, row) in batch.rows.iter().enumerate() {
            // A bad value ends the file before its row (whole rows only, the
            // window is rolled back) without breaking the connection.
            check_row(row, modes)?;
            let put = match put_row_chunked(&mut tx, &mut buf, row, modes, chunk, &mut bytes).await {
                Ok(()) => send_full(&mut tx, &mut buf, chunk, &mut bytes).await,
                e => e,
            };
            if let Err(e) = put {
                // Abort the stream: the statement fails and is rolled back.
                let _ = send(&mut tx, Err(io::Error::other(e.to_string()))).await;
                return Err(e);
            }
            rows += 1;
            if window.full(rows, bytes + buf.len() as u64) {
                cut = Some(i + 1);
                break;
            }
        }
        if let Some(i) = cut {
            // The rest of the batch opens the next window.
            if i < batch.rows.len() {
                let rest = batch.rows.split_off(i);
                let bytes = rest.iter().flatten().map(Cell::size).sum();
                *pending = Some(RowBatch { rows: rest, bytes });
            }
            break;
        }
    }
    if !buf.is_empty() {
        send(&mut tx, Ok(Chunk::from(buf))).await?;
    }
    Ok((rows, ended))
}

async fn load_local(
    conn: &mut Conn,
    table: &str,
    spec: &LoadSpec,
    modes: &[Mode],
    window: &Window,
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    let mut targets = Vec::with_capacity(spec.columns.len());
    let mut sets = Vec::new();
    // With a SET clause, MariaDB ignores NO_AUTO_VALUE_ON_ZERO for an
    // AUTO_INCREMENT column read directly (a 0 becomes the next value); set
    // from a variable, it keeps the 0.
    let any_set = modes.iter().any(|m| matches!(m, Mode::Hex | Mode::Bit));
    for (i, (c, m)) in spec.columns.iter().zip(modes).enumerate() {
        let q = quote_ident(Quote::Backtick, c);
        match m {
            Mode::Auto if any_set => {
                targets.push(format!("@c{i}"));
                sets.push(format!("{q} = @c{i}"));
            }
            Mode::Plain | Mode::Auto | Mode::Float => targets.push(q),
            Mode::Hex => {
                targets.push(format!("@c{i}"));
                sets.push(format!("{q} = UNHEX(@c{i})"));
            }
            Mode::Bit => {
                targets.push(format!("@c{i}"));
                sets.push(format!("{q} = CAST(@c{i} AS UNSIGNED)"));
            }
        }
    }
    let mut sql = format!(
        "LOAD DATA LOCAL INFILE 'dbine' INTO TABLE {table} CHARACTER SET utf8mb4 \
         FIELDS TERMINATED BY '\\t' ESCAPED BY '\\\\' LINES TERMINATED BY '\\n' ({})",
        targets.join(", ")
    );
    if !sets.is_empty() {
        sql.push_str(" SET ");
        sql.push_str(&sets.join(", "));
    }

    // Pieces of the file well under the packet limit (see put_row_chunked).
    let chunk = (max_packet(conn).await as usize / 4).clamp(16 * 1024, SEND_CHUNK);
    let mut total = 0u64;
    let mut first = true;
    // Rows left over from the previous window's last batch.
    let mut pending: Option<RowBatch> = None;
    loop {
        if pending.is_none() {
            pending = match source.next().await {
                Some(b) if b.is_empty() => continue,
                Some(b) => Some(b),
                None => return Ok(total),
            };
        }
        let (tx, rx) = mpsc::channel::<io::Result<Chunk>>(4);
        let (started_tx, started_rx) = oneshot::channel::<()>();
        conn.set_infile_handler(async move {
            let _ = started_tx.send(());
            Ok(Box::pin(rx) as InfileData)
        });
        let query = conn.query_drop(sql.as_str());
        let producer = Box::pin(produce(started_rx, tx, &mut pending, source, modes, window, chunk));
        let outcome = match select(query, producer).await {
            Either::Right((produced, query)) => match (query.await, produced) {
                (Err(e), _) => Err(e),
                (Ok(()), Err(e)) => return Err(e),
                (Ok(()), Ok(p)) => Ok(p),
            },
            // A refusal comes before the server asks for the data, so the
            // producer (waiting for that request) never took a batch.
            Either::Left((Err(e), _)) => Err(e),
            Either::Left((Ok(()), _)) => return Err(Error::State("el servidor no pidió los datos de LOAD DATA".into())),
        };
        let (sent, ended) = match outcome {
            Ok(p) => p,
            Err(e) if first && pending.is_some() && local_infile_refused(&e) => {
                tracing::info!("mysql: LOAD DATA LOCAL refused ({e}); loading with prepared INSERTs");
                // No stale handler left behind for later statements.
                conn.set_infile_handler(async { Err(mysql_async::LocalInfileError::NoHandler.into()) });
                return load_prepared(conn, table, spec, modes, window, pending, source, progress, total).await;
            }
            Err(e) => return Err(err(e)),
        };
        first = false;
        // Before SHOW WARNINGS, which resets it.
        let affected = conn.affected_rows();
        check_warnings(conn, &spec.columns).await?;
        if affected != sent {
            return Err(Error::Query(format!("el servidor cargó {affected} de {sent} filas (claves duplicadas o valores inválidos)")));
        }
        conn.query_drop("COMMIT").await.map_err(err)?;
        total += sent;
        progress(total);
        if ended {
            return Ok(total);
        }
    }
}

/// The server's `max_allowed_packet` (4 MiB, MySQL 5.7's default, when it
/// can't be read).
async fn max_packet(conn: &mut Conn) -> u64 {
    conn.query_first::<Row, _>("SELECT @@max_allowed_packet")
        .await
        .ok()
        .flatten()
        .and_then(|r| r.as_ref(0).and_then(value_text))
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024 * 1024)
}

/// Multi-row prepared `INSERT`s: the fallback without `LOAD DATA LOCAL`,
/// and TiDB's load.
#[allow(clippy::too_many_arguments)]
async fn load_prepared(
    conn: &mut Conn,
    table: &str,
    spec: &LoadSpec,
    modes: &[Mode],
    window: &Window,
    mut pending: Option<RowBatch>,
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
    mut total: u64,
) -> Result<u64> {
    let ncols = spec.columns.len().max(1);
    let max_packet = max_packet(conn).await;
    let budget = (max_packet as usize / 2).clamp(64 * 1024, PREPARED_STATEMENT);
    let head = format!(
        "INSERT INTO {table} ({}) VALUES ",
        spec.columns.iter().map(|c| quote_ident(Quote::Backtick, c)).collect::<Vec<_>>().join(", ")
    );
    let one = format!("({})", vec!["?"; ncols].join(", "));
    let statement = |n: usize| {
        let mut s = String::with_capacity(head.len() + n * (one.len() + 2));
        s.push_str(&head);
        for i in 0..n {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&one);
        }
        s
    };
    let max_rows = (65_535 / ncols).max(1);
    let mut per_stmt = 0usize;
    let mut full_sql = String::new();
    let (mut params, mut in_stmt, mut stmt_bytes) = (Vec::<Value>::new(), 0usize, 0usize);
    let (mut win_rows, mut win_bytes) = (0u64, 0u64);

    async fn flush(conn: &mut Conn, sql: &str, params: &mut Vec<Value>, loaded: &[String]) -> Result<()> {
        conn.exec_drop(sql, Params::Positional(std::mem::take(params))).await.map_err(err)?;
        check_warnings(conn, loaded).await
    }

    loop {
        let batch = match pending.take() {
            Some(b) => Some(b),
            None => source.next().await,
        };
        let Some(batch) = batch else { break };
        if per_stmt == 0 && !batch.is_empty() {
            let avg = (batch.bytes / batch.len()).max(1);
            per_stmt = (budget / avg).clamp(1, max_rows);
            full_sql = statement(per_stmt);
        }
        for row in &batch.rows {
            let size: usize = row.iter().map(Cell::size).sum();
            if in_stmt > 0 && stmt_bytes + size > budget {
                flush(conn, &statement(in_stmt), &mut params, &spec.columns).await?;
                (in_stmt, stmt_bytes) = (0, 0);
            }
            for (i, c) in row.iter().enumerate() {
                params.push(param(c, modes.get(i).copied().unwrap_or(Mode::Plain))?);
            }
            in_stmt += 1;
            stmt_bytes += size;
            win_rows += 1;
            win_bytes += size as u64;
            if in_stmt == per_stmt {
                flush(conn, &full_sql, &mut params, &spec.columns).await?;
                (in_stmt, stmt_bytes) = (0, 0);
            }
            if window.full(win_rows, win_bytes) {
                if in_stmt > 0 {
                    flush(conn, &statement(in_stmt), &mut params, &spec.columns).await?;
                    (in_stmt, stmt_bytes) = (0, 0);
                }
                conn.query_drop("COMMIT").await.map_err(err)?;
                total += win_rows;
                progress(total);
                (win_rows, win_bytes) = (0, 0);
            }
        }
    }
    if in_stmt > 0 {
        flush(conn, &statement(in_stmt), &mut params, &spec.columns).await?;
    }
    if win_rows > 0 {
        conn.query_drop("COMMIT").await.map_err(err)?;
        total += win_rows;
        progress(total);
    }
    Ok(total)
}

/// StarRocks, Doris, Databend, GreptimeDB: text `INSERT`s, each one a
/// load that commits on its own.
async fn load_text(
    conn: &mut Conn,
    table: &str,
    spec: &LoadSpec,
    window: &Window,
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    let head = format!(
        "INSERT INTO {table} ({}) VALUES ",
        spec.columns.iter().map(|c| quote_ident(Quote::Backtick, c)).collect::<Vec<_>>().join(", ")
    );
    let limit = (window.bytes as usize).clamp(64 * 1024, TEXT_STATEMENT);
    let (mut sql, mut in_stmt, mut total) = (String::with_capacity(limit + 64 * 1024), 0u64, 0u64);
    while let Some(batch) = source.next().await {
        for row in &batch.rows {
            sql.push_str(if in_stmt == 0 { &head } else { ", " });
            sql.push('(');
            for (i, c) in row.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&literal(c)?);
            }
            sql.push(')');
            in_stmt += 1;
            if sql.len() >= limit || in_stmt >= window.rows.min(TEXT_ROWS) {
                conn.query_drop(sql.as_str()).await.map_err(err)?;
                total += in_stmt;
                progress(total);
                sql.clear();
                in_stmt = 0;
            }
        }
    }
    if in_stmt > 0 {
        conn.query_drop(sql.as_str()).await.map_err(err)?;
        total += in_stmt;
        progress(total);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_column_a_warning_names() {
        assert_eq!(warned_column("Incorrect decimal value: '12.7050' for column 'total_iva'").as_deref(), Some("total_iva"));
        assert_eq!(warned_column("Data truncated for column 'total_iva' at row 1").as_deref(), Some("total_iva"));
        assert_eq!(warned_column("Out of range value for column 'a'b' at row 3").as_deref(), Some("a'b"));
        assert_eq!(warned_column("Duplicate entry '1' for key 'PRIMARY'"), None);
    }

    /// A field of a `LOAD DATA` line read back as the server does
    /// (`None` for `\N`).
    fn parse_field(f: &[u8]) -> Option<Vec<u8>> {
        if f == b"\\N" {
            return None;
        }
        let mut out = Vec::new();
        let mut it = f.iter();
        while let Some(&c) = it.next() {
            if c != b'\\' {
                out.push(c);
                continue;
            }
            out.push(match it.next() {
                Some(b'0') => 0,
                Some(b'b') => 8,
                Some(b'n') => b'\n',
                Some(b'r') => b'\r',
                Some(b't') => b'\t',
                Some(b'Z') => 26,
                Some(&x) => x,
                None => b'\\',
            });
        }
        Some(out)
    }

    /// Lines and fields split at unescaped tabs and newlines.
    fn parse(data: &[u8]) -> Vec<Vec<Option<Vec<u8>>>> {
        let (mut rows, mut row, mut field, mut escaped) = (Vec::new(), Vec::new(), Vec::new(), false);
        for &c in data {
            if escaped {
                field.push(c);
                escaped = false;
            } else if c == b'\\' {
                field.push(c);
                escaped = true;
            } else if c == b'\t' {
                row.push(parse_field(&std::mem::take(&mut field)));
            } else if c == b'\n' {
                row.push(parse_field(&std::mem::take(&mut field)));
                rows.push(std::mem::take(&mut row));
            } else {
                field.push(c);
            }
        }
        rows
    }

    fn unhex(s: &[u8]) -> Vec<u8> {
        s.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect()
    }

    #[test]
    fn transfer_tsv_round_trips_text_nulls_and_unicode() {
        let texts = ["", "plain", "tab\there", "line\nbreak\r\n", "back\\slash", "\\N", "nul\0byte", "ñandú 🦀 日本", "\u{1A}"];
        let mut out = Vec::new();
        for t in texts {
            put_row(&mut out, &[Cell::Text(t.into()), Cell::Null, Cell::Int(-5)], &[Mode::Plain; 3]).unwrap();
        }
        let rows = parse(&out);
        assert_eq!(rows.len(), texts.len());
        for (row, t) in rows.iter().zip(texts) {
            assert_eq!(row[0].as_deref(), Some(t.as_bytes()), "{t:?}");
            assert_eq!(row[1], None);
            assert_eq!(row[2].as_deref(), Some(&b"-5"[..]));
        }
    }

    #[test]
    fn transfer_tsv_round_trips_binaries() {
        let all: Vec<u8> = (0..=255u8).chain((0..=255u8).rev()).collect();
        let text: Vec<u8> = (0..=127u8).chain("ñ🦀".bytes()).collect();
        let mut out = Vec::new();
        put_row(&mut out, &[Cell::Bytes(all.clone()), Cell::Bytes(text.clone()), Cell::Bytes(vec![])], &[Mode::Hex, Mode::Plain, Mode::Hex])
            .unwrap();
        let rows = parse(&out);
        assert_eq!(rows.len(), 1);
        assert_eq!(unhex(rows[0][0].as_ref().unwrap()), all);
        assert_eq!(rows[0][1].as_ref().unwrap(), &text, "raw escaped bytes");
        assert_eq!(rows[0][2].as_deref(), Some(&b""[..]), "empty is not NULL");
    }

    /// Bytes that aren't UTF-8 never reach a text column: MySQL 8 drops
    /// them from a prepared statement's parameter without a warning.
    #[test]
    fn transfer_non_utf8_bytes_into_text_fail() {
        for bad in [vec![0xFF, 0xFE], vec![0x61, 0xFF, 0x62], vec![0xC3]] {
            for mode in [Mode::Plain, Mode::Auto, Mode::Float] {
                assert!(param(&Cell::Bytes(bad.clone()), mode).is_err(), "{bad:?} {mode:?}");
                assert!(put_field(&mut Vec::new(), &Cell::Bytes(bad.clone()), mode).is_err(), "{bad:?} {mode:?}");
            }
            assert_eq!(param(&Cell::Bytes(bad.clone()), Mode::Hex).unwrap(), Value::Bytes(bad.clone()));
            assert!(put_field(&mut Vec::new(), &Cell::Bytes(bad.clone()), Mode::Hex).is_ok());
        }
        assert_eq!(param(&Cell::Bytes("ñ".into()), Mode::Plain).unwrap(), Value::Bytes("ñ".into()));
        let big = [vec![b'a'; 100], vec![0xFF]].concat();
        assert!(big_field(&Cell::Bytes(big.clone()), Mode::Plain, 10).is_err());
        assert!(big_field(&Cell::Bytes(big), Mode::Hex, 10).unwrap().is_some());
    }

    /// FLT_MAX reads as the f32's exact value (its shortest text,
    /// 3.4028235e38, is out of a FLOAT's range as a double), and a FLOAT
    /// column gets a double within range.
    #[test]
    fn transfer_float_extremes_stay_in_range() {
        let max = f64::from(f32::MAX);
        assert_eq!(to_cell(Kind::Float, Value::Float(f32::MAX)), Cell::Float(max));
        assert_eq!(to_cell(Kind::Float, Value::Float(f32::MIN)), Cell::Float(-max));
        assert_eq!(to_cell(Kind::Float, Value::Float(0.1)), Cell::Float(0.1));
        assert_eq!(to_cell(Kind::Float, Value::Float(f32::MIN_POSITIVE)), Cell::Float(1.1754944e-38));
        assert_eq!(to_cell(Kind::Float, Value::Bytes(b"3.4028235e38".to_vec())), Cell::Float(max));
        // A double column keeps any double.
        assert_eq!(to_cell(Kind::Double, Value::Bytes(b"3.4028235e38".to_vec())), Cell::Float(3.4028235e38));
        for f in [3.4028235e38, -3.4028235e38] {
            let mut out = Vec::new();
            put_field(&mut out, &Cell::Float(f), Mode::Float).unwrap();
            let back: f64 = std::str::from_utf8(&out).unwrap().parse().unwrap();
            assert!(back.abs() <= max && back as f32 == f as f32, "{back}");
            assert_eq!(param(&Cell::Float(f), Mode::Float).unwrap(), Value::Double(f.signum() * max));
        }
        assert_eq!(param(&Cell::Float(3.4028235e38), Mode::Plain).unwrap(), Value::Double(3.4028235e38));
        assert_eq!(mode_of("FLOAT"), Mode::Float);
    }

    /// A row with a field far over the packet limit goes in pieces no
    /// bigger than twice the chunk, and reads back whole.
    #[test]
    fn transfer_big_fields_are_sent_in_bounded_pieces() {
        use futures::StreamExt;
        let chunk = 64 * 1024;
        let blob: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let text = "ñ\t\\x".repeat(200_000);
        let row = vec![Cell::Int(1), Cell::Bytes(blob.clone()), Cell::Text(text.clone()), Cell::Null];
        let modes = [Mode::Plain, Mode::Hex, Mode::Plain, Mode::Plain];
        let (mut tx, rx) = mpsc::channel::<io::Result<Chunk>>(4);
        let (pieces, sent) = futures::executor::block_on(async {
            let producer = async {
                let (mut buf, mut sent) = (Vec::new(), 0u64);
                put_row_chunked(&mut tx, &mut buf, &row, &modes, chunk, &mut sent).await.unwrap();
                sent += buf.len() as u64;
                send(&mut tx, Ok(Chunk::from(buf))).await.unwrap();
                drop(tx);
                sent
            };
            let collect = rx.map(|c| c.unwrap().to_vec()).collect::<Vec<_>>();
            let (sent, pieces) = futures::join!(producer, collect);
            (pieces, sent)
        });
        assert!(pieces.len() > 10);
        assert!(pieces.iter().all(|p| p.len() < 2 * chunk), "{:?}", pieces.iter().map(Vec::len).max());
        let all = pieces.concat();
        assert_eq!(all.len() as u64, sent);
        let rows = parse(&all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some(&b"1"[..]));
        assert_eq!(unhex(rows[0][1].as_ref().unwrap()), blob);
        assert_eq!(rows[0][2].as_deref(), Some(text.as_bytes()));
        assert_eq!(rows[0][3], None);
    }

    /// A row of many fields just under the slicing limit (each sent whole)
    /// is still flushed field by field: no packet reaches twice the chunk.
    #[test]
    fn transfer_many_medium_fields_are_sent_in_bounded_pieces() {
        use futures::StreamExt;
        let chunk = 64 * 1024;
        let piece = chunk / 4;
        let blob: Vec<u8> = (0..piece as u32).map(|i| (i % 251) as u8).collect();
        let text = "\\\t".repeat(piece / 2);
        let mut row = vec![Cell::Int(1)];
        let mut modes = vec![Mode::Plain];
        for i in 0..33 {
            if i % 2 == 0 {
                row.push(Cell::Bytes(blob.clone()));
                modes.push(Mode::Hex);
            } else {
                row.push(Cell::Text(text.clone()));
                modes.push(Mode::Plain);
            }
        }
        let (mut tx, rx) = mpsc::channel::<io::Result<Chunk>>(4);
        let (pieces, sent) = futures::executor::block_on(async {
            let producer = async {
                let (mut buf, mut sent) = (Vec::new(), 0u64);
                put_row_chunked(&mut tx, &mut buf, &row, &modes, chunk, &mut sent).await.unwrap();
                sent += buf.len() as u64;
                send(&mut tx, Ok(Chunk::from(buf))).await.unwrap();
                drop(tx);
                sent
            };
            let collect = rx.map(|c| c.unwrap().to_vec()).collect::<Vec<_>>();
            let (sent, pieces) = futures::join!(producer, collect);
            (pieces, sent)
        });
        assert!(pieces.len() > 10);
        assert!(pieces.iter().all(|p| p.len() < 2 * chunk), "{:?}", pieces.iter().map(Vec::len).max());
        let all = pieces.concat();
        assert_eq!(all.len() as u64, sent);
        let rows = parse(&all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 34);
        for (i, f) in rows[0][1..].iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(unhex(f.as_ref().unwrap()), blob);
            } else {
                assert_eq!(f.as_deref(), Some(text.as_bytes()));
            }
        }
    }

    #[test]
    fn transfer_tsv_numbers_bits_and_uuids() {
        let mut out = Vec::new();
        let row = [
            Cell::UInt(u64::MAX),
            Cell::Float(0.1),
            Cell::Float(1e300),
            Cell::Bool(true),
            Cell::Bytes(vec![1, 2]),
            Cell::Uuid("0f8fad5b-d9cb-469f-a165-70867728950e".into()),
            Cell::Decimal("-12345678901234567890.123456789".into()),
            Cell::DateTimeTz("2024-01-01 01:30:00.5+03:00".into()),
        ];
        let modes = [Mode::Plain, Mode::Plain, Mode::Plain, Mode::Plain, Mode::Bit, Mode::Hex, Mode::Plain, Mode::Plain];
        put_row(&mut out, &row, &modes).unwrap();
        let r = &parse(&out)[0];
        let s = |i: usize| String::from_utf8(r[i].clone().unwrap()).unwrap();
        assert_eq!(s(0), "18446744073709551615");
        assert_eq!(s(1).parse::<f64>().unwrap(), 0.1);
        assert_eq!(s(2).parse::<f64>().unwrap(), 1e300);
        assert_eq!(s(3), "1");
        assert_eq!(s(4), "258");
        assert_eq!(s(5), "0F8FAD5BD9CB469FA16570867728950E");
        assert_eq!(s(6), "-12345678901234567890.123456789");
        assert_eq!(s(7), "2023-12-31 22:30:00.5");
        assert!(put_row(&mut Vec::new(), &[Cell::Float(f64::NAN)], &[Mode::Plain]).is_err());
    }

    #[test]
    fn transfer_utc_conversion() {
        assert_eq!(to_utc("2024-03-01 00:10:00-01:00").as_deref(), Some("2024-03-01 01:10:00"));
        assert_eq!(to_utc("2024-03-01 00:10:00+01:00").as_deref(), Some("2024-02-29 23:10:00"));
        assert_eq!(to_utc("1999-12-31T23:59:59.123456Z").as_deref(), Some("1999-12-31 23:59:59.123456"));
        assert_eq!(to_utc("2000-01-01 00:00:00+0530").as_deref(), Some("1999-12-31 18:30:00"));
        assert_eq!(to_utc("nonsense"), None);
        for d in [-800_000, -1, 0, 59, 60, 10_957, 2_932_896] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn transfer_cells_from_values() {
        assert_eq!(to_cell(Kind::UInt, Value::UInt(u64::MAX)), Cell::UInt(u64::MAX));
        assert_eq!(to_cell(Kind::Int, Value::UInt(7)), Cell::Int(7));
        assert_eq!(to_cell(Kind::Float, Value::Float(0.1)), Cell::Float(0.1));
        assert_eq!(to_cell(Kind::Decimal, Value::Bytes(b"-1.50".to_vec())), Cell::Decimal("-1.50".into()));
        assert_eq!(to_cell(Kind::Date, Value::Date(2024, 2, 29, 0, 0, 0, 0)), Cell::Date("2024-02-29".into()));
        assert_eq!(
            to_cell(Kind::Timestamp, Value::Date(2024, 2, 29, 1, 2, 3, 4)),
            Cell::DateTimeTz("2024-02-29 01:02:03.000004+00:00".into())
        );
        assert_eq!(to_cell(Kind::Time, Value::Time(false, 0, 23, 59, 59, 0)), Cell::Time("23:59:59".into()));
        assert_eq!(to_cell(Kind::Time, Value::Time(true, 0, 1, 0, 0, 0)), Cell::Text("-01:00:00".into()));
        assert_eq!(to_cell(Kind::Time, Value::Time(false, 34, 22, 59, 59, 0)), Cell::Text("838:59:59".into()));
        assert_eq!(to_cell(Kind::Time, Value::Bytes(b"-838:59:59".to_vec())), Cell::Text("-838:59:59".into()));
        assert_eq!(to_cell(Kind::Bit, Value::Bytes(vec![1, 0])), Cell::UInt(256));
        assert_eq!(to_cell(Kind::Json, Value::Bytes(b"{\"a\":1}".to_vec())), Cell::Json("{\"a\":1}".into()));
        assert_eq!(to_cell(Kind::Bytes, Value::Bytes(vec![0xff])), Cell::Bytes(vec![0xff]));
        assert_eq!(to_cell(Kind::Text, Value::Bytes(vec![0xff])), Cell::Bytes(vec![0xff]));
        // Text protocol: numbers parsed by the column's type.
        assert_eq!(to_cell(Kind::Int, Value::Bytes(b"-9".to_vec())), Cell::Int(-9));
        assert_eq!(to_cell(Kind::Int, Value::Bytes(b"170141183460469231731687303715884105727".to_vec())).clone(),
            Cell::Decimal("170141183460469231731687303715884105727".into()));
        assert_eq!(to_cell(Kind::Double, Value::Bytes(b"2.5".to_vec())), Cell::Float(2.5));
        assert_eq!(to_cell(Kind::Timestamp, Value::Bytes(b"2024-01-01 00:00:00".to_vec())), Cell::DateTimeTz("2024-01-01 00:00:00+00:00".into()));
    }

    #[test]
    fn transfer_literals() {
        assert_eq!(literal(&Cell::Text("it's a \\ test".into())).unwrap(), "'it''s a \\\\ test'");
        assert_eq!(literal(&Cell::Bytes(vec![0, 0xab])).unwrap(), "X'00AB'");
        assert_eq!(literal(&Cell::Null).unwrap(), "NULL");
        assert_eq!(literal(&Cell::Bool(false)).unwrap(), "FALSE");
        assert_eq!(mode_of("LONGBLOB"), Mode::Hex);
        assert_eq!(mode_of("bit"), Mode::Bit);
        assert_eq!(mode_of("varchar"), Mode::Plain);
    }
}

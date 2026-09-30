//! Bulk transfer (see `dbine_driver::transfer`): typed reads, `COPY` loads
//! and the native copy between two sessions of this crate.
//!
//! - **Read** ([`read_batches`]): `COPY (SELECT …) TO STDOUT (FORMAT binary)`
//!   decoded into cells. Types without a decoder here (arrays, intervals,
//!   enums, `timetz`, `xml`…) are cast to `text` in the SELECT; `money` is
//!   cast to `numeric` so it stays exact. Variants that refuse binary `COPY
//!   TO` (CockroachDB) read the same SELECT through the extended protocol,
//!   whose rows come in the same binary encoding. Variants whose extended
//!   protocol or catalog isn't PostgreSQL's (Redshift, Denodo, H2, CrateDB,
//!   the streaming engines, Yellowbrick) read over the simple protocol:
//!   every value as the server's text.
//! - **Load** ([`bulk_load`]): `COPY t (cols) FROM STDIN (FORMAT binary)`,
//!   encoding each cell for the target column's type (read once from the
//!   prepared SELECT). If a target column has a type without an encoder
//!   here, or the variant has no binary `COPY FROM` (Greenplum and its
//!   forks), the load uses `COPY`'s text format instead, which every type
//!   parses. Each commit window is a `COPY` of its own (autocommit), so a
//!   finished window is committed. A target error (a constraint, a full
//!   disk) only surfaces when its window finishes: with `commit_rows` and
//!   `commit_bytes` at 0 the whole table is one window, and a failure shows
//!   up only after all of it was sent. `table_lock` is ignored: a lock gives
//!   PostgreSQL's `COPY` no faster path (no minimal logging like SQL
//!   Server's `TABLOCK`) and an `ACCESS EXCLUSIVE` lock would only block the
//!   target's readers. `keep_identity` needs nothing: `COPY` writes the
//!   given values, identity columns included, and the migration resyncs the
//!   sequences afterwards (`Driver::data_load_wrap`).
//! - **Native copy** ([`copy_native`]): the source's binary `COPY TO` bytes
//!   go untouched into the target's binary `COPY FROM`, in chunks of up to
//!   2 MiB. The source is one `COPY`; the target is cut into commit
//!   windows of `commit_rows` at tuple boundaries (found from the tuple
//!   headers, values never decoded), each a `COPY` of its own that closes
//!   with the trailer and commits. tokio-postgres only sees a target error
//!   when a `COPY` finishes, so the windows are also what surfaces it
//!   early instead of after streaming the whole table. Progress reports the
//!   committed rows. Only when every column has the same built-in type on
//!   both sides and none is bound to its server (`money`, `oid`, `reg*`);
//!   otherwise it answers `Unsupported` and the migration uses read + load.

use crate::session::PgSession;
use crate::{err, Variant};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result, Session};
use futures::{pin_mut, SinkExt, StreamExt};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Cursor;
use std::ops::Range;
use std::pin::Pin;
use tokio_postgres::types::{FromSql, Type};
use tokio_postgres::{CopyInSink, SimpleQueryMessage};

/// Bytes handed to the server per `COPY` data message.
const SEND_CHUNK: usize = 2 * 1024 * 1024;
/// The binary `COPY` signature, flags and an empty header extension.
const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";
const MONEY: u32 = 790;
/// Type oids below this are built into the server (same on every database).
const FIRST_USER_OID: u32 = 16384;
/// Microseconds in a day.
const DAY_US: i64 = 86_400_000_000;
/// Days from 1970-01-01 to 2000-01-01 (PostgreSQL's epoch).
const PG_EPOCH_DAYS: i64 = 10_957;

/// Variants whose `COPY` takes the binary format both ways.
fn binary_copy(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres
            | Variant::Timescale
            | Variant::Yugabyte
            | Variant::Kingbase
            | Variant::AlloyDb
            | Variant::CloudSql
            | Variant::Aurora
            | Variant::Edb
            | Variant::Fujitsu
            | Variant::OpenGauss
    )
}

/// Variants that bulk load with `COPY … FROM STDIN`: binary, or text for
/// Greenplum and its forks (their `COPY` has no binary format). CockroachDB
/// isn't one: it only takes `COPY FROM` over the simple protocol
/// ("CopyFrom not supported in extended protocol mode"), and tokio-postgres
/// sends it over the extended one.
pub(crate) fn bulk_capable(v: Variant) -> bool {
    binary_copy(v) || v.mpp()
}

/// [`copy_native`] between these two variants.
pub(crate) fn native_capable(source: Variant, target: Variant) -> bool {
    binary_copy(source) && binary_copy(target)
}

/// Variants read over the simple protocol (text values).
fn text_read(v: Variant) -> bool {
    !v.has_pg_catalog()
        || matches!(v, Variant::CrateDb | Variant::RisingWave | Variant::Materialize | Variant::Yellowbrick)
}

/// Column types with a binary codec here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Uuid,
    Date,
    Time,
    Timestamp,
    Timestamptz,
    Json,
    Jsonb,
}

impl Kind {
    pub(crate) fn of(oid: u32) -> Option<Kind> {
        Some(match oid {
            16 => Kind::Bool,
            17 => Kind::Bytea,
            19 | 25 | 1042 | 1043 => Kind::Text,
            20 => Kind::Int8,
            21 => Kind::Int2,
            23 => Kind::Int4,
            114 => Kind::Json,
            700 => Kind::Float4,
            701 => Kind::Float8,
            1082 => Kind::Date,
            1083 => Kind::Time,
            1114 => Kind::Timestamp,
            1184 => Kind::Timestamptz,
            1700 => Kind::Numeric,
            2950 => Kind::Uuid,
            3802 => Kind::Jsonb,
            _ => return None,
        })
    }
}

fn table_name(t: &ObjectRef) -> String {
    qualified_name(Quote::Double, t.schema(), &t.name)
}

fn column_list(cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ")
}

fn select_sql(table: &str, cols: &str, filter: Option<&str>) -> String {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("SELECT {cols} FROM {table} WHERE ({f})"),
        None => format!("SELECT {cols} FROM {table}"),
    }
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

// ---------------------------------------------------------------- reading

/// Column names and types of the read: the spec's columns, or all of them.
async fn probe(s: &PgSession, table: &str, columns: Option<&[String]>) -> Result<Vec<(String, Type)>> {
    let cols = columns.map_or_else(|| "*".to_string(), column_list);
    let stmt = s.client.prepare(&format!("SELECT {cols} FROM {table}")).await.map_err(err)?;
    Ok(stmt.columns().iter().map(|c| (c.name().to_string(), c.type_().clone())).collect())
}

/// The catalog's spelling of each column's type and its nullability.
async fn describe(s: &PgSession, table: &str, cols: &[(String, Type)]) -> Vec<TransferColumn> {
    let sql = "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull
               FROM pg_attribute a WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped";
    let catalog: Vec<(String, String, bool)> = match s.client.query(sql, &[&table]).await {
        Ok(rows) => rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect(),
        Err(e) => {
            tracing::debug!("{:?}: column types unavailable: {e}", s.variant);
            Vec::new()
        }
    };
    cols.iter()
        .map(|(name, ty)| match catalog.iter().find(|c| &c.0 == name) {
            Some((_, t, n)) => TransferColumn { name: name.clone(), type_name: t.clone(), nullable: *n },
            None => TransferColumn { name: name.clone(), type_name: ty.name().to_string(), nullable: true },
        })
        .collect()
}

/// Makes the session's text output parse the same on any target: ISO
/// dates (a `DMY` DateStyle would swap day and month under `MDY`), the
/// `postgres` interval style and floats with every digit (before
/// PostgreSQL 12, and on Redshift and Greenplum, `extra_float_digits`
/// defaults to 0: 15 digits, bits lost). It matters for every value read
/// as text: the `::text` casts (arrays, ranges, composites) and the
/// variants read over the simple protocol. Engines that reject a setting
/// keep their own (errors ignored; the transfer session is autocommit).
async fn portable_text_output(s: &PgSession) {
    for set in ["SET DateStyle = ISO", "SET IntervalStyle = postgres"] {
        if let Err(e) = s.client.simple_query(set).await {
            tracing::debug!("{:?}: {set}: {e}", s.variant);
        }
    }
    // 3 is the maximum since PostgreSQL 12; older servers and Redshift stop at 2.
    if s.client.simple_query("SET extra_float_digits = 3").await.is_err() {
        if let Err(e) = s.client.simple_query("SET extra_float_digits = 2").await {
            tracing::debug!("{:?}: extra_float_digits: {e}", s.variant);
        }
    }
}

pub(crate) async fn read_batches(s: &mut PgSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let table = table_name(&spec.table);
    portable_text_output(s).await;
    if text_read(s.variant) {
        let cols = spec.columns.as_deref().map_or_else(|| "*".to_string(), column_list);
        return read_text(s, &select_sql(&table, &cols, spec.filter.as_deref()), sink).await;
    }
    let cols = probe(s, &table, spec.columns.as_deref()).await?;
    let described = describe(s, &table, &cols).await;
    // Each column's expression and how its values decode.
    let (exprs, kinds): (Vec<String>, Vec<Kind>) = cols
        .iter()
        .map(|(name, ty)| {
            let q = quote_ident(Quote::Double, name);
            match Kind::of(ty.oid()) {
                Some(k) => (q, k),
                None if ty.oid() == MONEY => (format!("{q}::numeric"), Kind::Numeric),
                None => (format!("{q}::text"), Kind::Text),
            }
        })
        .unzip();
    let select = select_sql(&table, &exprs.join(", "), spec.filter.as_deref());
    sink.lock().map_err(lock_err)?.begin(&described)?;

    if binary_copy(s.variant) || s.variant.mpp() {
        let copy = format!("COPY ({select}) TO STDOUT (FORMAT binary)");
        match s.client.copy_out(&copy).await {
            Ok(stream) => return read_copy(stream, &described, &kinds, &sink).await,
            Err(e) => tracing::debug!("{:?}: binary COPY TO refused, reading rows: {e}", s.variant),
        }
    }
    read_rows(s, &select, &described, &kinds, &sink).await
}

async fn read_copy(
    stream: tokio_postgres::CopyOutStream,
    cols: &[TransferColumn],
    kinds: &[Kind],
    sink: &BatchSinkRef,
) -> Result<u64> {
    pin_mut!(stream);
    let mut parser = CopyParser::new(kinds.len());
    let mut builder = BatchBuilder::new();
    let mut fields = Vec::with_capacity(kinds.len());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(err)?;
        parser.feed(&chunk);
        let mut guard = None;
        while parser.next_tuple(&mut fields).map_err(Error::Query)? {
            let row = decode_row(&parser.buf, &fields, kinds, cols)?;
            if guard.is_none() {
                guard = Some(sink.lock().map_err(lock_err)?);
            }
            builder.push(row, &mut **guard.as_mut().expect("locked"))?;
        }
    }
    if !parser.done {
        return Err(Error::Query("el COPY terminó sin su marca de fin".into()));
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

fn decode_row(buf: &[u8], fields: &[Option<Range<usize>>], kinds: &[Kind], cols: &[TransferColumn]) -> Result<Vec<Cell>> {
    fields
        .iter()
        .zip(kinds)
        .enumerate()
        .map(|(i, (f, k))| match f {
            None => Ok(Cell::Null),
            Some(r) => decode(*k, &buf[r.clone()]).map_err(|e| Error::Query(format!("columna «{}»: {e}", cols[i].name))),
        })
        .collect()
}

/// A column's value as the server sent it (binary).
struct Raw<'a>(&'a [u8]);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

/// The read through the extended protocol (binary results, same decoders).
async fn read_rows(s: &PgSession, select: &str, cols: &[TransferColumn], kinds: &[Kind], sink: &BatchSinkRef) -> Result<u64> {
    let stmt = s.client.prepare(select).await.map_err(err)?;
    let rows = s.client.query_raw(&stmt, std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>()).await.map_err(err)?;
    pin_mut!(rows);
    let mut builder = BatchBuilder::new();
    while let Some(row) = rows.next().await {
        let row = row.map_err(err)?;
        let mut cells = Vec::with_capacity(kinds.len());
        for (i, k) in kinds.iter().enumerate() {
            cells.push(match row.try_get::<_, Option<Raw>>(i).map_err(err)? {
                None => Cell::Null,
                Some(Raw(b)) => decode(*k, b).map_err(|e| Error::Query(format!("columna «{}»: {e}", cols[i].name)))?,
            });
        }
        builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

/// The read over the simple protocol: every value is the server's text.
async fn read_text(s: &PgSession, select: &str, sink: BatchSinkRef) -> Result<u64> {
    let stream = s.client.simple_query_raw(select).await.map_err(err)?;
    pin_mut!(stream);
    let mut builder = BatchBuilder::new();
    let mut begun = false;
    while let Some(msg) = stream.next().await {
        match msg.map_err(err)? {
            SimpleQueryMessage::RowDescription(cols) if !begun => {
                begun = true;
                let cols: Vec<TransferColumn> = cols
                    .iter()
                    .map(|c| TransferColumn { name: c.name().to_string(), type_name: String::new(), nullable: true })
                    .collect();
                sink.lock().map_err(lock_err)?.begin(&cols)?;
            }
            SimpleQueryMessage::Row(r) => {
                let cells = (0..r.len()).map(|i| r.get(i).map_or(Cell::Null, |t| Cell::Text(t.to_string()))).collect();
                builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
            }
            _ => {}
        }
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

/// Splits a binary `COPY` stream into tuples, whatever the chunking.
pub(crate) struct CopyParser {
    pub(crate) buf: Vec<u8>,
    pos: usize,
    columns: usize,
    header: bool,
    pub(crate) done: bool,
}

impl CopyParser {
    pub(crate) fn new(columns: usize) -> Self {
        CopyParser { buf: Vec::new(), pos: 0, columns, header: false, done: false }
    }

    /// Bytes of `buf` taken by the header and the tuples read so far.
    pub(crate) fn consumed(&self) -> usize {
        self.pos
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete tuple's fields (ranges into `buf`); `false` when
    /// more bytes are needed or the trailer was read.
    pub(crate) fn next_tuple(&mut self, fields: &mut Vec<Option<Range<usize>>>) -> std::result::Result<bool, String> {
        let b = &self.buf;
        if !self.header {
            if b.len() < 19 {
                return Ok(false);
            }
            if b[..11] != HEADER[..11] {
                return Err("el COPY binario no empieza con su firma".into());
            }
            let ext = i32::from_be_bytes(b[15..19].try_into().unwrap()) as usize;
            if b.len() < 19 + ext {
                return Ok(false);
            }
            self.header = true;
            self.pos = 19 + ext;
        }
        if self.done {
            return Ok(false);
        }
        let b = &self.buf;
        let mut p = self.pos;
        if b.len() < p + 2 {
            return Ok(false);
        }
        let n = i16::from_be_bytes([b[p], b[p + 1]]);
        p += 2;
        if n == -1 {
            self.done = true;
            self.pos = p;
            return Ok(false);
        }
        if n as usize != self.columns {
            return Err(format!("el COPY trajo {n} columnas y se esperaban {}", self.columns));
        }
        fields.clear();
        for _ in 0..n {
            if b.len() < p + 4 {
                return Ok(false);
            }
            let len = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
            p += 4;
            if len < 0 {
                fields.push(None);
                continue;
            }
            let len = len as usize;
            if b.len() < p + len {
                return Ok(false);
            }
            fields.push(Some(p..p + len));
            p += len;
        }
        self.pos = p;
        Ok(true)
    }
}

// ---------------------------------------------------------------- decoding

fn utf8(b: &[u8]) -> std::result::Result<String, String> {
    String::from_utf8(b.to_vec()).map_err(|_| "texto que no es UTF-8".to_string())
}

fn be<const N: usize>(b: &[u8], kind: Kind) -> std::result::Result<[u8; N], String> {
    b.try_into().map_err(|_| format!("{} bytes inesperados para {kind:?}", b.len()))
}

pub(crate) fn decode(kind: Kind, b: &[u8]) -> std::result::Result<Cell, String> {
    Ok(match kind {
        Kind::Bool => Cell::Bool(be::<1>(b, kind)?[0] != 0),
        Kind::Int2 => Cell::Int(i16::from_be_bytes(be(b, kind)?) as i64),
        Kind::Int4 => Cell::Int(i32::from_be_bytes(be(b, kind)?) as i64),
        Kind::Int8 => Cell::Int(i64::from_be_bytes(be(b, kind)?)),
        Kind::Float4 => Cell::Float(f32::from_be_bytes(be(b, kind)?) as f64),
        Kind::Float8 => Cell::Float(f64::from_be_bytes(be(b, kind)?)),
        Kind::Numeric => decode_numeric(b)?,
        Kind::Text => Cell::Text(utf8(b)?),
        Kind::Json => Cell::Json(utf8(b)?),
        Kind::Jsonb => match b.split_first() {
            Some((1, rest)) => Cell::Json(utf8(rest)?),
            _ => return Err("versión de jsonb desconocida".into()),
        },
        Kind::Bytea => Cell::Bytes(b.to_vec()),
        Kind::Uuid => {
            let u = be::<16>(b, kind)?;
            let h: String = u.iter().map(|x| format!("{x:02x}")).collect();
            Cell::Uuid(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
        }
        Kind::Date => match i32::from_be_bytes(be(b, kind)?) {
            i32::MAX => Cell::Text("infinity".into()),
            i32::MIN => Cell::Text("-infinity".into()),
            d => match fmt_date(d as i64) {
                (date, Era::Ad) => Cell::Date(date),
                (date, Era::Far) => Cell::Text(date),
                (date, Era::Bc) => Cell::Text(format!("{date} BC")),
            },
        },
        Kind::Time => Cell::Time(fmt_time(i64::from_be_bytes(be(b, kind)?))),
        Kind::Timestamp | Kind::Timestamptz => {
            let tz = kind == Kind::Timestamptz;
            match i64::from_be_bytes(be(b, kind)?) {
                i64::MAX => Cell::Text("infinity".into()),
                i64::MIN => Cell::Text("-infinity".into()),
                us => {
                    let (date, era) = fmt_date(us.div_euclid(DAY_US));
                    let mut s = format!("{date} {}", fmt_time(us.rem_euclid(DAY_US)));
                    if tz {
                        s.push_str("+00:00");
                    }
                    match (era, tz) {
                        (Era::Bc, _) => Cell::Text(format!("{s} BC")),
                        (Era::Far, _) => Cell::Text(s),
                        (Era::Ad, true) => Cell::DateTimeTz(s),
                        (Era::Ad, false) => Cell::DateTime(s),
                    }
                }
            }
        }
    })
}

/// Where a date's year falls, for how its cell is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    /// 1..=9999: a date cell.
    Ad,
    /// After 9999 AD: text, the year with five or more digits.
    Far,
    /// Before 1 AD: text, the year as PostgreSQL spells it (positive,
    /// followed by ` BC`, which the caller appends).
    Bc,
}

/// `YYYY-MM-DD` of a day counted from 2000-01-01, and its [`Era`].
fn fmt_date(days: i64) -> (String, Era) {
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    if y <= 0 {
        (format!("{:04}-{m:02}-{d:02}", 1 - y), Era::Bc)
    } else {
        (format!("{y:04}-{m:02}-{d:02}"), if y <= 9999 { Era::Ad } else { Era::Far })
    }
}

fn fmt_time(us: i64) -> String {
    let (secs, frac) = (us / 1_000_000, us % 1_000_000);
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac != 0 {
        let _ = write!(s, ".{frac:06}");
    }
    s
}

/// Year, month, day of a day counted from 1970-01-01 (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn decode_numeric(b: &[u8]) -> std::result::Result<Cell, String> {
    if b.len() < 8 {
        return Err("numeric incompleto".into());
    }
    let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
    let n = u16_at(0) as usize;
    let weight = u16_at(2) as i16 as i64;
    let sign = u16_at(4);
    let dscale = u16_at(6) as usize;
    if b.len() != 8 + 2 * n {
        return Err("numeric con largo inválido".into());
    }
    match sign {
        0xC000 => return Ok(Cell::Text("NaN".into())),
        0xD000 => return Ok(Cell::Text("Infinity".into())),
        0xF000 => return Ok(Cell::Text("-Infinity".into())),
        0 | 0x4000 => {}
        _ => return Err("signo de numeric inválido".into()),
    }
    let digit = |i: i64| if i >= 0 && (i as usize) < n { u16_at(8 + 2 * i as usize) } else { 0 };
    let mut s = String::with_capacity(4 * n + dscale + 2);
    if sign == 0x4000 && n > 0 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            let d = digit(i);
            let _ = if i == 0 { write!(s, "{d}") } else { write!(s, "{d:04}") };
        }
    }
    if dscale > 0 {
        s.push('.');
        let start = s.len();
        let mut i = weight + 1;
        while s.len() - start < dscale {
            let _ = write!(s, "{:04}", digit(i));
            i += 1;
        }
        s.truncate(start + dscale);
    }
    Ok(Cell::Decimal(s))
}

// ---------------------------------------------------------------- encoding

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        format!("{f}")
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// A non-null cell as text (the text format, text columns).
fn cell_text(c: &Cell) -> Cow<'_, str> {
    match c {
        Cell::Null => Cow::Borrowed(""),
        Cell::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        Cell::Int(i) => Cow::Owned(i.to_string()),
        Cell::UInt(u) => Cow::Owned(u.to_string()),
        Cell::Float(f) => Cow::Owned(float_text(*f)),
        Cell::Bytes(b) => Cow::Owned(hex(b)),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => {
            Cow::Borrowed(s)
        }
    }
}

fn bad(c: &Cell, kind: Kind) -> String {
    format!("el valor {c:?} no se puede cargar en una columna {kind:?}")
}

fn to_i64(c: &Cell, kind: Kind) -> std::result::Result<i64, String> {
    match c {
        Cell::Int(i) => Ok(*i),
        Cell::UInt(u) => i64::try_from(*u).map_err(|_| bad(c, kind)),
        Cell::Bool(b) => Ok(*b as i64),
        // [-2^63, 2^63): `as` would saturate anything outside.
        Cell::Float(f) if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f < -(i64::MIN as f64) => Ok(*f as i64),
        Cell::Decimal(s) | Cell::Text(s) => {
            let t = s.trim();
            t.parse::<i64>().ok().or_else(|| {
                let (i, f) = t.split_once('.')?;
                f.bytes().all(|x| x == b'0').then(|| i.parse().ok()).flatten()
            })
            .ok_or_else(|| bad(c, kind))
        }
        _ => Err(bad(c, kind)),
    }
}

fn to_f64(c: &Cell, kind: Kind) -> std::result::Result<f64, String> {
    match c {
        Cell::Float(f) => Ok(*f),
        Cell::Int(i) => Ok(*i as f64),
        Cell::UInt(u) => Ok(*u as f64),
        Cell::Decimal(s) | Cell::Text(s) => parse_float(s).ok_or_else(|| bad(c, kind)),
        _ => Err(bad(c, kind)),
    }
}

fn parse_float(s: &str) -> Option<f64> {
    match s.trim().to_ascii_lowercase().as_str() {
        "nan" => Some(f64::NAN),
        "infinity" | "+infinity" | "inf" => Some(f64::INFINITY),
        "-infinity" | "-inf" => Some(f64::NEG_INFINITY),
        t => t.parse().ok(),
    }
}

/// Append one field (length + value) of a binary `COPY` tuple.
pub(crate) fn encode(kind: Kind, c: &Cell, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    if matches!(c, Cell::Null) {
        out.extend_from_slice(&(-1i32).to_be_bytes());
        return Ok(());
    }
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    match kind {
        Kind::Bool => {
            let v = match c {
                Cell::Bool(b) => *b,
                Cell::Int(_) | Cell::UInt(_) => to_i64(c, kind)? != 0,
                Cell::Text(s) | Cell::Decimal(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "t" | "true" | "1" | "y" | "yes" | "on" => true,
                    "f" | "false" | "0" | "n" | "no" | "off" => false,
                    _ => return Err(bad(c, kind)),
                },
                _ => return Err(bad(c, kind)),
            };
            out.push(v as u8);
        }
        Kind::Int2 => out.extend_from_slice(&i16::try_from(to_i64(c, kind)?).map_err(|_| bad(c, kind))?.to_be_bytes()),
        Kind::Int4 => out.extend_from_slice(&i32::try_from(to_i64(c, kind)?).map_err(|_| bad(c, kind))?.to_be_bytes()),
        Kind::Int8 => out.extend_from_slice(&to_i64(c, kind)?.to_be_bytes()),
        Kind::Float4 => {
            let f = match c {
                // Parsed as f32 directly: no double rounding through f64.
                Cell::Text(s) | Cell::Decimal(s) => match parse_float(s) {
                    Some(f) if f.is_finite() => s.trim().parse::<f32>().map_err(|_| bad(c, kind))?,
                    Some(f) => f as f32,
                    None => return Err(bad(c, kind)),
                },
                _ => to_f64(c, kind)? as f32,
            };
            // A finite value that doesn't fit a real is an error, as in
            // PostgreSQL ("out of range for type real"), not Infinity.
            if f.is_infinite() && parse_float(&cell_text(c)).is_some_and(f64::is_finite) {
                return Err(bad(c, kind));
            }
            out.extend_from_slice(&f.to_be_bytes());
        }
        Kind::Float8 => out.extend_from_slice(&to_f64(c, kind)?.to_be_bytes()),
        Kind::Numeric => match c {
            Cell::Decimal(_) | Cell::Text(_) | Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) => encode_numeric(&cell_text(c), out)?,
            _ => return Err(bad(c, kind)),
        },
        Kind::Text | Kind::Json => out.extend_from_slice(cell_text(c).as_bytes()),
        Kind::Jsonb => {
            out.push(1);
            out.extend_from_slice(cell_text(c).as_bytes());
        }
        Kind::Bytea => match c {
            Cell::Bytes(b) => out.extend_from_slice(b),
            Cell::Text(s) if s.starts_with("\\x") => out.extend_from_slice(&unhex(&s[2..]).ok_or_else(|| bad(c, kind))?),
            other => out.extend_from_slice(cell_text(other).as_bytes()),
        },
        Kind::Uuid => match c {
            Cell::Bytes(b) if b.len() == 16 => out.extend_from_slice(b),
            Cell::Uuid(s) | Cell::Text(s) => {
                let h: String = s.trim().trim_matches(|x| x == '{' || x == '}').chars().filter(|x| *x != '-').collect();
                match unhex(&h) {
                    Some(b) if b.len() == 16 => out.extend_from_slice(&b),
                    _ => return Err(bad(c, kind)),
                }
            }
            _ => return Err(bad(c, kind)),
        },
        Kind::Date => match c {
            Cell::Date(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Text(s) => {
                let d = match s.trim().to_ascii_lowercase().as_str() {
                    "infinity" => i32::MAX,
                    "-infinity" => i32::MIN,
                    _ => {
                        let (days, _, _) = parse_timestamp(s).ok_or_else(|| bad(c, kind))?;
                        i32::try_from(days).map_err(|_| bad(c, kind))?
                    }
                };
                out.extend_from_slice(&d.to_be_bytes());
            }
            _ => return Err(bad(c, kind)),
        },
        Kind::Time => match c {
            Cell::Time(s) | Cell::Text(s) => {
                let us = parse_time(s.trim()).and_then(|(us, rest)| rest.trim().is_empty().then_some(us)).ok_or_else(|| bad(c, kind))?;
                out.extend_from_slice(&us.to_be_bytes());
            }
            _ => return Err(bad(c, kind)),
        },
        Kind::Timestamp | Kind::Timestamptz => match c {
            Cell::Date(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Text(s) => {
                let us = match s.trim().to_ascii_lowercase().as_str() {
                    "infinity" => i64::MAX,
                    "-infinity" => i64::MIN,
                    _ => {
                        let (days, us, offset) = parse_timestamp(s).ok_or_else(|| bad(c, kind))?;
                        // Without a zone, a timestamptz is taken as UTC; a
                        // plain timestamp keeps the wall clock as written.
                        let offset = if kind == Kind::Timestamptz { offset.unwrap_or(0) } else { 0 };
                        (days * DAY_US + us).checked_sub(offset * 1_000_000).ok_or_else(|| bad(c, kind))?
                    }
                };
                out.extend_from_slice(&us.to_be_bytes());
            }
            _ => return Err(bad(c, kind)),
        },
    }
    let len = (out.len() - at - 4) as i32;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let v = |x: u8| (x as char).to_digit(16).map(|d| d as u8);
    b.chunks(2).map(|p| Some(v(p[0])? << 4 | v(p[1])?)).collect()
}

/// `HH:MM[:SS[.fff…]]` as microseconds (rounded to the microsecond), and
/// what follows it.
fn parse_time(s: &str) -> Option<(i64, &str)> {
    let num = |s: &str| -> Option<(i64, usize)> {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        (n > 0).then(|| (s[..n].parse().ok(), n)).and_then(|(v, n)| Some((v?, n)))
    };
    let (h, n) = num(s)?;
    let s = s[n..].strip_prefix(':')?;
    let (m, n) = num(s)?;
    let mut s = &s[n..];
    let mut sec = 0;
    let mut us = 0;
    if let Some(r) = s.strip_prefix(':') {
        let (v, n) = num(r)?;
        sec = v;
        s = &r[n..];
        if let Some(r) = s.strip_prefix('.') {
            let n = r.bytes().take_while(u8::is_ascii_digit).count();
            let digits = &r[..n];
            let mut v: i64 = 0;
            for (i, d) in digits.bytes().take(6).enumerate() {
                v += (d - b'0') as i64 * 10i64.pow(5 - i as u32);
            }
            if digits.as_bytes().get(6).is_some_and(|d| *d >= b'5') {
                v += 1;
            }
            us = v;
            s = &r[n..];
        }
    }
    if h > 24 || m > 59 || sec > 60 {
        return None;
    }
    Some((((h * 60 + m) * 60 + sec) * 1_000_000 + us, s))
}

/// `[Y]YYY-MM-DD[( |T)HH:MM:SS[.f]][Z|±HH[:MM[:SS]]][ BC]`: days from
/// 2000-01-01, microseconds into the day and the zone's offset in seconds.
fn parse_timestamp(s: &str) -> Option<(i64, i64, Option<i64>)> {
    let mut s = s.trim();
    let bc = s.len() > 3 && s[s.len() - 3..].eq_ignore_ascii_case(" bc");
    if bc {
        s = s[..s.len() - 3].trim_end();
    }
    let (y, rest) = s.split_once('-')?;
    let (m, rest) = rest.split_once('-')?;
    let dn = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (d, mut rest) = rest.split_at(dn);
    let (y, m, d): (i64, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || (bc && y < 1) {
        return None;
    }
    let y = if bc { 1 - y } else { y };
    let days = days_from_civil(y, m, d) - PG_EPOCH_DAYS;
    let mut us = 0;
    if let Some(r) = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('T')) {
        let (t, r) = parse_time(r.trim_start())?;
        us = t;
        rest = r;
    }
    let rest = rest.trim();
    let offset = if rest.is_empty() {
        None
    } else if rest.eq_ignore_ascii_case("z") || rest.eq_ignore_ascii_case("utc") {
        Some(0)
    } else {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let r = &rest[1..];
        let parts: Vec<i64> = if r.contains(':') {
            r.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?
        } else if r.len() == 4 {
            vec![r[..2].parse().ok()?, r[2..].parse().ok()?]
        } else {
            vec![r.parse().ok()?]
        };
        let secs = parts.first()? * 3600 + parts.get(1).unwrap_or(&0) * 60 + parts.get(2).unwrap_or(&0);
        Some(sign * secs)
    };
    Some((days, us, offset))
}

/// A decimal string (sign, point, optional exponent; or NaN / ±Infinity)
/// as a binary `numeric`.
fn encode_numeric(s: &str, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    let head = |out: &mut Vec<u8>, n: u16, weight: i16, sign: u16, dscale: u16| {
        for x in [n, weight as u16, sign, dscale] {
            out.extend_from_slice(&x.to_be_bytes());
        }
    };
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" => {
            head(out, 0, 0, 0xC000, 0);
            return Ok(());
        }
        "infinity" | "+infinity" | "inf" => {
            head(out, 0, 0, 0xD000, 0);
            return Ok(());
        }
        "-infinity" | "-inf" => {
            head(out, 0, 0, 0xF000, 0);
            return Ok(());
        }
        _ => {}
    }
    let invalid = || format!("«{s}» no es un número");
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i64>().map_err(|_| invalid())?),
        None => (body, 0),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if ip.is_empty() && fp.is_empty() || !ip.bytes().chain(fp.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let mut digits: Vec<u8> = ip.bytes().chain(fp.bytes()).map(|b| b - b'0').collect();
    // Digits before the point.
    let mut point = ip.len() as i64 + exp;
    let dscale = (digits.len() as i64 - point).max(0);
    if dscale > 16383 || point > 131072 {
        return Err(invalid());
    }
    if point < 0 {
        digits.splice(0..0, std::iter::repeat_n(0, (-point) as usize));
        point = 0;
    }
    if point as usize > digits.len() {
        digits.resize(point as usize, 0);
    }
    let lead = ((4 - point % 4) % 4) as usize;
    digits.splice(0..0, std::iter::repeat_n(0, lead));
    let point = point as usize + lead;
    let trail = (4 - (digits.len() - point) % 4) % 4;
    digits.resize(digits.len() + trail, 0);
    let mut groups: Vec<u16> = digits.chunks(4).map(|c| c.iter().fold(0u16, |a, d| a * 10 + *d as u16)).collect();
    let mut weight = (point / 4) as i64 - 1;
    let first = groups.iter().position(|g| *g != 0).unwrap_or(groups.len());
    groups.drain(..first);
    weight -= first as i64;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        head(out, 0, 0, 0, dscale as u16);
        return Ok(());
    }
    let weight = i16::try_from(weight).map_err(|_| invalid())?;
    head(out, groups.len() as u16, weight, if neg { 0x4000 } else { 0 }, dscale as u16);
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Ok(())
}

/// One row in `COPY`'s text format.
fn encode_text_row(row: &[Cell], kinds: &[Option<Kind>], out: &mut Vec<u8>) {
    for (i, c) in row.iter().enumerate() {
        if i > 0 {
            out.push(b'\t');
        }
        if matches!(c, Cell::Null) {
            out.extend_from_slice(b"\\N");
            continue;
        }
        for b in cell_text(c).bytes() {
            match b {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                _ => out.push(b),
            }
        }
        // A zone-less timestamp into a timestamptz is UTC, as in binary.
        if kinds.get(i) == Some(&Some(Kind::Timestamptz)) && matches!(c, Cell::DateTime(_) | Cell::Date(_)) {
            out.extend_from_slice(b"+00");
        }
    }
    out.push(b'\n');
}

// ---------------------------------------------------------------- loading

type Sink = Pin<Box<CopyInSink<Cursor<Vec<u8>>>>>;

async fn send(sink: &mut Sink, buf: &mut Vec<u8>) -> Result<()> {
    if !buf.is_empty() {
        let chunk = std::mem::replace(buf, Vec::with_capacity(SEND_CHUNK + 64 * 1024));
        sink.send(Cursor::new(chunk)).await.map_err(err)?;
    }
    Ok(())
}

pub(crate) async fn bulk_load(s: &mut PgSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if !bulk_capable(s.variant) {
        return Err(Error::Unsupported(format!("{} no tiene COPY FROM STDIN", s.variant.info().name)));
    }
    let table = table_name(&spec.table);
    let cols = column_list(&spec.columns);
    let probe = probe(s, &table, Some(&spec.columns)).await?;
    let kinds: Vec<Option<Kind>> = probe.iter().map(|(_, t)| Kind::of(t.oid())).collect();
    let binary = binary_copy(s.variant) && kinds.iter().all(Option::is_some);
    let copy = if binary {
        format!("COPY {table} ({cols}) FROM STDIN (FORMAT binary)")
    } else {
        format!("COPY {table} ({cols}) FROM STDIN")
    };
    let max_rows = if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows };
    let max_bytes = if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes };

    let mut total = 0u64;
    let mut sink: Option<Sink> = None;
    let mut buf = Vec::with_capacity(SEND_CHUNK + 64 * 1024);
    let (mut rows, mut bytes) = (0u64, 0u64);
    while let Some(batch) = source.next().await {
        for row in &batch.rows {
            if row.len() != kinds.len() {
                return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {} columnas", row.len(), kinds.len())));
            }
            if sink.is_none() {
                sink = Some(Box::pin(s.client.copy_in(&copy).await.map_err(err)?));
                if binary {
                    buf.extend_from_slice(HEADER);
                }
            }
            let before = buf.len();
            if binary {
                buf.extend_from_slice(&(kinds.len() as i16).to_be_bytes());
                for (i, (c, k)) in row.iter().zip(&kinds).enumerate() {
                    encode(k.expect("binary kinds"), c, &mut buf).map_err(|e| {
                        Error::Query(format!("fila {}, columna «{}»: {e}", total + rows + 1, spec.columns[i]))
                    })?;
                }
            } else {
                encode_text_row(row, &kinds, &mut buf);
            }
            rows += 1;
            bytes += (buf.len() - before) as u64;
            let window = sink.as_mut().expect("copy open");
            if buf.len() >= SEND_CHUNK {
                send(window, &mut buf).await?;
            }
            if rows >= max_rows || bytes >= max_bytes {
                total += finish(window, &mut buf, binary).await?;
                sink = None;
                (rows, bytes) = (0, 0);
                progress(total);
            }
        }
    }
    if let Some(window) = sink.as_mut() {
        total += finish(window, &mut buf, binary).await?;
        progress(total);
    }
    Ok(total)
}

async fn finish(sink: &mut Sink, buf: &mut Vec<u8>, binary: bool) -> Result<u64> {
    if binary {
        buf.extend_from_slice(&(-1i16).to_be_bytes());
    }
    send(sink, buf).await?;
    sink.as_mut().finish().await.map_err(err)
}

// ---------------------------------------------------------------- native copy

/// Built-in types whose binary value only means the same on its own server:
/// `money` (an integer count of the smallest unit, whose decimals come from
/// each server's `lc_monetary`) and the object identifiers (`oid`, `reg*`),
/// which name other objects, or none, on another database. Arrays of them
/// too. Read + load carries them as numeric and text instead.
fn server_bound(ty: &Type) -> bool {
    let oid = match ty.kind() {
        tokio_postgres::types::Kind::Array(inner) => inner.oid(),
        _ => ty.oid(),
    };
    // regproc, oid, regprocedure…regtype, regconfig, regdictionary,
    // regnamespace, regrole, regcollation.
    matches!(oid, MONEY | 24 | 26 | 2202..=2206 | 3734 | 3769 | 4089 | 4096 | 4191)
}

fn pg_session(s: &mut dyn Session) -> Option<&mut PgSession> {
    s.as_any()?.downcast_mut::<PgSession>()
}

pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: &str| Err(Error::Unsupported(format!("copia directa no disponible: {why}")));
    let (Some(src), Some(dst)) = (pg_session(source), pg_session(target)) else {
        return unsupported("las sesiones no son de PostgreSQL");
    };
    if !native_capable(src.variant, dst.variant) {
        return unsupported("el motor no tiene COPY binario");
    }
    let src_table = table_name(&spec.source.table);
    let dst_table = table_name(&spec.target.table);
    let src_cols = probe(src, &src_table, spec.source.columns.as_deref()).await?;
    let dst_cols = probe(dst, &dst_table, Some(&spec.target.columns)).await?;
    if src_cols.len() != dst_cols.len() {
        return unsupported("origen y destino tienen distinta cantidad de columnas");
    }
    for ((sn, st), (dn, dt)) in src_cols.iter().zip(&dst_cols) {
        if st.oid() != dt.oid() || st.oid() >= FIRST_USER_OID {
            return unsupported(&format!("la columna «{sn}» ({}) no tiene el mismo tipo nativo en «{dn}» ({})", st.name(), dt.name()));
        }
        if server_bound(st) {
            return unsupported(&format!("el valor binario de la columna «{sn}» ({}) depende del servidor", st.name()));
        }
    }
    let names: Vec<String> = src_cols.iter().map(|(n, _)| n.clone()).collect();
    let select = select_sql(&src_table, &column_list(&names), spec.source.filter.as_deref());
    let copy_out = format!("COPY ({select}) TO STDOUT (FORMAT binary)");
    let copy_in = format!("COPY {dst_table} ({}) FROM STDIN (FORMAT binary)", column_list(&spec.target.columns));

    let stream = src.client.copy_out(&copy_out).await.map_err(err)?;
    pin_mut!(stream);
    // The first window opens now, so a target that refuses the COPY fails
    // before anything is read.
    let mut sink: Option<Sink> = Some(Box::pin(dst.client.copy_in(&copy_in).await.map_err(err)?));
    let every = if spec.target.commit_rows == 0 { u64::MAX } else { spec.target.commit_rows };
    let mut parser = CopyParser::new(src_cols.len());
    let mut fields = Vec::new();
    // Rows committed, and rows sent to the open window.
    let (mut committed, mut window) = (0u64, 0u64);
    // `parser.buf[..sent]` is already in `buf` or sent.
    let mut sent = 0usize;
    let mut buf = Vec::with_capacity(SEND_CHUNK + 64 * 1024);
    let result: Result<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(err)?;
            // Drops the bytes before `consumed()`, all of them sent.
            parser.feed(&chunk);
            sent = 0;
            while parser.next_tuple(&mut fields).map_err(Error::Query)? {
                window += 1;
                if window < every {
                    continue;
                }
                // Close the window at this tuple's end: its bytes, the
                // trailer, and the target's COPY commits.
                if sink.is_none() {
                    open_window(&mut sink, dst, &copy_in, &mut buf).await?;
                }
                let w = sink.as_mut().expect("window open");
                buf.extend_from_slice(&parser.buf[sent..parser.consumed()]);
                sent = parser.consumed();
                buf.extend_from_slice(&(-1i16).to_be_bytes());
                send(w, &mut buf).await?;
                committed += w.as_mut().finish().await.map_err(err)?;
                sink = None;
                window = 0;
                progress(committed);
            }
            let end = parser.consumed();
            if end > sent {
                // Tuples of the next window, and maybe the trailer. Only the
                // trailer (no tuple since the last window): nothing to send.
                if sink.is_none() && window > 0 {
                    open_window(&mut sink, dst, &copy_in, &mut buf).await?;
                }
                if let Some(w) = sink.as_mut() {
                    buf.extend_from_slice(&parser.buf[sent..end]);
                    if buf.len() >= SEND_CHUNK {
                        send(w, &mut buf).await?;
                    }
                }
                sent = end;
            }
        }
        if !parser.done {
            return Err(Error::Query("el COPY de origen terminó sin su marca de fin".into()));
        }
        if let Some(w) = sink.as_mut() {
            send(w, &mut buf).await?;
            committed += w.as_mut().finish().await.map_err(err)?;
            sink = None;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        // Stop the source's COPY on the server; dropping the open window
        // aborts the target's (CopyFail). The windows before it stay
        // committed, as in a load.
        drop(sink);
        let _ = src.client.cancel_token().cancel_query(src.tls.clone()).await;
        return Err(e);
    }
    progress(committed);
    Ok(committed)
}

/// Opens the next window of a native copy: a new `COPY FROM` whose data
/// starts with the binary header (the source's went to the first one).
async fn open_window(sink: &mut Option<Sink>, dst: &PgSession, copy_in: &str, buf: &mut Vec<u8>) -> Result<()> {
    *sink = Some(Box::pin(dst.client.copy_in(copy_in).await.map_err(err)?));
    buf.extend_from_slice(HEADER);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(kind: Kind, cell: Cell) -> Cell {
        let mut buf = Vec::new();
        encode(kind, &cell, &mut buf).unwrap_or_else(|e| panic!("{kind:?} {cell:?}: {e}"));
        let len = i32::from_be_bytes(buf[..4].try_into().unwrap());
        if len < 0 {
            return Cell::Null;
        }
        assert_eq!(len as usize, buf.len() - 4);
        decode(kind, &buf[4..]).unwrap()
    }

    fn same(kind: Kind, cell: Cell) {
        assert_eq!(round(kind, cell.clone()), cell, "{kind:?}");
    }

    #[test]
    fn every_type_round_trips() {
        same(Kind::Bool, Cell::Bool(true));
        same(Kind::Bool, Cell::Bool(false));
        for v in [i16::MIN as i64, -1, 0, i16::MAX as i64] {
            same(Kind::Int2, Cell::Int(v));
        }
        for v in [i32::MIN as i64, 42, i32::MAX as i64] {
            same(Kind::Int4, Cell::Int(v));
        }
        for v in [i64::MIN, 0, i64::MAX] {
            same(Kind::Int8, Cell::Int(v));
        }
        same(Kind::Float4, Cell::Float(1.5));
        same(Kind::Float4, Cell::Float(f32::MAX as f64));
        for v in [0.1, -1e300, f64::MIN_POSITIVE, f64::INFINITY] {
            same(Kind::Float8, Cell::Float(v));
        }
        assert!(matches!(round(Kind::Float8, Cell::Float(f64::NAN)), Cell::Float(f) if f.is_nan()));
        for d in [
            "0",
            "0.00",
            "1",
            "-1",
            "10000",
            "123456789.000100",
            "0.0000000001",
            "-0.5",
            "9999999999999999999999999999.9999999999",
            "-9999999999999999999999999999.9999999999",
            "100000000000000000000000000000000000000",
            "3.14159265358979323846264338327950288419716939937510",
        ] {
            same(Kind::Numeric, Cell::Decimal(d.into()));
        }
        for t in ["NaN", "Infinity", "-Infinity"] {
            same(Kind::Numeric, Cell::Text(t.into()));
        }
        assert_eq!(round(Kind::Numeric, Cell::Text("1.5e3".into())), Cell::Decimal("1500".into()));
        assert_eq!(round(Kind::Numeric, Cell::Int(-7)), Cell::Decimal("-7".into()));
        same(Kind::Text, Cell::Text("ñandú \t\n\\ 日本".into()));
        same(Kind::Bytea, Cell::Bytes((0..=255).collect()));
        same(Kind::Bytea, Cell::Bytes(Vec::new()));
        same(Kind::Uuid, Cell::Uuid("123e4567-e89b-12d3-a456-426614174000".into()));
        for d in ["2000-01-01", "1999-12-31", "0001-01-01", "9999-12-31", "1970-01-01", "2024-02-29"] {
            same(Kind::Date, Cell::Date(d.into()));
        }
        same(Kind::Date, Cell::Text("0044-03-15 BC".into()));
        same(Kind::Date, Cell::Text("infinity".into()));
        for t in ["00:00:00", "23:59:59.999999", "12:34:56.000001", "24:00:00"] {
            same(Kind::Time, Cell::Time(t.into()));
        }
        for t in ["2000-01-01 00:00:00", "1900-06-15 08:30:00.123456", "2262-04-11 23:47:16.854775", "0001-01-01 00:00:00"] {
            same(Kind::Timestamp, Cell::DateTime(t.into()));
        }
        same(Kind::Timestamp, Cell::Text("-infinity".into()));
        same(Kind::Timestamp, Cell::Text("0100-01-01 10:00:00 BC".into()));
        same(Kind::Timestamptz, Cell::DateTimeTz("2024-05-01 12:00:00.5+00:00".replace(".5", ".500000")));
        assert_eq!(
            round(Kind::Timestamptz, Cell::DateTimeTz("2024-05-01 09:00:00-03:00".into())),
            Cell::DateTimeTz("2024-05-01 12:00:00+00:00".into())
        );
        assert_eq!(
            round(Kind::Timestamptz, Cell::DateTime("2024-05-01T12:00:00.1234567".into())),
            Cell::DateTimeTz("2024-05-01 12:00:00.123457+00:00".into())
        );
        same(Kind::Json, Cell::Json("{\"a\": [1, 2]}".into()));
        same(Kind::Jsonb, Cell::Json("{\"a\": [1, 2]}".into()));
        assert_eq!(round(Kind::Int4, Cell::Null), Cell::Null);
    }

    #[test]
    fn bad_values_are_errors() {
        let mut b = Vec::new();
        assert!(encode(Kind::Int2, &Cell::Int(40_000), &mut b).is_err());
        assert!(encode(Kind::Uuid, &Cell::Text("nope".into()), &mut b).is_err());
        assert!(encode(Kind::Numeric, &Cell::Text("1.2.3".into()), &mut b).is_err());
        assert!(encode(Kind::Date, &Cell::Text("2024-13-01".into()), &mut b).is_err());
    }

    #[test]
    fn years_after_9999_stay_ad() {
        for d in ["10000-01-01", "5874897-12-31"] {
            same(Kind::Date, Cell::Text(d.into()));
        }
        same(Kind::Timestamp, Cell::Text("10000-01-01 00:00:00".into()));
        same(Kind::Timestamptz, Cell::Text("294276-12-31 23:59:59.999999+00:00".into()));
        // The same instant parsed back by the loader is AD, not BC.
        assert_eq!(parse_timestamp("10000-01-01").unwrap().0, days_from_civil(10000, 1, 1) - PG_EPOCH_DAYS);
    }

    #[test]
    fn out_of_range_numbers_are_errors_not_saturated() {
        let mut b = Vec::new();
        assert!(encode(Kind::Float4, &Cell::Float(1e300), &mut b).is_err());
        assert!(encode(Kind::Float4, &Cell::Text("1e40".into()), &mut b).is_err());
        assert!(encode(Kind::Float4, &Cell::Decimal("-1e40".into()), &mut b).is_err());
        same(Kind::Float4, Cell::Float(f64::INFINITY));
        same(Kind::Float4, Cell::Float(f64::NEG_INFINITY));
        assert_eq!(round(Kind::Float4, Cell::Text("Infinity".into())), Cell::Float(f64::INFINITY));
        assert!(encode(Kind::Int8, &Cell::Float(9.25e18), &mut b).is_err());
        assert!(encode(Kind::Int8, &Cell::Float(-(i64::MIN as f64)), &mut b).is_err());
        assert!(encode(Kind::Int8, &Cell::Float(-9.25e18), &mut b).is_err());
        assert_eq!(round(Kind::Int8, Cell::Float(i64::MIN as f64)), Cell::Int(i64::MIN));
        assert_eq!(round(Kind::Int8, Cell::Float((i64::MAX - 1023) as f64)), Cell::Int(i64::MAX - 1023));
    }

    #[test]
    fn server_bound_types_skip_the_native_copy() {
        for t in [Type::MONEY, Type::MONEY_ARRAY, Type::OID, Type::REGCLASS, Type::REGCLASS_ARRAY, Type::REGTYPE, Type::REGPROC, Type::REGROLE] {
            assert!(server_bound(&t), "{t}");
        }
        for t in [Type::INT4, Type::INT4_ARRAY, Type::NUMERIC, Type::TEXT, Type::TIMESTAMPTZ] {
            assert!(!server_bound(&t), "{t}");
        }
    }

    #[test]
    fn a_copy_stream_parses_in_any_chunking() {
        let kinds = [Kind::Int4, Kind::Text, Kind::Bytea];
        let rows = vec![
            vec![Cell::Int(1), Cell::Text("a".into()), Cell::Bytes(vec![0; 3000])],
            vec![Cell::Null, Cell::Null, Cell::Null],
            vec![Cell::Int(-5), Cell::Text("".into()), Cell::Bytes(vec![7])],
        ];
        let mut stream = HEADER.to_vec();
        for r in &rows {
            stream.extend_from_slice(&3i16.to_be_bytes());
            for (c, k) in r.iter().zip(kinds) {
                encode(k, c, &mut stream).unwrap();
            }
        }
        stream.extend_from_slice(&(-1i16).to_be_bytes());
        for size in [1, 3, 7, 1000, stream.len()] {
            let mut p = CopyParser::new(3);
            let mut fields = Vec::new();
            let mut got = Vec::new();
            for chunk in stream.chunks(size) {
                p.feed(chunk);
                while p.next_tuple(&mut fields).unwrap() {
                    let cells: Vec<Cell> =
                        fields.iter().zip(kinds).map(|(f, k)| f.clone().map_or(Cell::Null, |r| decode(k, &p.buf[r]).unwrap())).collect();
                    got.push(cells);
                }
            }
            assert!(p.done, "chunks of {size}");
            assert_eq!(got, rows, "chunks of {size}");
        }
    }

    #[test]
    fn text_rows_escape_and_mark_nulls() {
        let mut out = Vec::new();
        encode_text_row(
            &[Cell::Text("a\tb\\c\nd".into()), Cell::Null, Cell::Bytes(vec![1, 255]), Cell::DateTime("2024-01-01 00:00:00".into())],
            &[Some(Kind::Text), None, Some(Kind::Bytea), Some(Kind::Timestamptz)],
            &mut out,
        );
        assert_eq!(out, b"a\\tb\\\\c\\nd\t\\N\t\\\\x01ff\t2024-01-01 00:00:00+00\n");
    }

    #[test]
    fn variants_with_copy() {
        assert!(bulk_capable(Variant::Postgres) && bulk_capable(Variant::Greenplum));
        assert!(!bulk_capable(Variant::Cockroach) && !bulk_capable(Variant::Redshift) && !bulk_capable(Variant::CrateDb) && !bulk_capable(Variant::H2));
        assert!(native_capable(Variant::Postgres, Variant::Timescale));
        assert!(!native_capable(Variant::Postgres, Variant::Cockroach));
    }
}

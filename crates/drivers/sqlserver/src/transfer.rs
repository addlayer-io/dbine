//! Bulk transfer (see `dbine_driver::transfer`): typed reads, `INSERT BULK`
//! loads and same-driver copies where rows travel as raw TDS bytes.
//!
//! Column handling:
//! - Reads skip computed and `rowversion` columns unless asked for them.
//!   `money` is read as `decimal(19,4)` (tiberius decodes money as `f64`),
//!   CLR types (`geography`, `geometry`, `hierarchyid`) as `varbinary(max)`.
//! - Loads declare the types tiberius can't encode as a wire type the server
//!   converts on insert: `xml` / `ntext` → `nvarchar(max)`, `text` →
//!   `varchar(max)`, `image` and CLR types → `varbinary(max)`,
//!   `smalldatetime` → `datetime`. A `CAST` is always reported nullable, so a
//!   NOT NULL column is declared `ISNULL(CAST(NULL AS w), neutral)`: its wire
//!   type then matches the table's.
//! - `sql_variant` is refused: it can't be declared to `INSERT BULK` nor
//!   passed through as raw bytes.

use crate::{err, format_type, text, SqlServerSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::transfer::{
    BatchBuilder, BatchSink, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn,
    CHUNK_BYTES, CHUNK_ROWS,
};
use dbine_driver::{async_trait, Error, ObjectRef, Result};
use futures::TryStreamExt;
use std::borrow::Cow;
use std::io;
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::xml::XmlData;
use tiberius::{
    BulkLoadRequest, ColumnData, FixedLenType, QueryItem, RawItem, RawMetadata, SqlBulkCopyOption, SqlBulkCopyOptions, TokenRow, TypeInfo,
    VarLenType,
};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

/// Turns the raw passthrough off (every native copy decodes rows).
pub const NO_RAW_ENV: &str = "DBINE_SQLSERVER_NO_RAW";

/// The TDS packet size sessions ask for: ~8x fewer packets (and TLS
/// records) than the default 4096 in bulk loads.
pub const PACKET_SIZE: u32 = 32_767;

/// A column as the catalog describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CatColumn {
    pub name: String,
    /// The system type (`nvarchar`, `money`…); alias types resolved.
    pub ty: String,
    pub max_len: i32,
    pub precision: i32,
    pub scale: i32,
    pub nullable: bool,
    pub computed: bool,
    pub identity: bool,
    /// A CLR type (`geography`, `geometry`, `hierarchyid`, user CLR types).
    pub clr: bool,
}

impl CatColumn {
    fn rowversion(&self) -> bool {
        self.ty == "timestamp"
    }

    fn type_name(&self) -> String {
        format_type(&self.ty, self.max_len, self.precision, self.scale)
    }
}

const COLUMNS_SQL: &str = "SELECT c.name,
        CASE WHEN t.is_user_defined = 1 AND t.is_assembly_type = 0 THEN TYPE_NAME(c.system_type_id) ELSE t.name END,
        CAST(c.max_length AS int), CAST(c.precision AS int), CAST(c.scale AS int),
        c.is_nullable, c.is_computed, c.is_identity, t.is_assembly_type
   FROM sys.columns c
   JOIN sys.types t ON t.user_type_id = c.user_type_id
  WHERE c.object_id = OBJECT_ID(@P1)
  ORDER BY c.column_id";

fn table_name(t: &ObjectRef) -> String {
    qualified_name(Quote::Bracket, t.schema(), &t.name)
}

fn q(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

async fn catalog(s: &mut SqlServerSession, table: &ObjectRef) -> Result<Vec<CatColumn>> {
    let rows = s.rows(COLUMNS_SQL, &[&table_name(table)]).await?;
    if rows.is_empty() {
        return Err(Error::Query(format!("No existe la tabla {}", table_name(table))));
    }
    Ok(rows
        .iter()
        .map(|r| {
            let ty = text(r, 1).unwrap_or_default().to_ascii_lowercase();
            CatColumn {
                name: text(r, 0).unwrap_or_default(),
                ty: if ty == "sysname" { "nvarchar".into() } else { ty },
                max_len: r.get(2).unwrap_or(0),
                precision: r.get(3).unwrap_or(0),
                scale: r.get(4).unwrap_or(0),
                nullable: r.get(5).unwrap_or(true),
                computed: r.get(6).unwrap_or(false),
                identity: r.get(7).unwrap_or(false),
                clr: r.get(8).unwrap_or(false),
            }
        })
        .collect())
}

/// The columns to move: those requested, in their order, or every stored
/// column (no computed nor `rowversion` ones).
pub(crate) fn pick(cat: &[CatColumn], requested: Option<&[String]>) -> Result<Vec<CatColumn>> {
    match requested {
        None => Ok(cat.iter().filter(|c| !c.computed && !c.rowversion()).cloned().collect()),
        Some(names) => names
            .iter()
            .map(|n| {
                cat.iter()
                    .find(|c| c.name == *n)
                    .or_else(|| cat.iter().find(|c| c.name.eq_ignore_ascii_case(n)))
                    .cloned()
                    .ok_or_else(|| Error::Query(format!("La tabla no tiene la columna «{n}»")))
            })
            .collect(),
    }
}

fn refuse_variant(c: &CatColumn) -> Result<()> {
    if c.ty == "sql_variant" {
        return Err(Error::Unsupported(format!(
            "la columna «{}» es sql_variant, que no se puede transferir en bloque",
            c.name
        )));
    }
    Ok(())
}

/// How a column is read for typed values.
pub(crate) fn read_expr(c: &CatColumn) -> Result<String> {
    refuse_variant(c)?;
    let n = q(&c.name);
    Ok(match c.ty.as_str() {
        "money" | "smallmoney" => format!("CAST({n} AS decimal(19,4)) AS {n}"),
        _ if c.clr => format!("CAST({n} AS varbinary(max)) AS {n}"),
        _ => n,
    })
}

/// The type a column travels as in a bulk load, when it isn't its own.
pub(crate) fn wire_type(c: &CatColumn) -> Option<&'static str> {
    match c.ty.as_str() {
        "xml" | "ntext" => Some("nvarchar(max)"),
        "text" => Some("varchar(max)"),
        "image" => Some("varbinary(max)"),
        "smalldatetime" => Some("datetime"),
        _ if c.clr => Some("varbinary(max)"),
        _ => None,
    }
}

fn neutral(wire: &str) -> &'static str {
    match wire {
        "nvarchar(max)" => "N''",
        "varchar(max)" => "''",
        "varbinary(max)" => "0x",
        _ => "CAST(0 AS datetime)",
    }
}

/// How a target column is declared to `INSERT BULK` (select-list form).
pub(crate) fn declare_expr(c: &CatColumn) -> String {
    let n = q(&c.name);
    match wire_type(c) {
        Some(w) if c.nullable => format!("CAST(NULL AS {w}) AS {n}"),
        Some(w) => format!("ISNULL(CAST(NULL AS {w}), {}) AS {n}", neutral(w)),
        None => n,
    }
}

/// How a source column is read for the raw passthrough into `target`: in
/// the target's wire type, NOT NULL when both sides are (nullability changes
/// fixed-length wire types).
pub(crate) fn raw_expr(source: &CatColumn, target: &CatColumn) -> Result<String> {
    refuse_variant(source)?;
    let n = q(&source.name);
    Ok(match wire_type(source).or_else(|| wire_type(target)) {
        Some(w) if !source.nullable && !target.nullable => format!("ISNULL(CAST({n} AS {w}), {}) AS {n}", neutral(w)),
        Some(w) => format!("CAST({n} AS {w}) AS {n}"),
        None => n,
    })
}

fn select_sql(list: &[String], table: &ObjectRef, filter: Option<&str>) -> String {
    let mut sql = format!("SELECT {} FROM {}", list.join(", "), table_name(table));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(" WHERE ");
        sql.push_str(f);
    }
    sql
}

// ---------------------------------------------------------------- reading

/// A batch sink behind its mutex, locked only while handing over a batch.
struct Locked(BatchSinkRef);

impl BatchSink for Locked {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.0.lock().map_err(|_| io::Error::other("destino de lotes"))?.begin(columns)
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.0.lock().map_err(|_| io::Error::other("destino de lotes"))?.batch(batch)
    }
}

pub(crate) async fn read_batches(s: &mut SqlServerSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let cat = catalog(s, &spec.table).await?;
    let cols = pick(&cat, spec.columns.as_deref())?;
    let list = cols.iter().map(read_expr).collect::<Result<Vec<_>>>()?;
    let sql = select_sql(&list, &spec.table, spec.filter.as_deref());
    let mut sink = Locked(sink);
    sink.begin(
        &cols.iter().map(|c| TransferColumn { name: c.name.clone(), type_name: c.type_name(), nullable: c.nullable }).collect::<Vec<_>>(),
    )?;
    let mut builder = BatchBuilder::new();
    let res = async {
        let mut stream = s.client.simple_query(sql).await.map_err(err)?;
        let mut first = true;
        while let Some(item) = stream.try_next().await.map_err(err)? {
            match item {
                QueryItem::Metadata(_) if first => first = false,
                // Only the SELECT's result set.
                QueryItem::Metadata(_) => break,
                QueryItem::Row(row) => {
                    let cells = row.into_iter().map(to_cell).collect::<std::result::Result<Vec<_>, _>>().map_err(Error::Query)?;
                    builder.push(cells, &mut sink)?;
                }
            }
        }
        Ok::<_, Error>(())
    }
    .await;
    if let Err(e) = res {
        // A half-read result leaves the connection mid-stream.
        if let Err(re) = s.reconnect().await {
            tracing::debug!("sqlserver: reconnect after a failed read: {re}");
        }
        return Err(e);
    }
    builder.flush(&mut sink)?;
    Ok(builder.rows)
}

/// A decoded value as a transfer cell, without loss.
pub(crate) fn to_cell(d: ColumnData<'static>) -> std::result::Result<Cell, String> {
    fn or_null<T>(v: Option<T>, f: impl FnOnce(T) -> Cell) -> Cell {
        v.map_or(Cell::Null, f)
    }
    Ok(match d {
        ColumnData::U8(v) => or_null(v, |x| Cell::Int(x.into())),
        ColumnData::I16(v) => or_null(v, |x| Cell::Int(x.into())),
        ColumnData::I32(v) => or_null(v, |x| Cell::Int(x.into())),
        ColumnData::I64(v) => or_null(v, Cell::Int),
        ColumnData::F32(v) => or_null(v, |x| Cell::Float(x.into())),
        ColumnData::F64(v) => or_null(v, Cell::Float),
        ColumnData::Bit(v) => or_null(v, Cell::Bool),
        ColumnData::String(v) => or_null(v, |s| Cell::Text(s.into_owned())),
        ColumnData::Guid(v) => or_null(v, |g| Cell::Uuid(g.to_string().to_uppercase())),
        ColumnData::Binary(v) => or_null(v, |b| Cell::Bytes(b.into_owned())),
        ColumnData::Numeric(v) => or_null(v, |n| Cell::Decimal(fmt_decimal(n.value(), n.scale()))),
        ColumnData::Xml(v) => or_null(v, |x| Cell::Text(x.into_owned().into_string())),
        ColumnData::DateTime(v) => or_null(v, |d| Cell::DateTime(fmt_datetime(d))),
        ColumnData::SmallDateTime(v) => or_null(v, |d| Cell::DateTime(fmt_smalldatetime(d))),
        ColumnData::Date(v) => or_null(v, |d| Cell::Date(fmt_date(d.days() as i64 + 1))),
        ColumnData::Time(v) => or_null(v, |t| Cell::Time(fmt_time(t.increments(), t.scale()))),
        ColumnData::DateTime2(v) => or_null(v, |d| {
            Cell::DateTime(format!("{} {}", fmt_date(d.date().days() as i64 + 1), fmt_time(d.time().increments(), d.time().scale())))
        }),
        ColumnData::DateTimeOffset(v) => or_null(v, |d| Cell::DateTimeTz(fmt_datetimeoffset(d))),
    })
}

// ------------------------------------------------------ text forms of values

/// Days from 0001-01-01 counted as day 1 (chrono's "days from CE").
fn ce_1900() -> i64 {
    693_596
}

fn pow10(n: u8) -> u64 {
    10u64.pow(n as u32)
}

pub(crate) fn fmt_decimal(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let mut out = String::with_capacity(digits.len() + 3);
    if value < 0 {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digits);
        return out;
    }
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    out.push_str(int);
    out.push('.');
    out.push_str(frac);
    out
}

fn fmt_date(ce: i64) -> String {
    use chrono::Datelike;
    match chrono::NaiveDate::from_num_days_from_ce_opt(ce as i32) {
        Some(d) => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        None => format!("<día {ce}>"),
    }
}

/// `HH:MM:SS[.f…]` with exactly `scale` fractional digits.
fn fmt_time(increments: u64, scale: u8) -> String {
    let per_sec = pow10(scale);
    let secs = increments / per_sec;
    let frac = increments % per_sec;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if scale == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:0width$}", width = scale as usize)
    }
}

/// `datetime` counts 1/300 s: shown with 7 digits (100 ns), which round back
/// to the same tick.
fn fmt_datetime(d: DateTime) -> String {
    let ticks = (d.seconds_fragments() as u64 * 100_000 + 1) / 3;
    format!("{} {}", fmt_date(d.days() as i64 + ce_1900()), fmt_time(ticks, 7))
}

fn fmt_smalldatetime(d: SmallDateTime) -> String {
    let m = d.seconds_fragments() as u64;
    format!("{} {:02}:{:02}:00", fmt_date(d.days() as i64 + ce_1900()), m / 60, m % 60)
}

/// The local time and its offset (the wire carries UTC).
fn fmt_datetimeoffset(d: DateTimeOffset) -> String {
    let (dt, offset) = (d.datetime2(), d.offset());
    let scale = dt.time().scale();
    let per_day = 86_400 * pow10(scale) as i128;
    let utc = (dt.date().days() as i128 + 1) * per_day + dt.time().increments() as i128;
    let local = utc + offset as i128 * 60 * pow10(scale) as i128;
    let (days, ticks) = (local.div_euclid(per_day), local.rem_euclid(per_day));
    let sign = if offset < 0 { '-' } else { '+' };
    let off = offset.unsigned_abs();
    format!("{} {}{sign}{:02}:{:02}", fmt_date(days as i64), fmt_time(ticks as u64, scale), off / 60, off % 60)
}

/// A parsed date/time: days from CE, nanoseconds into the day, offset in
/// minutes when the text had one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Stamp {
    ce: i64,
    nanos: u64,
    offset: Option<i16>,
}

fn parse_uint(s: &str) -> Option<u64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `HH:MM[:SS[.fffffffff]]` → nanoseconds, and the rest of the text.
fn parse_clock(s: &str) -> Option<(u64, &str)> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.')).unwrap_or(s.len());
    let (clock, rest) = s.split_at(end);
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = hms.split(':');
    let h = parse_uint(parts.next()?)?;
    let m = parse_uint(parts.next()?)?;
    let sec = match parts.next() {
        Some(x) => parse_uint(x)?,
        None => 0,
    };
    if parts.next().is_some() || h > 23 || m > 59 || sec > 59 {
        return None;
    }
    let mut nanos = 0u64;
    if !frac.is_empty() {
        let digits = &frac[..frac.len().min(9)];
        nanos = parse_uint(digits)? * pow10(9 - digits.len() as u8);
    }
    Some(((h * 3600 + m * 60 + sec) * 1_000_000_000 + nanos, rest))
}

fn parse_offset(s: &str) -> Option<Option<i16>> {
    let s = s.trim();
    if s.is_empty() {
        return Some(None);
    }
    if s == "Z" || s == "z" {
        return Some(Some(0));
    }
    let (sign, rest) = match s.as_bytes()[0] {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (parse_uint(h)?, parse_uint(m)?),
        None if rest.len() == 4 => (parse_uint(&rest[..2])?, parse_uint(&rest[2..])?),
        None => (parse_uint(rest)?, 0),
    };
    if h > 14 || m > 59 {
        return None;
    }
    Some(Some(sign * (h * 60 + m) as i16))
}

/// `YYYY-MM-DD[( |T)HH:MM[:SS[.f…]]][ ][Z|±HH:MM]`, or a time alone.
pub(crate) fn parse_stamp(s: &str) -> Option<Stamp> {
    let s = s.trim();
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        let y = parse_uint(&s[..4])? as i32;
        let m = parse_uint(&s[5..7])? as u32;
        let d = parse_uint(&s[8..10])? as u32;
        use chrono::Datelike;
        let ce = chrono::NaiveDate::from_ymd_opt(y, m, d)?.num_days_from_ce() as i64;
        let rest = &s[10..];
        let rest = rest.strip_prefix(['T', 't', ' ']).unwrap_or(rest);
        if rest.trim().is_empty() {
            return Some(Stamp { ce, nanos: 0, offset: None });
        }
        let (nanos, rest) = parse_clock(rest)?;
        return Some(Stamp { ce, nanos, offset: parse_offset(rest)? });
    }
    let (nanos, rest) = parse_clock(s)?;
    Some(Stamp { ce: 1, nanos, offset: parse_offset(rest)? })
}

/// Nanoseconds rounded to `scale` digits (half up).
fn round_to_scale(nanos: u64, scale: u8) -> u64 {
    let div = pow10(9 - scale);
    (nanos + div / 2) / div
}

/// SQL Server's decimal precision (and largest scale).
const MAX_DECIMAL_DIGITS: usize = 38;

pub(crate) fn parse_decimal(s: &str) -> Option<Numeric> {
    let s = s.trim();
    let (neg, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let frac = frac.trim_end_matches('0');
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // At most 38 digits in all (and a scale of 38): extra fractional
    // digits round half up; the load rescales to the column anyway.
    let int = int.trim_start_matches('0');
    if int.len() > MAX_DECIMAL_DIGITS {
        return None;
    }
    let keep = frac.len().min(MAX_DECIMAL_DIGITS - int.len());
    let mut v: i128 = 0;
    for b in int.bytes().chain(frac[..keep].bytes()) {
        v = v.checked_mul(10)?.checked_add((b - b'0') as i128)?;
    }
    let mut scale = keep as u8;
    if frac.as_bytes().get(keep).is_some_and(|&b| b >= b'5') {
        v = v.checked_add(1)?;
        // A carry that adds a digit (0.99…95 -> 1.00…0): drop the zeros it left.
        while scale > 0 && v % 10 == 0 {
            v /= 10;
            scale -= 1;
        }
    }
    Some(Numeric::new_with_scale(if neg { -v } else { v }, scale))
}

// ------------------------------------------------------ values for the load

/// What a target column takes, from the type the server declared for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    U8,
    I16,
    I32,
    I64,
    Bit,
    F32,
    F64,
    Money,
    Decimal,
    Guid,
    Str,
    Bin,
    Xml,
    DateTime,
    SmallDateTime,
    Date,
    Time(u8),
    DateTime2(u8),
    Dto(u8),
}

pub(crate) fn kind_of(ty: &TypeInfo) -> std::result::Result<Kind, String> {
    use FixedLenType as F;
    use VarLenType as V;
    Ok(match ty {
        TypeInfo::FixedLen(f) => match f {
            F::Int1 => Kind::U8,
            F::Int2 => Kind::I16,
            F::Int4 => Kind::I32,
            F::Int8 => Kind::I64,
            F::Bit => Kind::Bit,
            F::Float4 => Kind::F32,
            F::Float8 => Kind::F64,
            F::Money | F::Money4 => Kind::Money,
            F::Datetime => Kind::DateTime,
            F::Datetime4 => Kind::SmallDateTime,
            other => return Err(format!("tipo {other:?} no admitido en la carga masiva")),
        },
        TypeInfo::VarLenSized(v) => match (v.r#type(), v.len()) {
            (V::Intn, 1) => Kind::U8,
            (V::Intn, 2) => Kind::I16,
            (V::Intn, 4) => Kind::I32,
            (V::Intn, _) => Kind::I64,
            (V::Bitn, _) => Kind::Bit,
            (V::Floatn, 4) => Kind::F32,
            (V::Floatn, _) => Kind::F64,
            (V::Money, _) => Kind::Money,
            (V::Datetimen, 4) => Kind::SmallDateTime,
            (V::Datetimen, _) => Kind::DateTime,
            (V::Daten, _) => Kind::Date,
            (V::Timen, s) => Kind::Time(s as u8),
            (V::Datetime2, s) => Kind::DateTime2(s as u8),
            (V::DatetimeOffsetn, s) => Kind::Dto(s as u8),
            (V::Guid, _) => Kind::Guid,
            (V::BigVarChar | V::BigChar | V::NVarchar | V::NChar | V::Text | V::NText, _) => Kind::Str,
            (V::BigVarBin | V::BigBinary | V::Image, _) => Kind::Bin,
            (V::Xml, _) => Kind::Xml,
            (other, _) => return Err(format!("tipo {other:?} no admitido en la carga masiva")),
        },
        TypeInfo::VarLenSizedPrecision { .. } => Kind::Decimal,
        TypeInfo::Xml { .. } => Kind::Xml,
        other => return Err(format!("tipo {other:?} no admitido en la carga masiva")),
    })
}

fn cell_text(c: Cell) -> String {
    match c {
        Cell::Text(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s,
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Cell::Bytes(b) => String::from_utf8(b).unwrap_or_else(|e| {
            let mut s = String::from("0x");
            for x in e.as_bytes() {
                s.push_str(&format!("{x:02X}"));
            }
            s
        }),
        Cell::Null => String::new(),
    }
}

fn int_of(c: Cell) -> std::result::Result<i128, String> {
    match c {
        Cell::Int(i) => Ok(i.into()),
        Cell::UInt(u) => Ok(u.into()),
        Cell::Bool(b) => Ok(b.into()),
        Cell::Float(f) if f.fract() == 0.0 && f.abs() < 1e19 => Ok(f as i128),
        other => {
            let s = cell_text(other);
            // Only whole numbers ("12", "-3", "7.000"): a fraction is refused,
            // never rounded into the integer column.
            let t = s.trim();
            let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
            let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
            let whole = !int.is_empty() && int.bytes().all(|b| b.is_ascii_digit()) && frac.bytes().all(|b| b == b'0');
            match parse_decimal(&s) {
                Some(n) if whole && n.scale() == 0 => Ok(n.value()),
                _ => Err(format!("«{s}» no es un entero")),
            }
        }
    }
}

fn float_of(c: Cell) -> std::result::Result<f64, String> {
    match c {
        Cell::Float(f) => Ok(f),
        Cell::Int(i) => Ok(i as f64),
        Cell::UInt(u) => Ok(u as f64),
        Cell::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
        other => {
            let s = cell_text(other);
            s.trim().parse().map_err(|_| format!("«{s}» no es un número"))
        }
    }
}

fn numeric_of(c: Cell) -> std::result::Result<Numeric, String> {
    match c {
        Cell::Int(i) => Ok(Numeric::new_with_scale(i.into(), 0)),
        Cell::UInt(u) => Ok(Numeric::new_with_scale(u.into(), 0)),
        Cell::Bool(b) => Ok(Numeric::new_with_scale(b.into(), 0)),
        Cell::Float(f) if !f.is_finite() => Err(format!("{f} no es un decimal")),
        other => {
            let s = cell_text(other);
            parse_decimal(&s).ok_or_else(|| format!("«{s}» no es un decimal"))
        }
    }
}

fn stamp_of(c: Cell) -> std::result::Result<Stamp, String> {
    let s = cell_text(c);
    parse_stamp(&s).ok_or_else(|| format!("«{s}» no es una fecha u hora"))
}

fn fits<T: TryFrom<i128>>(v: i128) -> std::result::Result<T, String> {
    T::try_from(v).map_err(|_| format!("{v} fuera de rango"))
}

/// Split a count of ticks from CE into (days from CE, ticks into the day).
fn split_day(total: i128, per_day: i128) -> (i64, u64) {
    (total.div_euclid(per_day) as i64, total.rem_euclid(per_day) as u64)
}

fn date_of(ce: i64) -> std::result::Result<Date, String> {
    if !(1..=3_652_059).contains(&ce) {
        return Err("fecha fuera de rango".into());
    }
    Ok(Date::new((ce - 1) as u32))
}

impl Kind {
    /// The value for this column; `Cell::Null` is NULL.
    pub(crate) fn data(self, c: Cell) -> std::result::Result<ColumnData<'static>, String> {
        let null = matches!(c, Cell::Null);
        macro_rules! val {
            ($variant:ident, $e:expr) => {
                ColumnData::$variant(if null { None } else { Some($e) })
            };
        }
        Ok(match self {
            Kind::U8 => val!(U8, fits(int_of(c)?)?),
            Kind::I16 => val!(I16, fits(int_of(c)?)?),
            Kind::I32 => val!(I32, fits(int_of(c)?)?),
            Kind::I64 => val!(I64, fits(int_of(c)?)?),
            Kind::Bit => val!(
                Bit,
                match c {
                    Cell::Bool(b) => b,
                    Cell::Text(s) if s.eq_ignore_ascii_case("true") => true,
                    Cell::Text(s) if s.eq_ignore_ascii_case("false") => false,
                    other => int_of(other)? != 0,
                }
            ),
            Kind::F32 => val!(F32, float_of(c)? as f32),
            Kind::F64 => val!(F64, float_of(c)?),
            Kind::Money | Kind::Decimal => val!(Numeric, numeric_of(c)?),
            Kind::Guid => val!(Guid, {
                let s = cell_text(c);
                tiberius::Uuid::parse_str(s.trim()).map_err(|_| format!("«{s}» no es un UUID"))?
            }),
            Kind::Str => val!(String, Cow::Owned(cell_text(c))),
            Kind::Xml => val!(Xml, Cow::Owned(XmlData::new(cell_text(c)))),
            Kind::Bin => val!(
                Binary,
                Cow::Owned(match c {
                    Cell::Bytes(b) => b,
                    other => cell_text(other).into_bytes(),
                })
            ),
            Kind::DateTime => val!(DateTime, {
                let st = stamp_of(c)?;
                // 1/300 s ticks, rounded to the nearest.
                let (ce, frag) = split_day(st.ce as i128 * 25_920_000 + ((st.nanos * 3 + 5_000_000) / 10_000_000) as i128, 25_920_000);
                DateTime::new(fits(ce as i128 - ce_1900() as i128)?, frag as u32)
            }),
            Kind::SmallDateTime => val!(SmallDateTime, {
                let st = stamp_of(c)?;
                let (ce, min) = split_day(st.ce as i128 * 1440 + ((st.nanos + 30_000_000_000) / 60_000_000_000) as i128, 1440);
                SmallDateTime::new(fits(ce as i128 - ce_1900() as i128)?, min as u16)
            }),
            Kind::Date => val!(Date, date_of(stamp_of(c)?.ce)?),
            Kind::Time(scale) => val!(Time, {
                let st = stamp_of(c)?;
                let max = 86_400 * pow10(scale) - 1;
                Time::new(round_to_scale(st.nanos, scale).min(max), scale)
            }),
            Kind::DateTime2(scale) => val!(DateTime2, {
                let st = stamp_of(c)?;
                let per_day = 86_400 * pow10(scale) as i128;
                let (ce, ticks) = split_day(st.ce as i128 * per_day + round_to_scale(st.nanos, scale) as i128, per_day);
                DateTime2::new(date_of(ce)?, Time::new(ticks, scale))
            }),
            Kind::Dto(scale) => val!(DateTimeOffset, {
                let st = stamp_of(c)?;
                let offset = st.offset.unwrap_or(0);
                let per_day = 86_400 * pow10(scale) as i128;
                let local = st.ce as i128 * per_day + round_to_scale(st.nanos, scale) as i128;
                let (ce, ticks) = split_day(local - offset as i128 * 60 * pow10(scale) as i128, per_day);
                DateTimeOffset::new(DateTime2::new(date_of(ce)?, Time::new(ticks, scale)), offset)
            }),
        })
    }
}

// ---------------------------------------------------------------- loading

/// A bulk load's target, described once.
pub(crate) struct Target {
    /// `[schema].[table]`.
    table: String,
    names: Vec<String>,
    /// Bracketed names (`bulk_insert_with_options`)…
    quoted: Vec<String>,
    /// …or the select list that declares wire types (`bulk_insert_with_select`).
    select: Option<String>,
    options: SqlBulkCopyOptions,
    kinds: Vec<Kind>,
    meta: RawMetadata,
    cat: Vec<CatColumn>,
}

pub(crate) async fn plan_target(s: &mut SqlServerSession, spec: &LoadSpec) -> Result<Target> {
    let cat = pick(&catalog(s, &spec.table).await?, Some(&spec.columns))?;
    for c in &cat {
        refuse_variant(c)?;
        if c.computed || c.rowversion() {
            return Err(Error::Query(format!("La columna «{}» del destino es calculada o rowversion: no se puede cargar", c.name)));
        }
        if c.identity && !spec.keep_identity {
            return Err(Error::Query(format!(
                "La columna «{}» del destino es de identidad: se carga solo conservando los valores de identidad",
                c.name
            )));
        }
    }
    // NULLs stay NULL (not the column default), as a copy must.
    let mut options = SqlBulkCopyOptions::from(SqlBulkCopyOption::KeepNulls);
    if spec.table_lock {
        options |= SqlBulkCopyOption::TableLock;
    }
    if spec.keep_identity {
        options |= SqlBulkCopyOption::KeepIdentity;
    }
    let table = table_name(&spec.table);
    let quoted: Vec<String> = cat.iter().map(|c| q(&c.name)).collect();
    let select = cat.iter().any(|c| wire_type(c).is_some()).then(|| cat.iter().map(declare_expr).collect::<Vec<_>>().join(", "));
    let meta = match &select {
        Some(list) => s.client.bulk_metadata_select(&table, list).await,
        None => {
            let refs: Vec<&str> = quoted.iter().map(String::as_str).collect();
            s.client.bulk_metadata(&table, &refs, options).await
        }
    }
    .map_err(err)?;
    if meta.columns().len() != cat.len() {
        return Err(Error::Query(format!(
            "El destino declara {} columnas para la carga y se pidieron {}",
            meta.columns().len(),
            cat.len()
        )));
    }
    let kinds = meta
        .columns()
        .iter()
        .zip(&cat)
        .map(|(m, c)| kind_of(&m.base.ty).map_err(|e| Error::Unsupported(format!("columna «{}»: {e}", c.name))))
        .collect::<Result<Vec<_>>>()?;
    Ok(Target { table, names: cat.iter().map(|c| c.name.clone()).collect(), quoted, select, options, kinds, meta, cat })
}

/// Rows on their way into a load.
pub(crate) enum Chunk {
    /// Complete ROW tokens, back to back, matching the target's metadata.
    Raw { bytes: Vec<u8>, rows: u64 },
    Cells(RowBatch),
}

#[async_trait]
pub(crate) trait Chunks: Send {
    /// The next non-empty chunk, `None` at the end.
    async fn next(&mut self) -> Result<Option<Chunk>>;
}

type Tds = Compat<TcpStream>;

async fn send_chunk(req: &mut BulkLoadRequest<'_, Tds>, target: &Target, chunk: Chunk) -> Result<(u64, u64)> {
    match chunk {
        Chunk::Raw { bytes, rows } => {
            req.send_raw_rows(&bytes).await.map_err(err)?;
            Ok((rows, bytes.len() as u64))
        }
        Chunk::Cells(batch) => {
            let (n, size) = (batch.rows.len() as u64, batch.bytes as u64);
            let width = target.kinds.len();
            for row in batch.rows {
                if row.len() != width {
                    return Err(Error::Query(format!("Una fila trae {} valores y la carga tiene {width} columnas", row.len())));
                }
                let mut tr = TokenRow::with_capacity(width);
                for (i, cell) in row.into_iter().enumerate() {
                    tr.push(target.kinds[i].data(cell).map_err(|e| Error::Query(format!("Columna «{}»: {e}", target.names[i])))?);
                }
                req.send(tr).await.map_err(err)?;
            }
            Ok((n, size))
        }
    }
}

/// Load `src` into `target`, one `INSERT BULK` per commit window (it commits
/// on `finalize`). A window closes on a chunk boundary.
async fn load(s: &mut SqlServerSession, target: &Target, spec: &LoadSpec, src: &mut dyn Chunks, progress: Progress<'_>) -> Result<u64> {
    let refs: Vec<&str> = target.quoted.iter().map(String::as_str).collect();
    let (max_rows, max_bytes) = (spec.commit_rows.max(1), spec.commit_bytes.max(1));
    let mut committed = 0u64;
    let mut next = src.next().await?;
    while next.is_some() {
        let mut req = match &target.select {
            Some(list) => s.client.bulk_insert_with_select(&target.table, list, target.options, &[]).await,
            None => s.client.bulk_insert_with_options(&target.table, &refs, target.options, &[]).await,
        }
        .map_err(err)?;
        let (mut rows, mut bytes) = (0u64, 0u64);
        while let Some(chunk) = next.take() {
            let (r, b) = send_chunk(&mut req, target, chunk).await?;
            rows += r;
            bytes += b;
            next = src.next().await?;
            if rows >= max_rows || bytes >= max_bytes {
                break;
            }
        }
        req.finalize().await.map_err(err)?;
        committed += rows;
        progress(committed);
    }
    Ok(committed)
}

/// Batches from the caller.
struct FromSource<'a>(&'a mut dyn BatchSource);

#[async_trait]
impl Chunks for FromSource<'_> {
    async fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            match self.0.next().await {
                Some(b) if b.is_empty() => continue,
                other => return Ok(other.map(Chunk::Cells)),
            }
        }
    }
}

pub(crate) async fn bulk_load(
    s: &mut SqlServerSession,
    spec: &LoadSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    if !columns.is_empty() && columns.len() != spec.columns.len() {
        return Err(Error::Query(format!("Se leen {} columnas y se cargan {}", columns.len(), spec.columns.len())));
    }
    let target = plan_target(s, spec).await?;
    let res = load(s, &target, spec, &mut FromSource(source), progress).await;
    if res.is_err() {
        // A load cut short leaves the connection mid-message; the server
        // rolls the open window back when it closes.
        if let Err(re) = s.reconnect().await {
            tracing::debug!("sqlserver: reconnect after a failed load: {re}");
        }
    }
    res
}

// ------------------------------------------------------------ native copy

/// Raw rows from the source, in chunks of [`CHUNK_ROWS`] / [`CHUNK_BYTES`].
struct RawChunks<'a> {
    stream: tiberius::RawRowStream<'a>,
    done: bool,
}

#[async_trait]
impl Chunks for RawChunks<'_> {
    async fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(64 * 1024);
        let mut rows = 0u64;
        while let Some(item) = self.stream.try_next().await.map_err(err)? {
            if let RawItem::Row(r) = item {
                bytes.extend_from_slice(&r);
                rows += 1;
                if rows as usize >= CHUNK_ROWS || bytes.len() >= CHUNK_BYTES {
                    return Ok(Some(Chunk::Raw { bytes, rows }));
                }
            }
        }
        self.done = true;
        Ok((rows > 0).then_some(Chunk::Raw { bytes, rows }))
    }
}

/// Decoded rows from the source, as cells.
struct TypedChunks<'a> {
    stream: tiberius::QueryStream<'a>,
    done: bool,
}

#[async_trait]
impl Chunks for TypedChunks<'_> {
    async fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        let mut batch = RowBatch { rows: Vec::with_capacity(CHUNK_ROWS), bytes: 0 };
        while let Some(item) = self.stream.try_next().await.map_err(err)? {
            if let QueryItem::Row(row) = item {
                let cells = row.into_iter().map(to_cell).collect::<std::result::Result<Vec<_>, _>>().map_err(Error::Query)?;
                batch.bytes += cells.iter().map(Cell::size).sum::<usize>();
                batch.rows.push(cells);
                if batch.rows.len() >= CHUNK_ROWS || batch.bytes >= CHUNK_BYTES {
                    return Ok(Some(Chunk::Cells(batch)));
                }
            }
        }
        self.done = true;
        Ok((!batch.is_empty()).then_some(Chunk::Cells(batch)))
    }
}

/// Chunks read ahead by a concurrent reader, through a bounded channel.
struct Piped(tokio::sync::mpsc::Receiver<Result<Chunk>>);

#[async_trait]
impl Chunks for Piped {
    async fn next(&mut self) -> Result<Option<Chunk>> {
        self.0.recv().await.transpose()
    }
}

/// How many chunks the reader may be ahead of the load (~32 MiB at most).
const CHUNKS_IN_FLIGHT: usize = 16;

/// Read `src` and load into `target` at the same time: the source server
/// sends while the target one inserts.
async fn pipe(dst: &mut SqlServerSession, target: &Target, spec: &LoadSpec, src: &mut dyn Chunks, progress: Progress<'_>) -> Result<u64> {
    read_ahead(src, move |mut piped| async move { load(dst, target, spec, &mut piped, progress).await }).await
}

/// Run `consume` over `src` read ahead by a concurrent reader. `consume`
/// owns the receiving end, so when it returns (an error included) the
/// channel closes and the reader stops instead of blocking on a full
/// channel forever.
async fn read_ahead<F, Fut>(src: &mut dyn Chunks, consume: F) -> Result<u64>
where
    F: FnOnce(Piped) -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
{
    let (tx, rx) = tokio::sync::mpsc::channel(CHUNKS_IN_FLIGHT);
    let reader = async move {
        loop {
            let item = src.next().await.transpose();
            let stop = !matches!(item, Some(Ok(_)));
            // A closed channel: the load failed and stopped reading.
            if let Some(item) = item {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
            if stop {
                break;
            }
        }
    };
    let (_, res) = futures::join!(reader, consume(Piped(rx)));
    res
}

/// The source's raw select list when its rows fit the target's bulk
/// declaration byte for byte; `Err` says why not.
async fn raw_plan(src: &mut SqlServerSession, spec: &CopySpec, cols: &[CatColumn], target: &Target) -> Result<std::result::Result<Vec<String>, String>> {
    if std::env::var_os(NO_RAW_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        return Ok(Err(format!("{NO_RAW_ENV} está activo")));
    }
    let list = cols.iter().zip(&target.cat).map(|(s, t)| raw_expr(s, t)).collect::<Result<Vec<_>>>()?;
    // The source's wire types, without reading a row.
    let probe = format!("SELECT TOP 0 {} FROM {}", list.join(", "), table_name(&spec.source.table));
    let mut meta = None;
    {
        let mut stream = src.client.query_raw_rows(probe).await.map_err(err)?;
        while let Some(item) = stream.try_next().await.map_err(err)? {
            if let (RawItem::Metadata(m), None) = (item, &meta) {
                meta = Some(m);
            }
        }
    }
    let Some(meta) = meta else {
        return Ok(Err("el origen no describió sus columnas".into()));
    };
    Ok(meta.check_compatible(&target.meta).map(|()| list))
}

pub(crate) async fn copy(src: &mut SqlServerSession, dst: &mut SqlServerSession, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let cols = pick(&catalog(src, &spec.source.table).await?, spec.source.columns.as_deref())?;
    if cols.len() != spec.target.columns.len() {
        return Err(Error::Query(format!("Se leen {} columnas y se cargan {}", cols.len(), spec.target.columns.len())));
    }
    let target = plan_target(dst, &spec.target).await?;
    let raw = raw_plan(src, spec, &cols, &target).await?;
    let filter = spec.source.filter.as_deref();
    let table = table_name(&spec.target.table);
    let res = match raw {
        Ok(list) => {
            tracing::info!(table = %table, mode = "raw", "sqlserver native copy: raw TDS rows");
            // The source only runs this SELECT.
            let stream = src.client.query_raw_rows(select_sql(&list, &spec.source.table, filter)).await.map_err(err);
            match stream {
                Ok(stream) => pipe(dst, &target, &spec.target, &mut RawChunks { stream, done: false }, progress).await,
                Err(e) => Err(e),
            }
        }
        Err(why) => {
            tracing::info!(table = %table, mode = "decoded", reason = %why, "sqlserver native copy: decoded rows");
            let list = cols.iter().map(read_expr).collect::<Result<Vec<_>>>()?;
            let stream = src.client.simple_query(select_sql(&list, &spec.source.table, filter)).await.map_err(err);
            match stream {
                Ok(stream) => pipe(dst, &target, &spec.target, &mut TypedChunks { stream, done: false }, progress).await,
                Err(e) => Err(e),
            }
        }
    };
    if res.is_err() {
        // Both connections may be mid-message.
        for s in [src, dst] {
            if let Err(re) = s.reconnect().await {
                tracing::debug!("sqlserver: reconnect after a failed copy: {re}");
            }
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiberius::VarLenContext;

    fn col(name: &str, ty: &str, nullable: bool) -> CatColumn {
        CatColumn {
            name: name.into(),
            ty: ty.into(),
            max_len: 0,
            precision: 0,
            scale: 0,
            nullable,
            computed: false,
            identity: false,
            clr: matches!(ty, "geography" | "geometry" | "hierarchyid"),
        }
    }

    #[test]
    fn default_columns_skip_computed_and_rowversion() {
        let mut calc = col("c", "int", true);
        calc.computed = true;
        let cat = vec![col("a", "int", false), calc, col("rv", "timestamp", false), col("b", "nvarchar", true)];
        let names = |v: Vec<CatColumn>| v.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(pick(&cat, None).unwrap()), ["a", "b"]);
        // Asked for explicitly: kept.
        assert_eq!(names(pick(&cat, Some(&["RV".into(), "c".into()])).unwrap()), ["rv", "c"]);
        assert!(pick(&cat, Some(&["zz".into()])).is_err());
    }

    #[test]
    fn select_lists() {
        assert_eq!(read_expr(&col("m", "money", true)).unwrap(), "CAST([m] AS decimal(19,4)) AS [m]");
        assert_eq!(read_expr(&col("g", "geography", true)).unwrap(), "CAST([g] AS varbinary(max)) AS [g]");
        assert_eq!(read_expr(&col("a]b", "int", true)).unwrap(), "[a]]b]");
        assert!(matches!(read_expr(&col("v", "sql_variant", true)), Err(Error::Unsupported(_))));

        assert_eq!(declare_expr(&col("x", "xml", true)), "CAST(NULL AS nvarchar(max)) AS [x]");
        assert_eq!(declare_expr(&col("x", "xml", false)), "ISNULL(CAST(NULL AS nvarchar(max)), N'') AS [x]");
        assert_eq!(declare_expr(&col("t", "text", false)), "ISNULL(CAST(NULL AS varchar(max)), '') AS [t]");
        assert_eq!(declare_expr(&col("i", "image", true)), "CAST(NULL AS varbinary(max)) AS [i]");
        assert_eq!(declare_expr(&col("h", "hierarchyid", true)), "CAST(NULL AS varbinary(max)) AS [h]");
        assert_eq!(declare_expr(&col("s", "smalldatetime", false)), "ISNULL(CAST(NULL AS datetime), CAST(0 AS datetime)) AS [s]");
        assert_eq!(declare_expr(&col("n", "nvarchar", false)), "[n]");

        // Raw: NOT NULL only when both sides are.
        assert_eq!(raw_expr(&col("s", "smalldatetime", false), &col("s", "smalldatetime", false)).unwrap(), "ISNULL(CAST([s] AS datetime), CAST(0 AS datetime)) AS [s]");
        assert_eq!(raw_expr(&col("s", "smalldatetime", true), &col("s", "smalldatetime", false)).unwrap(), "CAST([s] AS datetime) AS [s]");
        assert_eq!(raw_expr(&col("x", "nvarchar", true), &col("x", "xml", true)).unwrap(), "CAST([x] AS nvarchar(max)) AS [x]");
        assert_eq!(raw_expr(&col("i", "int", true), &col("i", "int", true)).unwrap(), "[i]");

        let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() };
        assert_eq!(select_sql(&["[a]".into(), "[b]".into()], &t, Some(" id > 5 ")), "SELECT [a], [b] FROM [dbo].[t] WHERE id > 5");
        assert_eq!(select_sql(&["[a]".into()], &t, Some("")), "SELECT [a] FROM [dbo].[t]");
    }

    #[test]
    fn decimals_are_exact() {
        assert_eq!(fmt_decimal(123_400, 4), "12.3400");
        assert_eq!(fmt_decimal(-5, 1), "-0.5");
        assert_eq!(fmt_decimal(-5, 3), "-0.005");
        assert_eq!(fmt_decimal(0, 2), "0.00");
        assert_eq!(fmt_decimal(42, 0), "42");
        // money extremes as decimal(19,4)
        assert_eq!(fmt_decimal(-9_223_372_036_854_775_808, 4), "-922337203685477.5808");
        let n = parse_decimal("-922337203685477.5808").unwrap();
        assert_eq!((n.value(), n.scale()), (-9_223_372_036_854_775_808, 4));
        let n = parse_decimal("12.3400").unwrap();
        assert_eq!((n.value(), n.scale()), (1234, 2));
        let n = parse_decimal("99999999999999999999999999999999999999").unwrap();
        assert_eq!(n.scale(), 0);
        assert!(parse_decimal("1e5").is_none());
        assert!(parse_decimal("").is_none());
        assert!(parse_decimal(".").is_none());

        // decimal(38,38): 38 fractional digits, as fmt_decimal writes them.
        let max = 99_999_999_999_999_999_999_999_999_999_999_999_999i128;
        let n = parse_decimal(&fmt_decimal(max, 38)).unwrap();
        assert_eq!((n.value(), n.scale()), (max, 38));
        let n = parse_decimal("-0.12345678901234567890123456789012345678").unwrap();
        assert_eq!((n.value(), n.scale()), (-12_345_678_901_234_567_890_123_456_789_012_345_678, 38));
        // More digits than fit round half up instead of failing.
        let n = parse_decimal("0.123456789012345678901234567890123456785").unwrap();
        assert_eq!((n.value(), n.scale()), (12_345_678_901_234_567_890_123_456_789_012_345_679, 38));
        let n = parse_decimal("0.123456789012345678901234567890123456784999").unwrap();
        assert_eq!((n.value(), n.scale()), (12_345_678_901_234_567_890_123_456_789_012_345_678, 38));
        let n = parse_decimal("123456789012345678901234567890.1234567891").unwrap();
        assert_eq!((n.value(), n.scale()), (12_345_678_901_234_567_890_123_456_789_012_345_679, 8));
        let n = parse_decimal("-0.999999999999999999999999999999999999995").unwrap();
        assert_eq!((n.value(), n.scale()), (-1, 0));
        assert!(parse_decimal("123456789012345678901234567890123456789").is_none());
        let n = parse_decimal("000000000000000000000000000000000000000001.5").unwrap();
        assert_eq!((n.value(), n.scale()), (15, 1));
    }

    /// Endless chunks; counts how many were read.
    struct Endless(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Chunks for Endless {
        async fn next(&mut self) -> Result<Option<Chunk>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok(Some(Chunk::Raw { bytes: vec![0], rows: 1 }))
        }
    }

    #[tokio::test]
    async fn failed_load_stops_the_reader() {
        let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut src = Endless(read.clone());
        let run = read_ahead(&mut src, |mut piped| async move {
            piped.next().await?;
            Err(Error::Query("PK violation".into()))
        });
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), run).await.expect("the copy hung after the load failed");
        assert!(matches!(res, Err(Error::Query(m)) if m == "PK violation"));
        assert!(read.load(std::sync::atomic::Ordering::Relaxed) <= CHUNKS_IN_FLIGHT + 3);
    }

    fn roundtrip(kind: Kind, value: ColumnData<'static>) -> Cell {
        let cell = to_cell(value.clone()).unwrap();
        let back = kind.data(cell.clone()).unwrap();
        assert_eq!(format!("{back:?}"), format!("{value:?}"), "{cell:?}");
        cell
    }

    #[test]
    fn values_round_trip() {
        assert_eq!(roundtrip(Kind::U8, ColumnData::U8(Some(255))), Cell::Int(255));
        assert_eq!(roundtrip(Kind::I16, ColumnData::I16(Some(-32768))), Cell::Int(-32768));
        assert_eq!(roundtrip(Kind::I64, ColumnData::I64(Some(i64::MIN))), Cell::Int(i64::MIN));
        assert_eq!(roundtrip(Kind::Bit, ColumnData::Bit(Some(true))), Cell::Bool(true));
        assert_eq!(roundtrip(Kind::F32, ColumnData::F32(Some(1.1))), Cell::Float(1.1f32 as f64));
        assert_eq!(roundtrip(Kind::F64, ColumnData::F64(Some(f64::MAX))), Cell::Float(f64::MAX));
        roundtrip(Kind::Decimal, ColumnData::Numeric(Some(Numeric::new_with_scale(-12345, 3))));
        assert_eq!(roundtrip(Kind::I32, ColumnData::I32(None)), Cell::Null);
        assert_eq!(roundtrip(Kind::Str, ColumnData::String(Some("ñandú".into()))), Cell::Text("ñandú".into()));
        let big = vec![7u8; 5 * 1024 * 1024];
        assert_eq!(roundtrip(Kind::Bin, ColumnData::Binary(Some(big.clone().into()))), Cell::Bytes(big));
        let g = tiberius::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(roundtrip(Kind::Guid, ColumnData::Guid(Some(g))), Cell::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()));

        // Temporal values, full precision.
        let ce2024 = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        use chrono::Datelike;
        let d0001 = (ce2024.num_days_from_ce() - 1) as u32;
        assert_eq!(roundtrip(Kind::Date, ColumnData::Date(Some(Date::new(d0001)))), Cell::Date("2024-02-29".into()));
        assert_eq!(roundtrip(Kind::Date, ColumnData::Date(Some(Date::new(0)))), Cell::Date("0001-01-01".into()));
        assert_eq!(roundtrip(Kind::Time(7), ColumnData::Time(Some(Time::new(863_999_999_999, 7)))), Cell::Time("23:59:59.9999999".into()));
        assert_eq!(roundtrip(Kind::Time(0), ColumnData::Time(Some(Time::new(3661, 0)))), Cell::Time("01:01:01".into()));
        assert_eq!(
            roundtrip(Kind::DateTime2(3), ColumnData::DateTime2(Some(DateTime2::new(Date::new(d0001), Time::new(45_296_789, 3))))),
            Cell::DateTime("2024-02-29 12:34:56.789".into())
        );
        // datetime: 1/300 s ticks; the last tick of 9999-12-31 and pre-1900.
        assert_eq!(roundtrip(Kind::DateTime, ColumnData::DateTime(Some(DateTime::new(0, 1)))), Cell::DateTime("1900-01-01 00:00:00.0033333".into()));
        roundtrip(Kind::DateTime, ColumnData::DateTime(Some(DateTime::new(2_958_463, 25_919_999))));
        assert_eq!(roundtrip(Kind::DateTime, ColumnData::DateTime(Some(DateTime::new(-53_690, 0)))), Cell::DateTime("1753-01-01 00:00:00.0000000".into()));
        assert_eq!(roundtrip(Kind::SmallDateTime, ColumnData::SmallDateTime(Some(SmallDateTime::new(1, 61)))), Cell::DateTime("1900-01-02 01:01:00".into()));
        // datetimeoffset: UTC on the wire, local text.
        let dto = DateTimeOffset::new(DateTime2::new(Date::new(d0001), Time::new(1_000, 3)), -180);
        assert_eq!(roundtrip(Kind::Dto(3), ColumnData::DateTimeOffset(Some(dto))), Cell::DateTimeTz("2024-02-28 21:00:01.000-03:00".into()));
        let dto = DateTimeOffset::new(DateTime2::new(Date::new(d0001), Time::new(863_999_999_999, 7)), 840);
        assert_eq!(roundtrip(Kind::Dto(7), ColumnData::DateTimeOffset(Some(dto))), Cell::DateTimeTz("2024-03-01 13:59:59.9999999+14:00".into()));
    }

    #[test]
    fn values_from_other_engines() {
        let d = |k: Kind, c: Cell| format!("{:?}", k.data(c).unwrap());
        assert_eq!(d(Kind::I32, Cell::Text("42".into())), "I32(Some(42))");
        assert!(Kind::U8.data(Cell::Int(300)).is_err());
        assert_eq!(d(Kind::Bit, Cell::Int(0)), "Bit(Some(false))");
        assert_eq!(d(Kind::Str, Cell::Int(7)), d(Kind::Str, Cell::Text("7".into())));
        assert_eq!(d(Kind::Money, Cell::Float(1.5)), format!("{:?}", ColumnData::Numeric(Some(Numeric::new_with_scale(15, 1)))));
        // ISO with a T and a Z; more digits than the scale round half up.
        assert_eq!(
            d(Kind::DateTime2(2), Cell::Text("2024-01-01T23:59:59.995Z".into())),
            d(Kind::DateTime2(2), Cell::DateTime("2024-01-02 00:00:00".into()))
        );
        assert_eq!(d(Kind::Dto(0), Cell::DateTime("2024-01-01 00:00:00".into())), d(Kind::Dto(0), Cell::DateTimeTz("2024-01-01 00:00:00+00:00".into())));
        assert_eq!(d(Kind::Date, Cell::DateTimeTz("2024-05-06 10:00:00 -03:00".into())), d(Kind::Date, Cell::Date("2024-05-06".into())));
        assert!(Kind::Date.data(Cell::Text("mañana".into())).is_err());
        assert_eq!(d(Kind::Time(3), Cell::Time("10:20".into())), format!("{:?}", ColumnData::Time(Some(Time::new(37_200_000, 3)))));
    }

    #[test]
    fn stamps() {
        let s = parse_stamp("2024-02-29 12:34:56.123456789+05:30").unwrap();
        assert_eq!((s.nanos, s.offset), (45_296_123_456_789, Some(330)));
        assert_eq!(parse_stamp("2024-02-29").unwrap().nanos, 0);
        assert_eq!(parse_stamp("23:00:00").unwrap().nanos, 82_800_000_000_000);
        assert!(parse_stamp("2024-02-30").is_none());
        assert!(parse_stamp("25:00:00").is_none());
    }

    #[test]
    fn kinds_from_wire_types() {
        let v = |t, n| TypeInfo::VarLenSized(VarLenContext::new(t, n, None));
        assert_eq!(kind_of(&TypeInfo::FixedLen(FixedLenType::Int4)).unwrap(), Kind::I32);
        assert_eq!(kind_of(&v(VarLenType::Intn, 2)).unwrap(), Kind::I16);
        assert_eq!(kind_of(&v(VarLenType::Datetimen, 4)).unwrap(), Kind::SmallDateTime);
        assert_eq!(kind_of(&v(VarLenType::DatetimeOffsetn, 3)).unwrap(), Kind::Dto(3));
        assert_eq!(kind_of(&v(VarLenType::NVarchar, 0xffff)).unwrap(), Kind::Str);
        assert_eq!(kind_of(&TypeInfo::FixedLen(FixedLenType::Money4)).unwrap(), Kind::Money);
        assert!(kind_of(&v(VarLenType::SSVariant, 8016)).is_err());
    }
}

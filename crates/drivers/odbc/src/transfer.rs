//! Bulk transfer (see `dbine_driver::transfer`) for every ODBC preset.
//!
//! - **Read** ([`read`]): one `SELECT <columns> FROM <table> [WHERE (…)]`
//!   with the spec's columns in its order. Each column is read with the C
//!   type of its SQL type, so cells come out typed and lossless:
//!   integers as `Int` (`UInt` for unsigned `BIGINT`), floats as `Float`,
//!   `BIT` as `Bool`, `DECIMAL`/`NUMERIC` as the driver's exact text
//!   (`Decimal`), `DATE` as `Date`, `TIMESTAMP` from the timestamp struct
//!   with its nanoseconds (`DateTime`), `TIME` as text (`Time`, keeps the
//!   fraction the struct would drop), offsets as `DateTimeTz`, GUIDs as
//!   `Uuid`, JSON and Hive/Spark complex types as `Json`, binaries whole
//!   (SQL Server's CLR types too: `hierarchyid`, `geography`, `geometry`).
//!   When every column has a bounded size, the columns are bound to
//!   column-wise buffers and fetched in blocks (`SQL_ATTR_ROW_ARRAY_SIZE`,
//!   up to [`CHUNK_ROWS`] rows and [`FETCH_BYTES`] of buffers per fetch).
//!   A result with a long column (`LONGVARCHAR`, `varbinary(max)`, sizes
//!   the driver doesn't give) is fetched row by row, each cell with
//!   `SQLGetData` in chunks until its end: text and binaries arrive whole.
//!   Hive, Impala, Spark, Kyuubi and Cloudera always go row by row: their
//!   drivers report `STRING` with a nominal length shorter than the data.
//! - **Load** ([`bulk_load`]): one prepared `INSERT … VALUES (?, …)` whose
//!   parameters take the target columns' SQL types (from describing
//!   `SELECT <columns> FROM <table> WHERE 1=0`), sent as parameter arrays
//!   (`SQL_ATTR_PARAMSET_SIZE`): as many rows per execution as fit in
//!   [`PARAM_BYTES`] of parameter buffers (at least one). Integers, floats,
//!   bits and binaries are bound natively; decimals, dates, times and text
//!   as UTF-16 text the driver converts to the target type, so no precision
//!   is lost on the way ([`declared`]: narrow text columns get non-ASCII
//!   values declared wide, long values the long types; fractions longer
//!   than the column's are cut to it, [`trim_fraction`]). A driver that refuses parameter arrays gets the
//!   same prepared statement executed once per row. Where the driver has
//!   transactions (`SQL_TXN_CAPABLE`), autocommit goes off and every
//!   commit window of the spec (rows or bytes) is one transaction; a
//!   failure rolls back only the open window. Without transactions every
//!   execution commits by itself and progress is reported per execution
//!   (with the rows of a failed parameter array that went in). One batch
//!   is in the loading thread at a time, and a dropped load commits
//!   nothing more ([`Gate`]).
//!   `keep_identity` turns `IDENTITY_INSERT` on for Sybase ASE and SQL
//!   Server (generic preset) when the target has an identity column; the
//!   other engines take explicit values in their identity columns as is
//!   (Db2's `GENERATED ALWAYS` refuses them). `table_lock` has no ODBC
//!   equivalent and is ignored.
//! - Hive, Impala, Spark, Kyuubi and Cloudera have no bulk load here
//!   ([`supports_bulk_load`]): each `INSERT` is a job that writes a file,
//!   so the migration's multi-row `INSERT` script is the better path, and
//!   their real bulk path (files to HDFS/S3 + `LOAD DATA`) isn't reachable
//!   through ODBC. NetSuite (SuiteAnalytics Connect) is read-only.

use crate::design::{eng, Eng};
use crate::ffi::*;
use crate::odbc::{diags, fmt_date, fmt_timestamp, query_error, wide, Conn, Stmt, StmtSlot};
use crate::presets::Preset;
use crate::OdbcSession;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{
    BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn, CHUNK_ROWS,
};
use dbine_driver::{Error, Result};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

/// Column buffers of one block fetch, at most.
pub const FETCH_BYTES: usize = 2 * 1024 * 1024;
/// Parameter buffers of one execution, at most (a single larger row still
/// goes, alone).
pub const PARAM_BYTES: usize = 2 * 1024 * 1024;
/// Text columns up to this many characters are bound; longer ones are read
/// with `SQLGetData`.
const MAX_BOUND_CHARS: usize = 4_000;
/// Same for binaries, in bytes.
const MAX_BOUND_BYTES: usize = 8_000;
/// Chunk of a long cell read with `SQLGetData`.
const LONG_CHUNK: usize = 64 * 1024;
/// Longest text a plain (`WCHAR`/`WVARCHAR`) parameter is declared with;
/// longer ones go as `WLONGVARCHAR` (SQL Server refuses more).
const MAX_WIDE_PARAM: usize = 4_000;
/// Same for narrow text and binaries, in bytes.
const MAX_NARROW_PARAM: usize = 8_000;

// ODBC values not in ffi.rs.
const SQL_C_SBIGINT: i16 = -25;
const SQL_C_UBIGINT: i16 = -27;
const SQL_TIME_V2: i16 = 10;
const SQL_TYPE_TIME: i16 = 92;
const SQL_TYPE_TIMESTAMP_TZ: i16 = 95;
const SQL_GUID: i16 = -11;
const SQL_SS_TIME2: i16 = -154;
const SQL_SS_TIMESTAMPOFFSET: i16 = -155;
/// SQL Server CLR types: `hierarchyid`, `geography`, `geometry`.
const SQL_SS_UDT: i16 = -151;
const SQL_DESC_UNSIGNED: u16 = 8;
const SQL_DESC_AUTO_UNIQUE_VALUE: u16 = 11;
const SQL_ATTR_PARAM_STATUS_PTR: i32 = 20;
const SQL_ATTR_PARAMSET_SIZE: i32 = 22;
const SQL_ATTR_ROWS_FETCHED_PTR: i32 = 26;
const SQL_ATTR_ROW_ARRAY_SIZE: i32 = 27;
const SQL_ATTR_AUTOCOMMIT: i32 = 102;
const SQL_IS_UINTEGER: i32 = -1;
const SQL_IS_POINTER: i32 = -4;
const SQL_TXN_CAPABLE: u16 = 46;
const SQL_COMMIT: i16 = 0;
const SQL_ROLLBACK: i16 = 1;
const SQL_UNBIND: u16 = 2;
const SQL_RESET_PARAMS: u16 = 3;
const SQL_PARAM_SUCCESS: u16 = 0;
const SQL_PARAM_ERROR: u16 = 5;
const SQL_PARAM_SUCCESS_WITH_INFO: u16 = 6;
const SQL_PARAM_UNUSED: u16 = 7;
const SQL_NEED_DATA: SqlReturn = 99;

fn ok(rc: SqlReturn) -> bool {
    rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO
}

fn simba(p: &Preset) -> bool {
    matches!(eng(p), Eng::Hive | Eng::Impala | Eng::Spark)
}

/// The preset's sessions implement `bulk_load` (see the module docs).
pub fn supports_bulk_load(p: &Preset) -> bool {
    !simba(p) && eng(p) != Eng::NetSuite
}

fn no_bulk_load(p: &Preset) -> Error {
    Error::Unsupported(if eng(p) == Eng::NetSuite {
        "SuiteAnalytics Connect es de solo lectura: no admite cargas".into()
    } else {
        format!(
            "{} no tiene carga masiva por ODBC: cada INSERT es un trabajo que escribe un archivo; la migración usa INSERT de varias filas",
            p.name
        )
    })
}

/// The driver reports the real maximum length of text columns (Simba's
/// Hive/Impala/Spark drivers give a nominal one for `STRING`).
pub fn sizes_reliable(p: &Preset) -> bool {
    !simba(p)
}

fn column_list(quote: Quote, cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident(quote, c)).collect::<Vec<_>>().join(", ")
}

/// The read's `SELECT`: the spec's columns in its order (all of them when
/// `None`) and its filter.
pub fn select_sql(quote: Quote, spec: &ReadSpec) -> String {
    let cols = spec.columns.as_deref().map_or_else(|| "*".to_string(), |c| column_list(quote, c));
    let table = qualified_name(quote, spec.table.schema(), &spec.table.name);
    match spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("SELECT {cols} FROM {table} WHERE ({f})"),
        None => format!("SELECT {cols} FROM {table}"),
    }
}

// ---------------------------------------------------------------- columns

/// A result column, as `SQLDescribeCol` / `SQLColAttribute` give it.
#[derive(Debug, Clone, Default)]
pub struct Col {
    pub name: String,
    pub sql_type: i16,
    /// Column size (characters, bytes or precision); 0 when unknown.
    pub size: usize,
    pub digits: i16,
    pub nullable: bool,
    pub type_name: String,
    pub unsigned: bool,
    /// Identity / auto-increment.
    pub auto: bool,
}

fn describe(api: &Api, st: &Stmt) -> Result<Vec<Col>> {
    let h = st.raw();
    let n = st.num_cols()?;
    let mut out = Vec::with_capacity(n as usize);
    for col in 1..=n {
        let mut name = vec![0u16; 512];
        let (mut name_len, mut sql_type, mut digits, mut nullable) = (0i16, 0i16, 0i16, 0i16);
        let mut size: SqlULen = 0;
        // SAFETY: buffers sized as declared.
        let rc = unsafe {
            (api.SQLDescribeColW)(
                h,
                col,
                name.as_mut_ptr(),
                name.len() as i16,
                &mut name_len,
                &mut sql_type,
                &mut size,
                &mut digits,
                &mut nullable,
            )
        };
        if !ok(rc) {
            return Err(st.err());
        }
        let numeric = |field: u16| -> SqlLen {
            let mut v: SqlLen = 0;
            // SAFETY: numeric attribute, no character buffer.
            let rc = unsafe { (api.SQLColAttributeW)(h, col, field, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut v) };
            if ok(rc) {
                v
            } else {
                0
            }
        };
        let mut tbuf = vec![0u16; 256];
        let mut tlen = 0i16;
        let mut unused: SqlLen = 0;
        // SAFETY: buffer length in bytes.
        let rc = unsafe {
            (api.SQLColAttributeW)(h, col, SQL_DESC_TYPE_NAME, tbuf.as_mut_ptr() as *mut c_void, (tbuf.len() * 2) as i16, &mut tlen, &mut unused)
        };
        let type_name = if ok(rc) { String::from_utf16_lossy(&tbuf[..((tlen.max(0) as usize) / 2).min(tbuf.len() - 1)]) } else { String::new() };
        let n = (name_len.max(0) as usize).min(name.len() - 1);
        out.push(Col {
            name: String::from_utf16_lossy(&name[..n]),
            sql_type,
            // Drivers use huge sizes (2^31-1…) for "no limit": treat as unknown.
            size: if size >= 1 << 30 { 0 } else { size },
            digits,
            nullable: nullable != 0,
            type_name,
            unsigned: numeric(SQL_DESC_UNSIGNED) != 0,
            auto: numeric(SQL_DESC_AUTO_UNIQUE_VALUE) != 0,
        });
    }
    Ok(out)
}

fn transfer_column(c: &Col) -> TransferColumn {
    let size = (c.size > 0).then_some(c.size as u64);
    let digits = (c.digits >= 0).then_some(c.digits as u64);
    let name = if c.type_name.is_empty() { format!("sql_type({})", c.sql_type) } else { c.type_name.clone() };
    TransferColumn { name: c.name.clone(), type_name: crate::format_type(&name, c.sql_type, size, digits), nullable: c.nullable }
}

// ------------------------------------------------------------------- read

/// What a text read becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum As {
    Text,
    Decimal,
    Time,
    DateTimeTz,
    Uuid,
    Json,
}

/// How a result column is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Int,
    UInt,
    Float,
    Bit,
    Date,
    Timestamp,
    /// Text of at most `units` UTF-16 units: bindable.
    Text { units: usize, as_: As },
    /// Binary of at most `len` bytes: bindable.
    Bytes { len: usize },
    /// Unbounded text, read with `SQLGetData` to its end.
    LongText(As),
    LongBytes,
}

impl Fetch {
    fn is_long(self) -> bool {
        matches!(self, Fetch::LongText(_) | Fetch::LongBytes)
    }

    /// Bytes of one element in a bound column buffer.
    fn elem(self) -> usize {
        match self {
            Fetch::Int | Fetch::UInt | Fetch::Float => 8,
            Fetch::Bit => 1,
            Fetch::Date => size_of::<SqlDate>(),
            Fetch::Timestamp => size_of::<SqlTimestamp>(),
            // Room for the terminating NUL.
            Fetch::Text { units, .. } => (units + 1) * 2,
            Fetch::Bytes { len } => len.max(1),
            Fetch::LongText(_) | Fetch::LongBytes => 0,
        }
    }

    fn c_type(self) -> i16 {
        match self {
            Fetch::Int => SQL_C_SBIGINT,
            Fetch::UInt => SQL_C_UBIGINT,
            Fetch::Float => SQL_C_DOUBLE,
            Fetch::Bit => SQL_C_BIT,
            Fetch::Date => SQL_C_TYPE_DATE,
            Fetch::Timestamp => SQL_C_TYPE_TIMESTAMP,
            Fetch::Text { .. } | Fetch::LongText(_) => SQL_C_WCHAR,
            Fetch::Bytes { .. } | Fetch::LongBytes => SQL_C_BINARY,
        }
    }
}

/// JSON documents and Hive/Spark complex types (sent as JSON text).
fn is_json(type_name: &str) -> bool {
    let t = type_name.trim().to_ascii_lowercase();
    t == "json" || t == "jsonb" || ["array", "map", "struct", "uniontype"].iter().any(|p| t.starts_with(p))
}

/// How column `c` is read; `sizes`: the driver's text sizes can be trusted.
pub fn plan(c: &Col, sizes: bool) -> Fetch {
    let text_as = if is_json(&c.type_name) { As::Json } else { As::Text };
    match c.sql_type {
        SQL_BIT => Fetch::Bit,
        SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER => Fetch::Int,
        SQL_BIGINT if c.unsigned => Fetch::UInt,
        SQL_BIGINT => Fetch::Int,
        SQL_REAL | SQL_FLOAT | SQL_DOUBLE => Fetch::Float,
        // Sign, point and a leading zero around the precision's digits.
        SQL_NUMERIC | SQL_DECIMAL if (1..=1000).contains(&c.size) => Fetch::Text { units: c.size + 3, as_: As::Decimal },
        SQL_NUMERIC | SQL_DECIMAL => Fetch::LongText(As::Decimal),
        SQL_TYPE_DATE | SQL_DATETIME => Fetch::Date,
        SQL_TYPE_TIMESTAMP | SQL_TIMESTAMP_V2 => Fetch::Timestamp,
        SQL_TYPE_TIME | SQL_TIME_V2 | SQL_SS_TIME2 => Fetch::Text { units: 32, as_: As::Time },
        SQL_SS_TIMESTAMPOFFSET | SQL_TYPE_TIMESTAMP_TZ => Fetch::Text { units: 48, as_: As::DateTimeTz },
        SQL_GUID => Fetch::Text { units: 40, as_: As::Uuid },
        SQL_BINARY | SQL_VARBINARY if sizes && (1..=MAX_BOUND_BYTES).contains(&c.size) => Fetch::Bytes { len: c.size },
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => Fetch::LongBytes,
        // CLR types: their binary form (as text the driver gives hex,
        // which can't be told from real text nor loaded back).
        SQL_SS_UDT => Fetch::LongBytes,
        // A character may take two UTF-16 units (engines that count
        // characters, not units).
        SQL_CHAR | SQL_VARCHAR | SQL_WCHAR | SQL_WVARCHAR if sizes && (1..=MAX_BOUND_CHARS).contains(&c.size) => {
            Fetch::Text { units: c.size * 2, as_: text_as }
        }
        // LONGVARCHAR, XML, intervals, vendor types: text to its end.
        _ => Fetch::LongText(text_as),
    }
}

/// Rows per block fetch: as many as fit in [`FETCH_BYTES`] of column and
/// indicator buffers, between 1 and [`CHUNK_ROWS`].
pub fn rows_per_fetch(plan: &[Fetch]) -> usize {
    let width: usize = plan.iter().map(|f| f.elem() + size_of::<SqlLen>()).sum();
    (FETCH_BYTES / width.max(1)).clamp(1, CHUNK_ROWS)
}

/// Some drivers print decimals without the leading zero (`.50`).
pub fn decimal(s: &str) -> String {
    let t = s.trim();
    if let Some(r) = t.strip_prefix("-.") {
        format!("-0.{r}")
    } else if let Some(r) = t.strip_prefix('.') {
        format!("0.{r}")
    } else {
        t.to_string()
    }
}

/// `2024-01-02 03:04:05.1234567 -03:00` (SQL Server's spelling) →
/// `2024-01-02 03:04:05.1234567-03:00`.
pub fn with_offset(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    let n = b.len();
    if n > 7 && b[n - 7] == b' ' && matches!(b[n - 6], b'+' | b'-') && b[n - 3] == b':' {
        format!("{}{}", &t[..n - 7], &t[n - 6..])
    } else {
        t.to_string()
    }
}

pub fn text_cell(s: String, as_: As) -> Cell {
    match as_ {
        As::Text => Cell::Text(s),
        As::Decimal => Cell::Decimal(decimal(&s)),
        As::Time => Cell::Time(s.trim().to_string()),
        As::DateTimeTz => Cell::DateTimeTz(with_offset(&s)),
        As::Uuid => Cell::Uuid(s.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_lowercase()),
        As::Json => Cell::Json(s),
    }
}

fn get_fixed<T: Default>(api: &Api, st: &Stmt, col: u16, c_type: i16) -> Result<Option<T>> {
    let mut v = T::default();
    let mut ind: SqlLen = 0;
    // SAFETY: `v` is a plain number or C struct of the declared size.
    let rc = unsafe { (api.SQLGetData)(st.raw(), col, c_type, &mut v as *mut T as *mut c_void, size_of::<T>() as SqlLen, &mut ind) };
    if !ok(rc) {
        return Err(st.err());
    }
    Ok((ind != SQL_NULL_DATA).then_some(v))
}

/// A whole long cell (text as UTF-16 units, or bytes), in chunks.
fn get_long<T: Copy + Default>(api: &Api, st: &Stmt, col: u16, c_type: i16) -> Result<Option<Vec<T>>> {
    let unit = size_of::<T>();
    // Text chunks keep room for the NUL the driver writes.
    let nul = if c_type == SQL_C_WCHAR { unit } else { 0 };
    let mut buf = vec![T::default(); LONG_CHUNK / unit];
    let cap = buf.len() * unit;
    let mut out: Vec<T> = Vec::new();
    loop {
        if st.slot_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut ind: SqlLen = 0;
        // SAFETY: buffer length in bytes.
        let rc = unsafe { (api.SQLGetData)(st.raw(), col, c_type, buf.as_mut_ptr() as *mut c_void, cap as SqlLen, &mut ind) };
        if rc == SQL_NO_DATA {
            break;
        }
        if !ok(rc) {
            return Err(st.err());
        }
        if ind == SQL_NULL_DATA {
            return Ok(None);
        }
        let whole = ind != SQL_NO_TOTAL && (ind as usize) + nul <= cap;
        let bytes = if whole { ind as usize } else { cap - nul };
        out.extend_from_slice(&buf[..bytes / unit]);
        if rc == SQL_SUCCESS || whole {
            break;
        }
    }
    Ok(Some(out))
}

/// One cell with `SQLGetData` (row-by-row reads).
fn get_cell(api: &Api, st: &Stmt, col: u16, f: Fetch) -> Result<Cell> {
    let cell = match f {
        Fetch::Int => get_fixed::<i64>(api, st, col, SQL_C_SBIGINT)?.map(Cell::Int),
        Fetch::UInt => get_fixed::<u64>(api, st, col, SQL_C_UBIGINT)?.map(Cell::UInt),
        Fetch::Float => get_fixed::<f64>(api, st, col, SQL_C_DOUBLE)?.map(Cell::Float),
        Fetch::Bit => get_fixed::<u8>(api, st, col, SQL_C_BIT)?.map(|b| Cell::Bool(b != 0)),
        Fetch::Date => get_fixed::<SqlDate>(api, st, col, SQL_C_TYPE_DATE)?.map(|d| Cell::Date(fmt_date(&d))),
        Fetch::Timestamp => get_fixed::<SqlTimestamp>(api, st, col, SQL_C_TYPE_TIMESTAMP)?.map(|t| Cell::DateTime(fmt_timestamp(&t))),
        Fetch::Text { as_, .. } | Fetch::LongText(as_) => {
            get_long::<u16>(api, st, col, SQL_C_WCHAR)?.map(|u| text_cell(String::from_utf16_lossy(&u), as_))
        }
        Fetch::Bytes { .. } | Fetch::LongBytes => get_long::<u8>(api, st, col, SQL_C_BINARY)?.map(Cell::Bytes),
    };
    Ok(cell.unwrap_or(Cell::Null))
}

/// A bound column: its buffer (8-byte aligned) and indicators.
struct Bound {
    f: Fetch,
    buf: Vec<u64>,
    ind: Vec<SqlLen>,
}

impl Bound {
    fn new(f: Fetch, rows: usize) -> Bound {
        Bound { f, buf: vec![0u64; (f.elem() * rows).div_ceil(8)], ind: vec![0; rows] }
    }

    fn bytes(&self, r: usize) -> &[u8] {
        let e = self.f.elem();
        // SAFETY: the buffer holds `rows * elem` bytes.
        let all = unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.buf.len() * 8) };
        &all[r * e..(r + 1) * e]
    }

    fn read<T: Copy>(&self, r: usize) -> T {
        // SAFETY: element `r` holds a `T` written by the driver.
        unsafe { std::ptr::read_unaligned(self.bytes(r).as_ptr() as *const T) }
    }

    fn cell(&self, r: usize, name: &str) -> Result<Cell> {
        let ind = self.ind[r];
        if ind == SQL_NULL_DATA {
            return Ok(Cell::Null);
        }
        let e = self.f.elem();
        let truncated = || {
            Error::Query(format!(
                "El driver ODBC informó para la columna «{name}» un tamaño menor que el de sus datos; no se puede leer sin cortarlos."
            ))
        };
        Ok(match self.f {
            Fetch::Int => Cell::Int(self.read::<i64>(r)),
            Fetch::UInt => Cell::UInt(self.read::<u64>(r)),
            Fetch::Float => Cell::Float(self.read::<f64>(r)),
            Fetch::Bit => Cell::Bool(self.read::<u8>(r) != 0),
            Fetch::Date => Cell::Date(fmt_date(&self.read::<SqlDate>(r))),
            Fetch::Timestamp => Cell::DateTime(fmt_timestamp(&self.read::<SqlTimestamp>(r))),
            Fetch::Text { as_, .. } => {
                if ind == SQL_NO_TOTAL || ind as usize + 2 > e {
                    return Err(truncated());
                }
                let units: Vec<u16> = self.bytes(r)[..ind as usize].as_chunks::<2>().0.iter().map(|p| u16::from_ne_bytes(*p)).collect();
                text_cell(String::from_utf16_lossy(&units), as_)
            }
            Fetch::Bytes { .. } => {
                if ind == SQL_NO_TOTAL || ind as usize > e {
                    return Err(truncated());
                }
                Cell::Bytes(self.bytes(r)[..ind as usize].to_vec())
            }
            Fetch::LongText(_) | Fetch::LongBytes => unreachable!("long columns aren't bound"),
        })
    }
}

/// Unbinds the columns before their buffers go.
struct Unbind<'a>(&'a Api, &'a Stmt<'a>);

impl Drop for Unbind<'_> {
    fn drop(&mut self) {
        // SAFETY: valid statement handle.
        unsafe { (self.0.SQLFreeStmt)(self.1.raw(), SQL_UNBIND) };
    }
}

fn set_attr(api: &Api, st: &Stmt, attr: i32, value: *mut c_void, len: i32) -> SqlReturn {
    // SAFETY: integer attributes pass the value as the pointer; pointer
    // attributes point to memory that outlives its use (see callers).
    unsafe { (api.SQLSetStmtAttrW)(st.raw(), attr, value, len) }
}

/// Run `sql` and hand its rows to `sink` in batches; the rows read.
/// `wanted`: how many columns the read asked for (`None`: all).
pub fn read(c: &Conn, slot: &StmtSlot, sql: &str, sizes: bool, wanted: Option<usize>, sink: BatchSinkRef) -> Result<u64> {
    let api = c.api;
    let st = c.stmt(slot)?;
    st.exec(sql)?;
    let cols = describe(api, &st)?;
    if let Some(n) = wanted {
        if n != cols.len() {
            return Err(Error::Query(format!("la lectura pidió {n} columnas y el servidor devolvió {}", cols.len())));
        }
    }
    let plan: Vec<Fetch> = cols.iter().map(|c| plan(c, sizes)).collect();
    let tcols: Vec<TransferColumn> = cols.iter().map(transfer_column).collect();
    let lock_err = || Error::State("destino de lotes".into());
    sink.lock().map_err(|_| lock_err())?.begin(&tcols)?;
    let mut builder = BatchBuilder::new();

    if plan.iter().any(|f| f.is_long()) {
        // Row by row, every cell whole.
        while st.fetch()? {
            let mut row = Vec::with_capacity(plan.len());
            for (i, f) in plan.iter().enumerate() {
                row.push(get_cell(api, &st, i as u16 + 1, *f)?);
            }
            let mut s = sink.lock().map_err(|_| lock_err())?;
            builder.push(row, &mut *s)?;
        }
    } else {
        // Blocks of rows into column-wise buffers.
        let mut rows = rows_per_fetch(&plan);
        if rows > 1 && set_attr(api, &st, SQL_ATTR_ROW_ARRAY_SIZE, rows as *mut c_void, SQL_IS_UINTEGER) != SQL_SUCCESS {
            // No block cursors (or another size substituted): one row a time.
            set_attr(api, &st, SQL_ATTR_ROW_ARRAY_SIZE, std::ptr::without_provenance_mut(1), SQL_IS_UINTEGER);
            rows = 1;
        }
        let mut fetched: SqlULen = 0;
        let mut bound: Vec<Bound> = plan.iter().map(|f| Bound::new(*f, rows)).collect();
        let _unbind = Unbind(api, &st);
        if !ok(set_attr(api, &st, SQL_ATTR_ROWS_FETCHED_PTR, &mut fetched as *mut SqlULen as *mut c_void, SQL_IS_POINTER)) {
            return Err(st.err());
        }
        for (i, b) in bound.iter_mut().enumerate() {
            // SAFETY: `bound` outlives every fetch (unbound by `_unbind`,
            // dropped first).
            let rc = unsafe {
                (api.SQLBindCol)(st.raw(), i as u16 + 1, b.f.c_type(), b.buf.as_mut_ptr() as *mut c_void, b.f.elem() as SqlLen, b.ind.as_mut_ptr())
            };
            if !ok(rc) {
                return Err(st.err());
            }
        }
        while st.fetch()? {
            let n = (fetched as usize).min(rows);
            let mut s = sink.lock().map_err(|_| lock_err())?;
            for r in 0..n {
                let row = bound.iter().zip(&cols).map(|(b, c)| b.cell(r, &c.name)).collect::<Result<Vec<_>>>()?;
                builder.push(row, &mut *s)?;
            }
        }
    }
    let mut s = sink.lock().map_err(|_| lock_err())?;
    builder.flush(&mut *s)?;
    Ok(builder.rows)
}

// ------------------------------------------------------------------- load

/// How a target column's parameter is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    Int,
    UInt,
    Float,
    Bit,
    Bytes,
    /// UTF-16 text the driver converts to the column's SQL type.
    Text,
}

/// A target column.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub bind: Bind,
    /// The SQL type the parameter is declared with (before [`declared`]
    /// adjusts it to each window's values).
    pub sql_type: i16,
    pub size: usize,
    pub digits: i16,
    /// A vendor type sent as text or binary: its parameter is sized by the
    /// values, not by the column (`sql_variant` describes as 8 000, which
    /// as `nvarchar(8000)` doesn't exist and as `nvarchar(max)` doesn't fit
    /// in it).
    pub by_value: bool,
}

impl Target {
    pub fn of(c: &Col) -> Target {
        let bind = match c.sql_type {
            SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER => Bind::Int,
            SQL_BIGINT if c.unsigned => Bind::UInt,
            SQL_BIGINT => Bind::Int,
            SQL_REAL | SQL_FLOAT | SQL_DOUBLE => Bind::Float,
            SQL_BIT => Bind::Bit,
            SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY | SQL_SS_UDT => Bind::Bytes,
            _ => Bind::Text,
        };
        // Standard types go as themselves; vendor types as wide text the
        // server converts.
        let standard = matches!(
            c.sql_type,
            SQL_CHAR
                | SQL_VARCHAR
                | SQL_LONGVARCHAR
                | SQL_WCHAR
                | SQL_WVARCHAR
                | SQL_WLONGVARCHAR
                | SQL_NUMERIC
                | SQL_DECIMAL
                | SQL_TYPE_DATE
                | SQL_TYPE_TIME
                | SQL_TYPE_TIMESTAMP
                | SQL_GUID
                | SQL_TINYINT
                | SQL_SMALLINT
                | SQL_INTEGER
                | SQL_BIGINT
                | SQL_REAL
                | SQL_FLOAT
                | SQL_DOUBLE
                | SQL_BIT
                | SQL_BINARY
                | SQL_VARBINARY
                | SQL_LONGVARBINARY
        );
        let sql_type = match (standard, c.sql_type) {
            // Unlimited (`nvarchar(max)` comes as WVARCHAR of size 0): the
            // long type, which takes any length.
            (true, SQL_VARCHAR) if c.size == 0 => SQL_LONGVARCHAR,
            (true, SQL_WVARCHAR) if c.size == 0 => SQL_WLONGVARCHAR,
            (true, SQL_VARBINARY) if c.size == 0 => SQL_LONGVARBINARY,
            (true, t) => t,
            (false, SQL_DATETIME) => SQL_TYPE_DATE,
            (false, SQL_TIMESTAMP_V2) => SQL_TYPE_TIMESTAMP,
            (false, SQL_TIME_V2) => SQL_TYPE_TIME,
            // CLR types take their binary form.
            (false, SQL_SS_UDT) => SQL_VARBINARY,
            (false, _) => SQL_WVARCHAR,
        };
        let by_value = !standard && !matches!(c.sql_type, SQL_DATETIME | SQL_TIMESTAMP_V2 | SQL_TIME_V2);
        Target { name: c.name.clone(), bind, sql_type, size: c.size, digits: c.digits.max(0), by_value }
    }

    fn numeric(&self) -> bool {
        matches!(self.sql_type, SQL_NUMERIC | SQL_DECIMAL)
    }

    /// A timestamp or time declared as such (its fraction is the column's).
    fn temporal(&self) -> bool {
        self.bind == Bind::Text && matches!(self.sql_type, SQL_TYPE_TIMESTAMP | SQL_TYPE_TIME)
    }

    /// Fraction digits of a temporal column: ODBC goes to nanoseconds.
    fn fraction(&self) -> i16 {
        self.digits.min(9)
    }

    fn c_type(&self) -> i16 {
        match self.bind {
            Bind::Int => SQL_C_SBIGINT,
            Bind::UInt => SQL_C_UBIGINT,
            Bind::Float => SQL_C_DOUBLE,
            Bind::Bit => SQL_C_BIT,
            Bind::Bytes => SQL_C_BINARY,
            Bind::Text => SQL_C_WCHAR,
        }
    }

    /// Bytes of one element in a parameter buffer, for a value of width `w`.
    fn elem(&self, w: usize) -> usize {
        match self.bind {
            Bind::Int | Bind::UInt | Bind::Float => 8,
            Bind::Bit => 1,
            Bind::Bytes => w.max(1),
            Bind::Text => w.max(2),
        }
    }
}

/// A parameter value, converted for its column.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Int(i64),
    UInt(u64),
    Float(f64),
    Bit(u8),
    Bytes(Vec<u8>),
    Text(Vec<u16>),
}

impl Val {
    /// Bytes it takes in its parameter buffer.
    pub fn width(&self) -> usize {
        match self {
            Val::Null => 0,
            Val::Int(_) | Val::UInt(_) | Val::Float(_) => 8,
            Val::Bit(_) => 1,
            Val::Bytes(b) => b.len(),
            Val::Text(t) => t.len() * 2,
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// A cell as the text a column of the target converts.
fn cell_text(cell: &Cell, numeric: bool) -> Option<String> {
    Some(match cell {
        Cell::Null => return None,
        Cell::Bool(b) if numeric => (if *b { "1" } else { "0" }).into(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        // Rust prints the shortest text that reads back as the same double.
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => String::from_utf8(b.clone()).unwrap_or_else(|_| hex(b)),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
    })
}

/// `12`, `12.000` → 12.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim();
    t.parse::<i64>().ok().or_else(|| {
        let (int, frac) = t.split_once('.')?;
        (frac.bytes().all(|b| b == b'0')).then(|| int.parse::<i64>().ok()).flatten()
    })
}

fn parse_uint(s: &str) -> Option<u64> {
    let t = s.trim();
    t.parse::<u64>().ok().or_else(|| {
        let (int, frac) = t.split_once('.')?;
        (frac.bytes().all(|b| b == b'0')).then(|| int.parse::<u64>().ok()).flatten()
    })
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "y" | "yes" => Some(true),
        "0" | "false" | "f" | "n" | "no" => Some(false),
        _ => None,
    }
}

/// Convert a cell for target column `t`.
pub fn to_val(cell: &Cell, t: &Target) -> std::result::Result<Val, String> {
    if matches!(cell, Cell::Null) {
        return Ok(Val::Null);
    }
    let text = || cell_text(cell, t.numeric()).unwrap_or_default();
    let bad = |what: &str| {
        let mut v = text();
        if v.chars().count() > 60 {
            v = v.chars().take(60).collect::<String>() + "…";
        }
        format!("la columna «{}» espera {what} y recibió «{v}»", t.name)
    };
    Ok(match t.bind {
        Bind::Int => Val::Int(
            match cell {
                Cell::Int(i) => Some(*i),
                Cell::UInt(u) => i64::try_from(*u).ok(),
                Cell::Bool(b) => Some(*b as i64),
                Cell::Float(f) if f.fract() == 0.0 && *f >= -9.2e18 && *f <= 9.2e18 => Some(*f as i64),
                Cell::Decimal(s) | Cell::Text(s) => parse_int(s),
                _ => None,
            }
            .ok_or_else(|| bad("un entero"))?,
        ),
        Bind::UInt => Val::UInt(
            match cell {
                Cell::Int(i) => u64::try_from(*i).ok(),
                Cell::UInt(u) => Some(*u),
                Cell::Bool(b) => Some(*b as u64),
                Cell::Float(f) if f.fract() == 0.0 && *f >= 0.0 && *f <= 1.8e19 => Some(*f as u64),
                Cell::Decimal(s) | Cell::Text(s) => parse_uint(s),
                _ => None,
            }
            .ok_or_else(|| bad("un entero sin signo"))?,
        ),
        Bind::Float => Val::Float(
            match cell {
                Cell::Float(f) => Some(*f),
                Cell::Int(i) => Some(*i as f64),
                Cell::UInt(u) => Some(*u as f64),
                Cell::Bool(b) => Some(*b as u8 as f64),
                Cell::Decimal(s) | Cell::Text(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .ok_or_else(|| bad("un número"))?,
        ),
        Bind::Bit => Val::Bit(
            match cell {
                Cell::Bool(b) => Some(*b),
                Cell::Int(i) if *i == 0 || *i == 1 => Some(*i == 1),
                Cell::UInt(u) if *u <= 1 => Some(*u == 1),
                Cell::Decimal(s) | Cell::Text(s) => parse_bool(s),
                _ => None,
            }
            .ok_or_else(|| bad("un booleano"))? as u8,
        ),
        Bind::Bytes => match cell {
            Cell::Bytes(b) => Val::Bytes(b.clone()),
            Cell::Text(s) | Cell::Json(s) => Val::Bytes(s.as_bytes().to_vec()),
            _ => return Err(bad("datos binarios")),
        },
        Bind::Text if t.temporal() => Val::Text(wide(&trim_fraction(&text(), t.fraction() as usize))),
        Bind::Text => Val::Text(wide(&text())),
    })
}

/// A time or timestamp text with at most `keep` fraction digits: the
/// column can't hold more, and a parameter declared with more digits than
/// the column's (9 from Db2 or Oracle into `datetime2(7)`) is refused by
/// the driver. The extra digits are cut, never rounded: rounding may carry
/// into the second, the day or the year.
pub fn trim_fraction(s: &str, keep: usize) -> String {
    let Some(dot) = s.rfind('.') else { return s.to_string() };
    let digits = s[dot + 1..].bytes().take_while(u8::is_ascii_digit).count();
    // A dot not after a `hh:mm:ss` is no fraction.
    if digits <= keep || !s[..dot].ends_with(|c: char| c.is_ascii_digit()) || !s[..dot].contains(':') {
        return s.to_string();
    }
    let cut = if keep == 0 { dot } else { dot + 1 + keep };
    format!("{}{}", &s[..cut], &s[dot + 1 + digits..])
}

/// The SQL type a text or binary parameter of `window` values is declared
/// with. Narrow text (`CHAR`, `VARCHAR`, `LONGVARCHAR`) goes as its wide
/// type unless every value is ASCII: bound as UTF-16 but declared narrow,
/// the driver converts it to the client's code page and loses what doesn't
/// fit (`ñ€𝄞` into a UTF-8 `varchar` came back `ñ€??`), while declared wide
/// the server converts it to the column's own encoding. Sizes past what
/// the plain types take use the long types.
pub fn declared(sql_type: i16, size: usize, ascii: bool) -> i16 {
    let t = match sql_type {
        SQL_CHAR if !ascii => SQL_WCHAR,
        SQL_VARCHAR if !ascii => SQL_WVARCHAR,
        SQL_LONGVARCHAR if !ascii => SQL_WLONGVARCHAR,
        t => t,
    };
    match t {
        SQL_WCHAR | SQL_WVARCHAR if size > MAX_WIDE_PARAM => SQL_WLONGVARCHAR,
        SQL_CHAR | SQL_VARCHAR if size > MAX_NARROW_PARAM => SQL_LONGVARCHAR,
        SQL_BINARY | SQL_VARBINARY if size > MAX_NARROW_PARAM => SQL_LONGVARBINARY,
        t => t,
    }
}

/// Rows of a parameter array the driver executed, by their status: the
/// others failed or weren't run (with no status, a row isn't counted).
pub fn applied_rows(status: &[u16]) -> usize {
    status.iter().filter(|s| matches!(**s, SQL_PARAM_SUCCESS | SQL_PARAM_SUCCESS_WITH_INFO)).count()
}

/// Rows of one execution: grows while the parameter buffers, sized by
/// each column's widest value, stay within [`PARAM_BYTES`].
#[derive(Debug, Clone)]
pub struct Window {
    /// Widest element per column so far.
    pub elems: Vec<usize>,
    pub rows: usize,
}

impl Window {
    pub fn new(columns: usize) -> Window {
        Window { elems: vec![0; columns], rows: 0 }
    }

    /// Bytes the buffers would take with `rows` rows.
    fn bytes(elems: &[usize], rows: usize) -> usize {
        rows * elems.iter().map(|e| e + size_of::<SqlLen>()).sum::<usize>()
    }

    /// Whether a row with these element sizes still fits (a first row
    /// always does).
    pub fn fits(&self, elems: &[usize]) -> bool {
        if self.rows == 0 {
            return true;
        }
        let wider: Vec<usize> = self.elems.iter().zip(elems).map(|(a, b)| *a.max(b)).collect();
        Self::bytes(&wider, self.rows + 1) <= PARAM_BYTES
    }

    pub fn add(&mut self, elems: &[usize]) {
        for (a, b) in self.elems.iter_mut().zip(elems) {
            *a = (*a).max(*b);
        }
        self.rows += 1;
    }
}

/// `INSERT INTO t (a, b) VALUES (?, ?)`.
pub fn insert_sql(quote: Quote, table: &str, columns: &[String]) -> String {
    let marks = vec!["?"; columns.len()].join(", ");
    format!("INSERT INTO {table} ({}) VALUES ({marks})", column_list(quote, columns))
}

/// What the loading thread needs from the session.
struct Job {
    quote: Quote,
    table: String,
    columns: Vec<String>,
    commit_rows: u64,
    commit_bytes: u64,
    /// `SET IDENTITY_INSERT` when the target has an identity column.
    identity_insert: bool,
}

enum Msg {
    Batch(RowBatch),
    /// The source ended: commit what's pending. A channel closed without
    /// it (the load was dropped) rolls back the open window.
    End,
}

/// From the loading thread to [`bulk_load`].
enum Event {
    /// The last batch was executed and freed: the next one may be read.
    Taken,
    /// Rows committed so far.
    Committed(u64),
}

/// Closed when [`bulk_load`] is dropped (the orchestrator drops the load
/// when the read fails): from then on the loading thread commits nothing.
/// Every commit (and, without transactions, every execution) runs holding
/// its lock, so when `close` returns none is running either: no rows land
/// after the load is gone.
#[derive(Default)]
struct Gate(std::sync::Mutex<bool>);

impl Gate {
    fn close(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Held while committing; `Cancelled` once closed.
    fn open(&self) -> Result<std::sync::MutexGuard<'_, bool>> {
        let g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if *g {
            Err(Error::Cancelled)
        } else {
            Ok(g)
        }
    }
}

/// Autocommit off for the load's life, where the driver has transactions.
struct Tx<'a> {
    c: &'a Conn,
    on: bool,
}

impl<'a> Tx<'a> {
    fn begin(c: &'a Conn) -> Tx<'a> {
        let api = c.api;
        let mut capable: u16 = 0;
        let mut len = 0i16;
        // SAFETY: SQL_TXN_CAPABLE is a SQLUSMALLINT.
        let rc = unsafe { (api.SQLGetInfoW)(c.dbc.0, SQL_TXN_CAPABLE, &mut capable as *mut u16 as *mut c_void, 2, &mut len) };
        // SAFETY: integer attribute passed as the pointer value.
        let on = ok(rc) && capable != 0 && ok(unsafe { (api.SQLSetConnectAttrW)(c.dbc.0, SQL_ATTR_AUTOCOMMIT, std::ptr::null_mut(), SQL_IS_UINTEGER) });
        Tx { c, on }
    }

    fn end(&self, how: i16) -> Result<()> {
        // SAFETY: valid connection handle.
        let rc = unsafe { (self.c.api.SQLEndTran)(SQL_HANDLE_DBC, self.c.dbc.0, how) };
        if ok(rc) {
            Ok(())
        } else {
            Err(query_error(&diags(self.c.api, SQL_HANDLE_DBC, self.c.dbc.0)))
        }
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.on {
            // Whatever wasn't committed goes; then autocommit back on (which
            // would otherwise commit it).
            let _ = self.end(SQL_ROLLBACK);
            // SAFETY: integer attribute passed as the pointer value.
            unsafe { (self.c.api.SQLSetConnectAttrW)(self.c.dbc.0, SQL_ATTR_AUTOCOMMIT, std::ptr::without_provenance_mut(1), SQL_IS_UINTEGER) };
        }
    }
}

/// Runs a statement on drop (`SET IDENTITY_INSERT … OFF`).
struct Finally<'a> {
    c: &'a Conn,
    slot: &'a StmtSlot,
    sql: Option<String>,
}

impl Drop for Finally<'_> {
    fn drop(&mut self) {
        if let Some(sql) = self.sql.take() {
            if let Err(e) = self.c.stmt(self.slot).and_then(|st| st.exec(&sql)) {
                tracing::warn!("odbc: {sql}: {e}");
            }
        }
    }
}

/// Parameter arrays: not tried yet, work, or refused by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arrays {
    Unknown,
    Yes,
    No,
}

/// Execute the prepared `INSERT` once for `rows` (a parameter array), or
/// once per row when the driver has no arrays. `applied` grows by the rows
/// the driver executed, also when a later one fails (without transactions
/// those are already committed).
fn exec_window(api: &Api, st: &Stmt, targets: &[Target], rows: &[Vec<Val>], arrays: &mut Arrays, applied: &mut usize) -> Result<()> {
    let n = rows.len();
    if n == 0 {
        return Ok(());
    }
    if *arrays != Arrays::No {
        let rc = set_attr(api, st, SQL_ATTR_PARAMSET_SIZE, n as *mut c_void, SQL_IS_UINTEGER);
        if n > 1 && rc != SQL_SUCCESS {
            set_attr(api, st, SQL_ATTR_PARAMSET_SIZE, std::ptr::without_provenance_mut(1), SQL_IS_UINTEGER);
            *arrays = Arrays::No;
            tracing::debug!("odbc: the driver refused parameter arrays; one execution per row");
        } else if n > 1 {
            *arrays = Arrays::Yes;
        }
    }
    if *arrays == Arrays::No && n > 1 {
        for r in rows {
            exec_window(api, st, targets, std::slice::from_ref(r), arrays, applied)?;
        }
        return Ok(());
    }
    // Column-wise parameter buffers.
    let mut bufs: Vec<(Vec<u64>, Vec<SqlLen>)> = Vec::with_capacity(targets.len());
    for (ci, t) in targets.iter().enumerate() {
        let elem = rows.iter().map(|r| t.elem(r[ci].width())).max().unwrap_or(1);
        let mut buf = vec![0u64; (elem * n).div_ceil(8)];
        let mut ind = vec![0 as SqlLen; n];
        // SAFETY: the buffer holds `n * elem` bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let mut max_units = 0usize;
        let mut ascii = true;
        for (r, row) in rows.iter().enumerate() {
            let at = &mut bytes[r * elem..(r + 1) * elem];
            ind[r] = match &row[ci] {
                Val::Null => SQL_NULL_DATA,
                Val::Int(v) => {
                    at[..8].copy_from_slice(&v.to_ne_bytes());
                    8
                }
                Val::UInt(v) => {
                    at[..8].copy_from_slice(&v.to_ne_bytes());
                    8
                }
                Val::Float(v) => {
                    at[..8].copy_from_slice(&v.to_ne_bytes());
                    8
                }
                Val::Bit(v) => {
                    at[0] = *v;
                    1
                }
                Val::Bytes(b) => {
                    at[..b.len()].copy_from_slice(b);
                    max_units = max_units.max(b.len());
                    b.len() as SqlLen
                }
                Val::Text(u) => {
                    for (i, x) in u.iter().enumerate() {
                        at[i * 2..i * 2 + 2].copy_from_slice(&x.to_ne_bytes());
                    }
                    max_units = max_units.max(u.len());
                    ascii = ascii && u.iter().all(|&c| c < 0x80);
                    (u.len() * 2) as SqlLen
                }
            };
        }
        let (sql_type, size, digits) = match t.bind {
            Bind::Text if t.numeric() => (t.sql_type, t.size.max(1), t.digits),
            // The column's fraction (`to_val` cut longer ones to it).
            _ if t.temporal() => {
                let base = if t.sql_type == SQL_TYPE_TIMESTAMP { 19 } else { 8 };
                let digits = t.fraction();
                (t.sql_type, base + if digits > 0 { 1 + digits as usize } else { 0 }, digits)
            }
            Bind::Text | Bind::Bytes => {
                let size = if t.by_value { max_units.max(1) } else { t.size.max(max_units).max(1) };
                (declared(t.sql_type, size, ascii), size, 0)
            }
            _ => (t.sql_type, t.size, 0),
        };
        // SAFETY: the buffers (moved into `bufs` below, their heap memory
        // stays put) live until the execution is done and the parameters
        // are reset.
        let rc = unsafe {
            (api.SQLBindParameter)(
                st.raw(),
                ci as u16 + 1,
                SQL_PARAM_INPUT,
                t.c_type(),
                sql_type,
                size,
                digits,
                buf.as_mut_ptr() as *mut c_void,
                elem as SqlLen,
                ind.as_mut_ptr(),
            )
        };
        if !ok(rc) {
            return Err(st.err());
        }
        bufs.push((buf, ind));
    }
    // Rows the driver doesn't report on count as not executed.
    let mut status = vec![SQL_PARAM_UNUSED; n];
    if n > 1 {
        set_attr(api, st, SQL_ATTR_PARAM_STATUS_PTR, status.as_mut_ptr() as *mut c_void, SQL_IS_POINTER);
    }
    if st.slot_cancelled() {
        return Err(Error::Cancelled);
    }
    // SAFETY: prepared statement with every parameter bound.
    let rc = unsafe { (api.SQLExecute)(st.raw()) };
    let result = if rc == SQL_ERROR || rc == SQL_INVALID_HANDLE || rc == SQL_NEED_DATA {
        if n > 1 {
            *applied += applied_rows(&status);
        }
        Err(st.err())
    } else if let Some(r) = status.iter().position(|s| *s == SQL_PARAM_ERROR).filter(|_| n > 1) {
        *applied += applied_rows(&status);
        let e = st.err();
        Err(match e {
            Error::Query(m) => Error::Query(format!("fila {} del lote: {m}", r + 1)),
            e => e,
        })
    } else {
        // Row counts of each parameter set, and errors some drivers only
        // report here.
        let mut r = Ok(());
        loop {
            match st.more_results() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    r = Err(e);
                    break;
                }
            }
        }
        *applied += match (&r, n) {
            (Ok(()), _) => n,
            (Err(_), 1) => 0,
            (Err(_), _) => applied_rows(&status),
        };
        r
    };
    // SAFETY: valid statement handle; the buffers go right after.
    unsafe {
        (api.SQLFreeStmt)(st.raw(), SQL_RESET_PARAMS);
    }
    if n > 1 {
        set_attr(api, st, SQL_ATTR_PARAM_STATUS_PTR, std::ptr::null_mut(), SQL_IS_POINTER);
    }
    drop(bufs);
    result
}

fn load(c: &Conn, slot: &StmtSlot, job: &Job, mut rx: mpsc::Receiver<Msg>, events: mpsc::UnboundedSender<Event>, gate: &Gate) -> Result<u64> {
    let api = c.api;
    drop(gate.open()?);
    let cols = column_list(job.quote, &job.columns);
    // The target's columns and types.
    let described = {
        let st = c.stmt(slot)?;
        st.exec(&format!("SELECT {cols} FROM {} WHERE 1=0", job.table))?;
        describe(api, &st)?
    };
    if described.len() != job.columns.len() {
        return Err(Error::Query(format!(
            "la tabla de destino devolvió {} columnas y la carga trae {}",
            described.len(),
            job.columns.len()
        )));
    }
    let targets: Vec<Target> = described.iter().map(Target::of).collect();

    // Dropped last: identity insert off, after the rollback of a failure.
    let mut _identity = Finally { c, slot, sql: None };
    if job.identity_insert && described.iter().any(|d| d.auto) {
        c.stmt(slot)?.exec(&format!("SET IDENTITY_INSERT {} ON", job.table))?;
        _identity.sql = Some(format!("SET IDENTITY_INSERT {} OFF", job.table));
    }
    let tx = Tx::begin(c);
    let st = c.stmt(slot)?;
    let insert = wide(&insert_sql(job.quote, &job.table, &job.columns));
    // SAFETY: the text outlives the call.
    if !ok(unsafe { (api.SQLPrepareW)(st.raw(), insert.as_ptr(), insert.len() as i32) }) {
        return Err(st.err());
    }

    let mut arrays = Arrays::Unknown;
    // `total`: rows executed; with transactions, committed ones are
    // reported at each commit, without them after each execution (which
    // committed by itself), also the part of a failed one that went in.
    let (mut total, mut since_rows, mut since_bytes) = (0u64, 0u64, 0u64);
    let commit_rows = if job.commit_rows == 0 { LoadSpec::DEFAULT_COMMIT_ROWS } else { job.commit_rows };
    let commit_bytes = if job.commit_bytes == 0 { LoadSpec::DEFAULT_COMMIT_BYTES } else { job.commit_bytes };
    let exec = |rows: &[Vec<Val>], arrays: &mut Arrays, total: &mut u64| -> Result<()> {
        let mut applied = 0usize;
        let r = if tx.on {
            exec_window(api, &st, &targets, rows, arrays, &mut applied)
        } else {
            let _g = gate.open()?;
            let r = exec_window(api, &st, &targets, rows, arrays, &mut applied);
            if applied > 0 {
                let _ = events.send(Event::Committed(*total + applied as u64));
            }
            r
        };
        *total += applied as u64;
        r
    };
    loop {
        match rx.blocking_recv() {
            Some(Msg::Batch(batch)) => {
                drop(gate.open()?);
                let before = total;
                let n = batch.rows.len() as u64;
                since_bytes += batch.rows.iter().flatten().map(Cell::size).sum::<usize>() as u64;
                let mut window = Window::new(targets.len());
                let mut pending: Vec<Vec<Val>> = Vec::new();
                for (i, row) in batch.rows.iter().enumerate() {
                    if row.len() != targets.len() {
                        return Err(Error::Query(format!("una fila trae {} valores para {} columnas", row.len(), targets.len())));
                    }
                    let vals = row
                        .iter()
                        .zip(&targets)
                        .map(|(cell, t)| to_val(cell, t))
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(|m| Error::Query(format!("fila {}: {m}", before + i as u64 + 1)))?;
                    let elems: Vec<usize> = vals.iter().zip(&targets).map(|(v, t)| t.elem(v.width())).collect();
                    if !window.fits(&elems) || (arrays == Arrays::No && window.rows > 0) {
                        exec(&pending, &mut arrays, &mut total)?;
                        pending.clear();
                        window = Window::new(targets.len());
                    }
                    window.add(&elems);
                    pending.push(vals);
                }
                exec(&pending, &mut arrays, &mut total)?;
                // Freed before the next batch is read from the source (the
                // orchestrator counts it in its window until then).
                drop(pending);
                drop(batch);
                let _ = events.send(Event::Taken);
                since_rows += n;
                if tx.on && (since_rows >= commit_rows || since_bytes >= commit_bytes) {
                    let g = gate.open()?;
                    tx.end(SQL_COMMIT)?;
                    drop(g);
                    let _ = events.send(Event::Committed(total));
                    since_rows = 0;
                    since_bytes = 0;
                }
            }
            Some(Msg::End) => {
                if tx.on && since_rows > 0 {
                    let g = gate.open()?;
                    tx.end(SQL_COMMIT)?;
                    drop(g);
                    let _ = events.send(Event::Committed(total));
                }
                break;
            }
            None => return Err(Error::Cancelled),
        }
    }
    drop(st);
    drop(tx);
    Ok(total)
}

/// On drop of an unfinished [`bulk_load`]: cancels the running statement
/// and closes the gate, so the loading thread (which lives on until it
/// sees the closed channel) commits nothing more.
struct StopOnDrop {
    gate: std::sync::Arc<Gate>,
    inner: std::sync::Arc<crate::Inner>,
    api: &'static Api,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.inner.slot.cancel(self.api);
            // Waits for a commit in progress, at most.
            self.gate.close();
        }
    }
}

/// Feed `source` to [`load`] on the session's blocking thread, reporting
/// its commits to `progress` as they happen. One batch at a time: the next
/// one is only read from `source` once the thread executed and freed the
/// last, so the rows in flight are the orchestrator's window and nothing
/// more (its slot for a batch is freed on the next `next()`).
pub(crate) async fn bulk_load(s: &OdbcSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if !supports_bulk_load(s.preset) {
        return Err(no_bulk_load(s.preset));
    }
    if spec.columns.is_empty() {
        return Err(Error::Query("la carga no tiene columnas".into()));
    }
    let dbms = s.dbms.to_ascii_lowercase();
    let job = Job {
        quote: s.quote,
        table: qualified_name(s.quote, spec.table.schema(), &spec.table.name),
        columns: spec.columns.clone(),
        commit_rows: spec.commit_rows,
        commit_bytes: spec.commit_bytes,
        identity_insert: spec.keep_identity
            && (eng(s.preset) == Eng::Ase || (s.preset.is_generic() && (dbms.contains("sql server") || dbms.contains("adaptive server")))),
    };
    let (tx, rx) = mpsc::channel::<Msg>(1);
    let (etx, mut erx) = mpsc::unbounded_channel::<Event>();
    let gate = std::sync::Arc::new(Gate::default());
    let mut guard = StopOnDrop { gate: gate.clone(), inner: s.inner.clone(), api: s.api, armed: true };
    let inner = s.inner.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let conn = inner.conn.lock().unwrap_or_else(|e| e.into_inner());
        inner.slot.cancelled.store(false, Ordering::SeqCst);
        load(&conn, &inner.slot, &job, rx, etx, &gate)
    });
    'feed: loop {
        // Commits keep being reported while the source makes us wait.
        let batch = {
            let next = source.next();
            tokio::pin!(next);
            loop {
                tokio::select! {
                    b = &mut next => break b,
                    Some(e) = erx.recv() => if let Event::Committed(n) = e {
                        progress(n);
                    },
                }
            }
        };
        let Some(batch) = batch else { break };
        if tx.send(Msg::Batch(batch)).await.is_err() {
            // The loader stopped (an error): its result says why.
            break;
        }
        loop {
            match erx.recv().await {
                Some(Event::Taken) => break,
                Some(Event::Committed(n)) => progress(n),
                None => break 'feed,
            }
        }
    }
    let _ = tx.send(Msg::End).await;
    drop(tx);
    while let Some(e) = erx.recv().await {
        if let Event::Committed(n) = e {
            progress(n);
        }
    }
    let r = worker.await.map_err(|e| Error::State(e.to_string()))?;
    guard.armed = false;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;
    use dbine_driver::ObjectRef;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn col(sql_type: i16, size: usize, type_name: &str) -> Col {
        Col { name: "c".into(), sql_type, size, type_name: type_name.into(), nullable: true, ..Default::default() }
    }

    #[test]
    fn types_map_to_exact_cells() {
        assert_eq!(plan(&col(SQL_INTEGER, 10, "int"), true), Fetch::Int);
        assert_eq!(plan(&col(SQL_BIGINT, 19, "bigint"), true), Fetch::Int);
        assert_eq!(plan(&Col { unsigned: true, ..col(SQL_BIGINT, 20, "bigint unsigned") }, true), Fetch::UInt);
        assert_eq!(plan(&col(SQL_DOUBLE, 15, "float"), true), Fetch::Float);
        assert_eq!(plan(&col(SQL_BIT, 1, "bit"), true), Fetch::Bit);
        assert_eq!(plan(&col(SQL_DECIMAL, 38, "decimal"), true), Fetch::Text { units: 41, as_: As::Decimal });
        assert_eq!(plan(&col(SQL_NUMERIC, 0, "numeric"), true), Fetch::LongText(As::Decimal));
        assert_eq!(plan(&col(SQL_TYPE_DATE, 10, "date"), true), Fetch::Date);
        assert_eq!(plan(&col(SQL_TYPE_TIMESTAMP, 27, "datetime2"), true), Fetch::Timestamp);
        assert_eq!(plan(&col(SQL_SS_TIME2, 16, "time"), true), Fetch::Text { units: 32, as_: As::Time });
        assert_eq!(plan(&col(SQL_SS_TIMESTAMPOFFSET, 34, "datetimeoffset"), true), Fetch::Text { units: 48, as_: As::DateTimeTz });
        assert_eq!(plan(&col(SQL_GUID, 36, "uniqueidentifier"), true), Fetch::Text { units: 40, as_: As::Uuid });
        assert_eq!(plan(&col(SQL_VARBINARY, 16, "varbinary"), true), Fetch::Bytes { len: 16 });
        assert_eq!(plan(&col(SQL_VARBINARY, 0, "varbinary"), true), Fetch::LongBytes);
        assert_eq!(plan(&col(SQL_LONGVARBINARY, 100, "image"), true), Fetch::LongBytes);
        assert_eq!(plan(&col(SQL_WVARCHAR, 50, "nvarchar"), true), Fetch::Text { units: 100, as_: As::Text });
        assert_eq!(plan(&col(SQL_WVARCHAR, 50, "nvarchar"), false), Fetch::LongText(As::Text));
        assert_eq!(plan(&col(SQL_WVARCHAR, 5000, "nvarchar"), true), Fetch::LongText(As::Text));
        assert_eq!(plan(&col(SQL_WLONGVARCHAR, 0, "ntext"), true), Fetch::LongText(As::Text));
        assert_eq!(plan(&col(SQL_VARCHAR, 255, "array<int>"), false), Fetch::LongText(As::Json));
        assert_eq!(plan(&col(SQL_LONGVARCHAR, 0, "json"), true), Fetch::LongText(As::Json));
        assert_eq!(plan(&col(-152, 0, "xml"), true), Fetch::LongText(As::Text));
    }

    #[test]
    fn text_cells_are_normalized() {
        assert_eq!(text_cell(".50".into(), As::Decimal), Cell::Decimal("0.50".into()));
        assert_eq!(text_cell("-.5".into(), As::Decimal), Cell::Decimal("-0.5".into()));
        assert_eq!(text_cell("12345678901234567890.0123456789".into(), As::Decimal), Cell::Decimal("12345678901234567890.0123456789".into()));
        assert_eq!(
            text_cell("2024-01-02 03:04:05.1234567 -03:00".into(), As::DateTimeTz),
            Cell::DateTimeTz("2024-01-02 03:04:05.1234567-03:00".into())
        );
        assert_eq!(text_cell("2024-01-02 03:04:05+00:00".into(), As::DateTimeTz), Cell::DateTimeTz("2024-01-02 03:04:05+00:00".into()));
        assert_eq!(
            text_cell("{6F9619FF-8B86-D011-B42D-00C04FC964FF}".into(), As::Uuid),
            Cell::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into())
        );
        assert_eq!(text_cell("13:45:00.1234567".into(), As::Time), Cell::Time("13:45:00.1234567".into()));
        // Text keeps its padding: it's data.
        assert_eq!(text_cell("ab  ".into(), As::Text), Cell::Text("ab  ".into()));
    }

    #[test]
    fn bound_timestamps_keep_their_fraction() {
        let mut b = Bound::new(Fetch::Timestamp, 2);
        let t = SqlTimestamp { year: 2024, month: 2, day: 29, hour: 23, minute: 59, second: 58, fraction: 123_456_700 };
        // SAFETY: two timestamps fit in the buffer.
        unsafe { std::ptr::write_unaligned((b.buf.as_mut_ptr() as *mut u8).add(size_of::<SqlTimestamp>()) as *mut SqlTimestamp, t) };
        b.ind = vec![SQL_NULL_DATA, 16];
        assert_eq!(b.cell(0, "t").unwrap(), Cell::Null);
        assert_eq!(b.cell(1, "t").unwrap(), Cell::DateTime("2024-02-29 23:59:58.1234567".into()));

        // A text longer than the driver said is an error, not a silent cut.
        let mut b = Bound::new(Fetch::Text { units: 2, as_: As::Text }, 1);
        b.ind = vec![10];
        assert!(b.cell(0, "x").is_err());
    }

    #[test]
    fn fetch_blocks_fit_the_budget() {
        // Narrow rows: a full batch per fetch.
        assert_eq!(rows_per_fetch(&[Fetch::Int, Fetch::Float, Fetch::Timestamp]), CHUNK_ROWS);
        // Wide rows: bounded by bytes.
        let wide = [Fetch::Text { units: 8000, as_: As::Text }, Fetch::Bytes { len: 8000 }];
        let n = rows_per_fetch(&wide);
        let per_row: usize = wide.iter().map(|f| f.elem() + size_of::<SqlLen>()).sum();
        assert!(n * per_row <= FETCH_BYTES && (n + 1) * per_row > FETCH_BYTES, "{n}");
        // Never zero, even for one enormous row.
        assert_eq!(rows_per_fetch(&[Fetch::Bytes { len: 64 * 1024 * 1024 }]), 1);
    }

    #[test]
    fn parameter_windows_fit_the_budget() {
        let mut w = Window::new(2);
        let row = [8, 2000];
        let mut n = 0;
        while w.fits(&row) {
            w.add(&row);
            n += 1;
        }
        assert_eq!(n, PARAM_BYTES / (8 + 2000 + 2 * size_of::<SqlLen>()));
        // A wider value shrinks what's left.
        let mut w = Window::new(1);
        w.add(&[10]);
        assert!(!w.fits(&[PARAM_BYTES]));
        // A first row always fits, however large.
        assert!(Window::new(1).fits(&[10 * PARAM_BYTES]));
    }

    fn target(sql_type: i16, unsigned: bool) -> Target {
        Target::of(&Col { unsigned, ..col(sql_type, 10, "") })
    }

    #[test]
    fn cells_convert_to_parameters() {
        let int = target(SQL_INTEGER, false);
        assert_eq!(to_val(&Cell::Int(5), &int), Ok(Val::Int(5)));
        assert_eq!(to_val(&Cell::Decimal("12.000".into()), &int), Ok(Val::Int(12)));
        assert_eq!(to_val(&Cell::Bool(true), &int), Ok(Val::Int(1)));
        assert!(to_val(&Cell::Decimal("12.5".into()), &int).is_err());
        assert!(to_val(&Cell::UInt(u64::MAX), &int).is_err());
        assert_eq!(to_val(&Cell::UInt(u64::MAX), &target(SQL_BIGINT, true)), Ok(Val::UInt(u64::MAX)));
        assert_eq!(to_val(&Cell::Float(0.1), &target(SQL_DOUBLE, false)), Ok(Val::Float(0.1)));
        assert_eq!(to_val(&Cell::Text("t".into()), &target(SQL_BIT, false)), Ok(Val::Bit(1)));
        assert_eq!(to_val(&Cell::Null, &target(SQL_BIT, false)), Ok(Val::Null));
        let blob = target(SQL_VARBINARY, false);
        assert_eq!(to_val(&Cell::Bytes(vec![0, 255]), &blob), Ok(Val::Bytes(vec![0, 255])));
        assert!(to_val(&Cell::Int(1), &blob).is_err());
        // Exact values go as text, for the driver to convert.
        let dec = target(SQL_DECIMAL, false);
        assert_eq!(dec.bind, Bind::Text);
        assert_eq!(to_val(&Cell::Decimal("-0.000000001".into()), &dec), Ok(Val::Text(wide("-0.000000001"))));
        assert_eq!(to_val(&Cell::Bool(false), &dec), Ok(Val::Text(wide("0"))));
        let ts = Target::of(&Col { digits: 7, ..col(SQL_TYPE_TIMESTAMP, 27, "datetime2") });
        assert_eq!(to_val(&Cell::DateTime("2024-01-31 13:45:00.1234567".into()), &ts), Ok(Val::Text(wide("2024-01-31 13:45:00.1234567"))));
        let text = target(SQL_WVARCHAR, false);
        assert_eq!(to_val(&Cell::Float(1e21), &text), Ok(Val::Text(wide("1000000000000000000000"))));
        assert_eq!(to_val(&Cell::Bytes(vec![0xff]), &text), Ok(Val::Text(wide("0xFF"))));
        // Vendor types are declared as wide text.
        assert_eq!(target(SQL_SS_TIMESTAMPOFFSET, false).sql_type, SQL_WVARCHAR);
        assert_eq!(target(SQL_TIMESTAMP_V2, false).sql_type, SQL_TYPE_TIMESTAMP);
        // `(max)` columns are declared with the long types.
        assert_eq!(Target::of(&col(SQL_WVARCHAR, 0, "nvarchar")).sql_type, SQL_WLONGVARCHAR);
        assert_eq!(Target::of(&col(SQL_VARBINARY, 0, "varbinary")).sql_type, SQL_LONGVARBINARY);
        assert_eq!(Target::of(&col(SQL_WVARCHAR, 50, "nvarchar")).sql_type, SQL_WVARCHAR);
    }

    #[test]
    fn statements_keep_the_requested_order() {
        let spec = ReadSpec {
            table: ObjectRef { kind: "table".into(), schema: Some("app".into()), name: "t".into() },
            columns: Some(vec!["b".into(), "a".into()]),
            filter: Some("a > 1 OR b = 2".into()),
        };
        assert_eq!(select_sql(Quote::Double, &spec), r#"SELECT "b", "a" FROM "app"."t" WHERE (a > 1 OR b = 2)"#);
        let all = ReadSpec { columns: None, filter: Some("  ".into()), ..spec };
        assert_eq!(select_sql(Quote::Bracket, &all), "SELECT * FROM [app].[t]");
        assert_eq!(insert_sql(Quote::Backtick, "`t`", &["a".into(), "b".into()]), "INSERT INTO `t` (`a`, `b`) VALUES (?, ?)");
    }

    #[test]
    fn bulk_load_per_preset() {
        for id in ["odbc", "db2", "sybase", "sqlanywhere", "informix", "teradata", "vertica", "access", "dbase", "nuodb", "iris"] {
            assert!(supports_bulk_load(preset(id)), "{id}");
            assert!(sizes_reliable(preset(id)), "{id}");
        }
        for id in ["hive", "impala", "spark", "kyuubi", "cloudera"] {
            assert!(!supports_bulk_load(preset(id)) && !sizes_reliable(preset(id)), "{id}");
            assert!(matches!(no_bulk_load(preset(id)), Error::Unsupported(_)));
        }
        assert!(!supports_bulk_load(preset("netsuite")));
    }

    #[test]
    fn clr_types_travel_as_binary() {
        // hierarchyid, geography, geometry: their binary form, not the
        // driver's hex text (which reads as text and can't be loaded back).
        assert_eq!(plan(&col(SQL_SS_UDT, 892, "hierarchyid"), true), Fetch::LongBytes);
        assert_eq!(plan(&col(SQL_SS_UDT, 0, "geography"), true), Fetch::LongBytes);
        let t = Target::of(&col(SQL_SS_UDT, 892, "hierarchyid"));
        assert_eq!((t.bind, t.sql_type, t.by_value), (Bind::Bytes, SQL_VARBINARY, true));
        assert_eq!(to_val(&Cell::Bytes(vec![0x5B, 0x40]), &t), Ok(Val::Bytes(vec![0x5B, 0x40])));
    }

    #[test]
    fn parameters_are_declared_so_nothing_is_lost() {
        // Narrow text with non-ASCII values goes wide: declared narrow, the
        // driver converts it to the client's code page and loses characters.
        assert_eq!(declared(SQL_VARCHAR, 10, true), SQL_VARCHAR);
        assert_eq!(declared(SQL_VARCHAR, 10, false), SQL_WVARCHAR);
        assert_eq!(declared(SQL_CHAR, 10, false), SQL_WCHAR);
        assert_eq!(declared(SQL_LONGVARCHAR, 0, false), SQL_WLONGVARCHAR);
        // Past the plain types' limit, the long ones.
        assert_eq!(declared(SQL_WVARCHAR, 4_000, true), SQL_WVARCHAR);
        assert_eq!(declared(SQL_WVARCHAR, 4_001, true), SQL_WLONGVARCHAR);
        assert_eq!(declared(SQL_VARCHAR, 8_000, true), SQL_VARCHAR);
        assert_eq!(declared(SQL_VARCHAR, 6_000, false), SQL_WLONGVARCHAR);
        assert_eq!(declared(SQL_VARCHAR, 8_001, true), SQL_LONGVARCHAR);
        assert_eq!(declared(SQL_VARBINARY, 8_001, true), SQL_LONGVARBINARY);
        assert_eq!(declared(SQL_TYPE_DATE, 10, false), SQL_TYPE_DATE);
        // Vendor types (xml, sql_variant) are sized by their values.
        let variant = Target::of(&col(-150, 8_000, "sql_variant"));
        assert_eq!((variant.bind, variant.sql_type, variant.by_value), (Bind::Text, SQL_WVARCHAR, true));
        assert!(!Target::of(&col(SQL_VARCHAR, 10, "varchar")).by_value);
    }

    #[test]
    fn fractions_are_cut_to_the_column() {
        // 9 digits (Db2, Oracle, Informix) into datetime2(7): a parameter
        // declared with 9 is refused by the driver.
        let ts = Target::of(&Col { digits: 7, ..col(SQL_TYPE_TIMESTAMP, 27, "datetime2") });
        assert_eq!(
            to_val(&Cell::DateTime("2024-01-01 00:00:00.123456789".into()), &ts),
            Ok(Val::Text(wide("2024-01-01 00:00:00.1234567")))
        );
        // Cut, never rounded (no carry into the next second or day).
        assert_eq!(trim_fraction("1999-12-31 23:59:59.999999999", 7), "1999-12-31 23:59:59.9999999");
        assert_eq!(trim_fraction("1999-12-31 23:59:59.999", 0), "1999-12-31 23:59:59");
        assert_eq!(trim_fraction("2024-01-01 00:00:00.12", 7), "2024-01-01 00:00:00.12");
        assert_eq!(trim_fraction("2024-01-01 00:00:00.123456789-03:00", 3), "2024-01-01 00:00:00.123-03:00");
        assert_eq!(trim_fraction("13:45:00.1234567", 3), "13:45:00.123");
        assert_eq!(trim_fraction("2024-01-01", 0), "2024-01-01");
        // Digits above 9 (a driver's odd report) are capped.
        assert_eq!(Target::of(&Col { digits: 12, ..col(SQL_TYPE_TIMESTAMP, 30, "ts") }).fraction(), 9);
        // Decimals keep their scale.
        assert_eq!(Target::of(&Col { digits: 10, ..col(SQL_DECIMAL, 38, "decimal") }).digits, 10);
    }

    #[test]
    fn partial_parameter_arrays_count_only_executed_rows() {
        // Without transactions the executed rows are committed and must be
        // reported, also when a later row of the same array fails.
        assert_eq!(applied_rows(&[SQL_PARAM_SUCCESS, SQL_PARAM_SUCCESS_WITH_INFO, SQL_PARAM_ERROR, SQL_PARAM_UNUSED]), 2);
        assert_eq!(applied_rows(&[SQL_PARAM_UNUSED; 3]), 0);
    }
}

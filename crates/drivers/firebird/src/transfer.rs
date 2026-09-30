//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! Reading: one `SELECT` whose exact numbers, dates and times are cast to
//! text by the server (no float detour on Firebird 3, whatever the session's
//! `SET BIND`), turned into typed cells with the catalog's types.
//! `CHARACTER SET NONE` text has no known encoding: it's read as its bytes
//! (never through the connection's decoder), and goes out as text when those
//! bytes are valid UTF-8, as bytes otherwise. `TIMESTAMP WITH TIME ZONE`
//! keeps its own zone: an offset as a `DateTimeTz`, a region
//! (`America/Sao_Paulo`) as the server's text, which a Firebird target
//! parses back as is.
//!
//! Loading: Firebird has no bulk load in its wire protocol, and the pure-Rust
//! client has no Firebird 4 batch API. Rows go through one prepared
//! `EXECUTE BLOCK` holding the INSERTs of many rows (one round trip for all
//! of them), reused for the whole load, with a commit per window; rows that
//! don't fill a block use a prepared single-row INSERT. With `keep_identity`
//! the INSERTs say `OVERRIDING SYSTEM VALUE` when a loaded column is
//! `GENERATED ALWAYS AS IDENTITY`; without it identity columns are left out
//! and Firebird generates them.
//!
//! A load that stops early (an error, or dropped: cancelled, or its source
//! failed) commits nothing more: the blocking work checks a [`Gate`] before
//! every statement and commits only through it, and whatever is open is
//! rolled back before the connection is used for anything else.

use super::{err, join_err, message, poisoned, q, Conn, FirebirdSession, StmtHandle};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use rsfbclient_core::{Column, FirebirdClientSqlOps, SqlType, TrOp};
use std::sync::{Arc, Mutex};

/// Most rows in one `EXECUTE BLOCK`.
const BLOCK_ROWS: usize = 256;
/// The block's input message stays under Firebird's 64 KiB limit.
const BLOCK_MESSAGE: i64 = 60_000;
/// Parameters in one block: the pure-Rust client can't describe a
/// statement with much more than 1 000 (it fails with 1 362).
const BLOCK_PARAMS: usize = 800;

/// How a column's values are read and written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Int,
    /// NUMERIC / DECIMAL / INT128.
    Exact,
    Float,
    DecFloat,
    Date,
    Time,
    Timestamp,
    TimeTz,
    TimestampTz,
    Text,
    /// `CHARACTER SET OCTETS`: binary.
    Octets,
    /// CHAR / VARCHAR in `CHARACTER SET NONE`: bytes of unknown encoding.
    Raw,
    Bool,
    TextBlob,
    /// `BLOB SUB_TYPE TEXT CHARACTER SET NONE`.
    RawBlob,
    BinaryBlob,
    Other,
}

pub(crate) fn kind_of(ty: Option<i64>, sub: i64, scale: i64, charset: Option<i64>) -> Kind {
    match ty {
        Some(7 | 8 | 16) if sub == 0 && scale == 0 => Kind::Int,
        Some(7 | 8 | 16 | 26) => Kind::Exact,
        Some(10 | 27) => Kind::Float,
        Some(24 | 25) => Kind::DecFloat,
        Some(12) => Kind::Date,
        Some(13) => Kind::Time,
        Some(35) => Kind::Timestamp,
        Some(28) => Kind::TimeTz,
        Some(29) => Kind::TimestampTz,
        Some(14 | 37 | 40) if charset == Some(1) => Kind::Octets,
        // No character set is NONE.
        Some(14 | 37 | 40) if matches!(charset, Some(0) | None) => Kind::Raw,
        Some(14 | 37 | 40) => Kind::Text,
        Some(23) => Kind::Bool,
        Some(261) if sub == 1 && matches!(charset, Some(0) | None) => Kind::RawBlob,
        Some(261) if sub == 1 => Kind::TextBlob,
        Some(261) => Kind::BinaryBlob,
        _ => Kind::Other,
    }
}

#[derive(Debug, Clone)]
struct Col {
    name: String,
    type_name: String,
    nullable: bool,
    kind: Kind,
    computed: bool,
    /// `GENERATED ALWAYS AS IDENTITY` (`Some(true)`) or `BY DEFAULT`.
    identity: Option<bool>,
    /// Bytes of the value in a message (for sizing blocks).
    length: i64,
}

/// Longest octets value sent as hex text: its hex has to fit a VARCHAR
/// (32 765 bytes). Longer ones go as a blob parameter.
const MAX_HEX_BYTES: i64 = 32_765 / 2;

impl Col {
    /// The value goes as hex text decoded by the INSERT (`HEX_DECODE`,
    /// Firebird 4+), not as a blob parameter per value.
    fn hex(&self, major: u32) -> bool {
        matches!(self.kind, Kind::Octets | Kind::Raw) && major >= 4 && self.length <= MAX_HEX_BYTES
    }
}

fn catalog(c: &mut Conn, table: &str) -> Result<Vec<Col>> {
    let rows = c.rows(
        "SELECT TRIM(rf.RDB$FIELD_NAME), f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH,
                f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE,
                COALESCE(rf.RDB$NULL_FLAG, f.RDB$NULL_FLAG, 0), f.RDB$CHARACTER_SET_ID,
                IIF(f.RDB$COMPUTED_BLR IS NULL, 0, 1), rf.RDB$IDENTITY_TYPE
           FROM RDB$RELATION_FIELDS rf JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE
          WHERE rf.RDB$RELATION_NAME = ? ORDER BY rf.RDB$FIELD_POSITION",
        vec![SqlType::Text(table.to_string())],
    )?;
    if rows.is_empty() {
        return Err(Error::Query(format!("no existe la tabla «{table}»")));
    }
    Ok(rows
        .iter()
        .map(|r| {
            let g = |i: usize| r.get(i).and_then(super::int);
            let ty = g(1);
            Col {
                name: r.first().and_then(super::text).unwrap_or_default(),
                type_name: super::field_type(ty, g(2), g(3), g(4), g(5), g(6)),
                nullable: g(7) != Some(1),
                kind: kind_of(ty, g(2).unwrap_or(0), g(6).unwrap_or(0), g(8)),
                computed: g(9) == Some(1),
                identity: g(10).map(|t| t == 0),
                length: g(3).unwrap_or(8).max(8),
            }
        })
        .collect())
}

/// The server's major version (3, 4, 5…).
fn major(c: &mut Conn) -> u32 {
    c.rows("SELECT RDB$GET_CONTEXT('SYSTEM', 'ENGINE_VERSION') FROM RDB$DATABASE", vec![])
        .ok()
        .and_then(|r| r.first().and_then(|r| r.first()).and_then(super::text))
        .and_then(|v| v.split('.').next()?.parse().ok())
        .unwrap_or(3)
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

// ---------------------------------------------------------------- reading

/// The column as the read selects it.
fn select_expr(c: &Col, major: u32) -> String {
    let n = q(&c.name);
    match c.kind {
        Kind::Exact | Kind::DecFloat => format!("CAST({n} AS VARCHAR(64))"),
        Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimeTz => format!("CAST({n} AS VARCHAR(64))"),
        // In its own zone: `… +02:00` or `… America/Sao_Paulo`.
        Kind::TimestampTz => format!("CAST({n} AS VARCHAR(64))"),
        // Bytes as they are: never through the connection's decoder.
        Kind::Octets | Kind::Raw if major >= 4 => format!("HEX_ENCODE({n})"),
        Kind::Octets | Kind::Raw | Kind::RawBlob => format!("CAST({n} AS BLOB SUB_TYPE BINARY)"),
        _ => n,
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// `-.50` → `-0.50` (plain decimal digits).
fn plain_decimal(s: &str) -> Option<String> {
    let s = s.trim();
    let (sign, body) = match s.strip_prefix('-') {
        Some(b) => ("-", b),
        None => ("", s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int = if int.is_empty() { "0" } else { int };
    Some(if frac.is_empty() { format!("{sign}{int}") } else { format!("{sign}{int}.{frac}") })
}

/// `2024-01-01 08:00:00.0000 +02:00` → a `DateTimeTz`
/// `2024-01-01 08:00:00.0000+02:00`; a region zone
/// (`… America/Sao_Paulo`), which an offset can't carry, stays the server's
/// text.
fn tz_cell(s: &str) -> Cell {
    let s = s.trim();
    match s.rfind(' ') {
        Some(i) if s[i + 1..].starts_with(['+', '-']) => Cell::DateTimeTz(format!("{}{}", &s[..i], &s[i + 1..])),
        _ => Cell::Text(s.to_string()),
    }
}

/// `CHARACTER SET NONE` bytes: text when they are UTF-8 (a Firebird target
/// writes the same bytes back), bytes otherwise.
fn raw_cell(b: Vec<u8>) -> Cell {
    String::from_utf8(b).map_or_else(|e| Cell::Bytes(e.into_bytes()), Cell::Text)
}

pub(crate) fn cell(kind: Kind, v: SqlType) -> Cell {
    match (kind, v) {
        (_, SqlType::Null) => Cell::Null,
        (Kind::Exact, SqlType::Text(s)) => plain_decimal(&s).map_or(Cell::Text(s), Cell::Decimal),
        (Kind::DecFloat, SqlType::Text(s)) => plain_decimal(&s).map_or(Cell::Text(s), Cell::Decimal),
        (Kind::Date, SqlType::Text(s)) => Cell::Date(s.trim().to_string()),
        (Kind::Time, SqlType::Text(s)) => Cell::Time(s.trim().to_string()),
        (Kind::Timestamp, SqlType::Text(s)) => Cell::DateTime(s.trim().to_string()),
        (Kind::TimestampTz, SqlType::Text(s)) => tz_cell(&s),
        (Kind::Octets, SqlType::Text(s)) => unhex(&s).map_or(Cell::Text(s), Cell::Bytes),
        (Kind::Raw, SqlType::Text(s)) => unhex(&s).map_or(Cell::Text(s), raw_cell),
        (Kind::Raw | Kind::RawBlob, SqlType::Binary(b)) => raw_cell(b),
        (_, SqlType::Text(s)) => Cell::Text(s),
        (_, SqlType::Integer(i)) => Cell::Int(i),
        (_, SqlType::Floating(f)) => Cell::Float(f),
        (_, SqlType::Boolean(b)) => Cell::Bool(b),
        (_, SqlType::Binary(b)) => Cell::Bytes(b),
        (Kind::Date, SqlType::Timestamp(t)) => Cell::Date(t.format("%Y-%m-%d").to_string()),
        (Kind::Time, SqlType::Timestamp(t)) => Cell::Time(t.format("%H:%M:%S%.f").to_string()),
        (_, SqlType::Timestamp(t)) => Cell::DateTime(t.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
    }
}

pub(crate) async fn read_batches(s: &mut FirebirdSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let table = spec.table.name.clone();
    let wanted = spec.columns.clone();
    let filter = spec.filter.clone().filter(|f| !f.trim().is_empty());
    s.run(move |c| {
        let major = major(c);
        let cat = catalog(c, &table)?;
        let cols: Vec<Col> = match &wanted {
            Some(names) => names
                .iter()
                .map(|n| cat.iter().find(|c| &c.name == n).cloned().ok_or_else(|| Error::Query(format!("no existe la columna «{n}»"))))
                .collect::<Result<_>>()?,
            // Computed columns can't be written: they aren't copied.
            None => cat.into_iter().filter(|c| !c.computed).collect(),
        };
        let exprs: Vec<String> = cols.iter().map(|c| select_expr(c, major)).collect();
        let mut sql = format!("SELECT {} FROM {}", exprs.join(", "), q(&table));
        if let Some(f) = &filter {
            sql.push_str(&format!(" WHERE ({f})"));
        }
        let described: Vec<TransferColumn> =
            cols.iter().map(|c| TransferColumn { name: c.name.clone(), type_name: c.type_name.clone(), nullable: c.nullable }).collect();
        sink.lock().map_err(lock_err)?.begin(&described)?;
        let (_, mut stmt) = c.prepare(&sql).map_err(err)?;
        let result = (|| -> Result<u64> {
            c.client.execute(&mut c.db, &mut c.tr, &mut stmt, vec![]).map_err(err)?;
            let mut builder = BatchBuilder::new();
            while let Some(row) = c.client.fetch(&mut c.db, &mut c.tr, &mut stmt).map_err(err)? {
                let cells = row.into_iter().zip(&cols).map(|(v, col): (Column, _)| cell(col.kind, v.value)).collect();
                builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
            }
            builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
            Ok(builder.rows)
        })();
        c.free(&mut stmt);
        result
    })
    .await
}

// ---------------------------------------------------------------- loading

/// `YYYY-MM-DD[ T]HH:MM:SS[.f…][±HH:MM|Z]` → (date, time with at most 4
/// fraction digits, offset).
fn split_timestamp(s: &str) -> Option<(&str, String, Option<&str>)> {
    let s = s.trim();
    let date = s.get(..10)?;
    let rest = s.get(10..)?.trim_start_matches([' ', 'T']);
    let (time, offset) = match rest.find(['+', '-', 'Z', ' ']) {
        Some(i) => (&rest[..i], Some(rest[i..].trim())),
        None => (rest, None),
    };
    Some((date, short_time(time), offset.filter(|o| !o.is_empty())))
}

/// Firebird keeps 1/10 000 s: more fraction digits are cut.
fn short_time(t: &str) -> String {
    match t.split_once('.') {
        Some((hms, f)) => format!("{hms}.{}", &f[..f.len().min(4)]),
        None => t.to_string(),
    }
}

/// `+02:00` / `Z` → seconds east of UTC.
fn offset_secs(o: &str) -> Option<i64> {
    if o == "Z" || o == "UTC" {
        return Some(0);
    }
    let sign = match o.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let d: String = o[1..].chars().filter(|c| *c != ':').collect();
    let (h, m) = match d.len() {
        2 => (d.parse::<i64>().ok()?, 0),
        4 => (d[..2].parse::<i64>().ok()?, d[2..].parse::<i64>().ok()?),
        _ => return None,
    };
    Some(sign * (h * 3600 + m * 60))
}

/// A timestamp with an offset as UTC, for a column without a zone.
fn to_utc(date: &str, time: &str, offset: &str) -> Option<String> {
    let t = chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S%.f").ok()?;
    let utc = t - chrono::Duration::seconds(offset_secs(offset)?);
    Some(short_time(&utc.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
}

fn uuid_bytes(s: &str) -> Option<Vec<u8>> {
    let h: String = s.trim().trim_matches(['{', '}']).chars().filter(|c| *c != '-').collect();
    if h.len() == 32 {
        unhex(&h)
    } else {
        None
    }
}

/// A cell as the parameter of a column of `kind`. With `hex` (see
/// [`Col::hex`]) octets and `NONE` columns take hex text the INSERT decodes;
/// otherwise their bytes go as a blob parameter. Text into them is its UTF-8
/// bytes; other text the server converts.
pub(crate) fn param(kind: Kind, c: &Cell, hex_octets: bool) -> std::result::Result<SqlType, String> {
    let bytes = matches!(kind, Kind::Octets | Kind::Raw | Kind::RawBlob);
    Ok(match c {
        Cell::Null => SqlType::Null,
        Cell::Bool(b) => match kind {
            Kind::Int | Kind::Exact | Kind::Float | Kind::DecFloat => SqlType::Integer(i64::from(*b)),
            Kind::Bool => SqlType::Boolean(*b),
            _ => SqlType::Text(if *b { "true" } else { "false" }.into()),
        },
        // 0 / 1 into BOOLEAN (engines without a boolean type).
        Cell::Int(i @ (0 | 1)) if kind == Kind::Bool => SqlType::Boolean(*i == 1),
        Cell::Text(s) if kind == Kind::Bool && matches!(s.trim(), "0" | "1") => SqlType::Boolean(s.trim() == "1"),
        Cell::Int(i) => SqlType::Integer(*i),
        Cell::UInt(u) => i64::try_from(*u).map_or_else(|_| SqlType::Text(u.to_string()), SqlType::Integer),
        Cell::Float(f) => SqlType::Floating(*f),
        Cell::Decimal(s) => match kind {
            // `12.00` into an integer column.
            Kind::Int => {
                let whole = s.split_once('.').filter(|(_, f)| f.bytes().all(|b| b == b'0')).map_or(s.as_str(), |(i, _)| i);
                whole.parse().map_or_else(|_| SqlType::Text(s.clone()), SqlType::Integer)
            }
            _ => SqlType::Text(s.clone()),
        },
        Cell::Bytes(b) if hex_octets => SqlType::Text(hex(b)),
        Cell::Bytes(b) => match kind {
            Kind::Text | Kind::TextBlob => String::from_utf8(b.clone()).map_or_else(|e| SqlType::Binary(e.into_bytes()), SqlType::Text),
            _ => SqlType::Binary(b.clone()),
        },
        Cell::Uuid(s) if kind == Kind::Octets => match uuid_bytes(s) {
            Some(b) if hex_octets => SqlType::Text(hex(&b)),
            Some(b) => SqlType::Binary(b),
            None => return Err(format!("«{s}» no es un UUID")),
        },
        Cell::Text(s) | Cell::Json(s) | Cell::Uuid(s) if hex_octets => SqlType::Text(hex(s.as_bytes())),
        Cell::Text(s) | Cell::Json(s) | Cell::Uuid(s) if bytes => SqlType::Binary(s.as_bytes().to_vec()),
        Cell::Text(s) | Cell::Json(s) | Cell::Uuid(s) | Cell::Date(s) => SqlType::Text(s.clone()),
        Cell::Time(s) => SqlType::Text(short_time(s.trim())),
        Cell::DateTime(s) | Cell::DateTimeTz(s) => {
            let (date, time, offset) = split_timestamp(s).ok_or_else(|| format!("«{s}» no es una fecha y hora"))?;
            SqlType::Text(match (kind, offset) {
                (Kind::Date, _) => date.to_string(),
                (Kind::Time, _) => time,
                (Kind::TimeTz, Some(o)) => format!("{time} {o}"),
                (Kind::TimeTz, None) => time,
                (Kind::TimestampTz, Some(o)) => format!("{date} {time} {}", if o == "Z" { "+00:00" } else { o }),
                (Kind::TimestampTz, None) | (_, None) => format!("{date} {time}"),
                // A zone into a column without one: the same instant in UTC.
                (_, Some(o)) => to_utc(date, &time, o).ok_or_else(|| format!("«{s}» no es una fecha y hora"))?,
            })
        }
    })
}

/// The single-row INSERT and, when rows fit, an `EXECUTE BLOCK` of `rows`
/// of them.
struct Statements {
    single: String,
    block: Option<(String, usize)>,
}

/// `overriding`: a loaded column is `GENERATED ALWAYS AS IDENTITY` and its
/// values are kept.
fn statements(table: &str, cols: &[Col], major: u32, overriding: bool) -> Statements {
    let names: Vec<String> = cols.iter().map(|c| q(&c.name)).collect();
    let insert = format!(
        "INSERT INTO {} ({}){} VALUES",
        q(table),
        names.join(", "),
        if overriding { " OVERRIDING SYSTEM VALUE" } else { "" }
    );
    // A bare `?` inside HEX_DECODE has no type: cast it. A blob parameter
    // into a `NONE` column would be transliterated: through OCTETS, its
    // bytes are copied.
    let value = |c: &Col, p: String| {
        if c.hex(major) {
            format!("HEX_DECODE(CAST({p} AS VARCHAR({}) CHARACTER SET ASCII))", c.length * 2)
        } else if c.kind == Kind::Raw {
            format!("CAST({p} AS VARCHAR({}) CHARACTER SET OCTETS)", c.length)
        } else {
            p
        }
    };
    let single = format!("{insert} ({})", cols.iter().map(|c| value(c, "?".into())).collect::<Vec<_>>().join(", "));
    // Input parameters are declared as their column (hex text for octets),
    // so the block's message size is known.
    let row_bytes: i64 = cols.iter().map(|c| if c.hex(major) { c.length * 2 + 4 } else { c.length + 4 }).sum();
    let rows = ((BLOCK_MESSAGE / row_bytes.max(1)) as usize).min(BLOCK_ROWS).min(BLOCK_PARAMS / cols.len().max(1));
    let blobs = cols.iter().any(|c| matches!(c.kind, Kind::TextBlob | Kind::RawBlob | Kind::BinaryBlob | Kind::Other));
    let block = (rows >= 8 && !blobs).then(|| {
        let mut decl = Vec::with_capacity(rows * cols.len());
        let mut body = String::new();
        for r in 0..rows {
            let mut vals = Vec::with_capacity(cols.len());
            for (i, c) in cols.iter().enumerate() {
                let p = format!("P{r}_{i}");
                let ty = if c.hex(major) {
                    format!("VARCHAR({}) CHARACTER SET ASCII", c.length * 2)
                } else if c.kind == Kind::Raw {
                    format!("VARCHAR({}) CHARACTER SET OCTETS", c.length)
                } else {
                    format!("TYPE OF COLUMN {}.{}", q(table), q(&c.name))
                };
                decl.push(format!("{p} {ty} = ?"));
                vals.push(value(c, format!(":{p}")));
            }
            body.push_str(&format!("{insert} ({});\n", vals.join(", ")));
        }
        (format!("EXECUTE BLOCK ({}) AS BEGIN\n{body}END", decl.join(", ")), rows)
    });
    Statements { single, block }
}

/// A load's statements, prepared on the connection.
struct Prepared {
    single: Option<StmtHandle>,
    block: Option<(StmtHandle, usize)>,
}

impl Prepared {
    fn free(&mut self, c: &mut Conn) {
        if let Some(mut s) = self.single.take() {
            c.free(&mut s);
        }
        if let Some((mut s, _)) = self.block.take() {
            c.free(&mut s);
        }
    }
}

/// Where a load is between batches.
struct State {
    prepared: Prepared,
    /// Rows waiting for a full block.
    pending: Vec<Vec<SqlType>>,
    window_rows: u64,
    window_bytes: u64,
    total: u64,
}

impl State {
    /// The number of the first pending row (1-based, in the whole load).
    fn first_pending(&self) -> u64 {
        self.total + self.window_rows - self.pending.len() as u64 + 1
    }
}

#[derive(Default)]
struct GateState {
    /// The load was dropped or failed: nothing may commit any more.
    closed: bool,
    /// A blocking step holds the connection.
    running: bool,
    /// The load's open transaction was rolled back after `closed`.
    rolled_back: bool,
}

/// Between a load and its blocking steps. A step commits only through
/// [`Gate::commit`], which checks and commits under the lock, so closing
/// the gate waits for a commit in flight and no commit starts afterwards.
#[derive(Default)]
struct Gate(Mutex<GateState>);

impl Gate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn closed(&self) -> bool {
        self.state().closed
    }

    fn commit(&self, c: &mut Conn) -> Result<()> {
        let g = self.state();
        if g.closed {
            return Err(cancelled());
        }
        c.end_transaction(TrOp::Commit).map_err(err)
    }
}

fn cancelled() -> Error {
    Error::Query("la carga se canceló".into())
}

fn rollback(c: &mut Conn) {
    if let Err(e) = c.end_transaction(TrOp::Rollback) {
        tracing::debug!("firebird: rollback of the load: {}", message(&e));
    }
}

/// Closes the gate if the load stops without finishing (dropped: cancelled,
/// or its source failed). The open window is rolled back before the
/// connection does anything else: here when no step holds it, otherwise
/// by that step before it lets go of it.
struct Guard {
    conn: Arc<Mutex<Conn>>,
    gate: Arc<Gate>,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut g = self.gate.state();
        g.closed = true;
        if g.running || g.rolled_back {
            return;
        }
        // Busy: a step that hasn't looked at the gate yet, and will roll back.
        if let Ok(mut c) = self.conn.try_lock() {
            rollback(&mut c);
            g.rolled_back = true;
        }
    }
}

fn exec(c: &mut Conn, stmt: &mut StmtHandle, params: Vec<SqlType>) -> std::result::Result<(), String> {
    c.client.execute(&mut c.db, &mut c.tr, stmt, params).map(|_| ()).map_err(|e| message(&e))
}

/// Send the pending rows: whole blocks, and with `all`, the rest one by one.
fn drain(c: &mut Conn, st: &mut State, stmts: &Statements, gate: &Gate, all: bool) -> Result<()> {
    if let (Some((sql, rows)), None) = (&stmts.block, &st.prepared.block) {
        st.prepared.block = Some((c.prepare(sql).map_err(err)?.1, *rows));
    }
    if let Some((stmt, rows)) = st.prepared.block.as_mut() {
        while st.pending.len() >= *rows {
            if gate.closed() {
                return Err(cancelled());
            }
            let first = st.total + st.window_rows - st.pending.len() as u64 + 1;
            let params: Vec<SqlType> = st.pending.drain(..*rows).flatten().collect();
            exec(c, stmt, params).map_err(|e| Error::Query(format!("filas {first}–{}: {e}", first + *rows as u64 - 1)))?;
        }
    }
    if all || st.prepared.block.is_none() {
        if st.prepared.single.is_none() && !st.pending.is_empty() {
            st.prepared.single = Some(c.prepare(&stmts.single).map_err(err)?.1);
        }
        let first = st.first_pending();
        for (i, row) in st.pending.drain(..).enumerate() {
            if gate.closed() {
                return Err(cancelled());
            }
            let stmt = st.prepared.single.as_mut().expect("prepared");
            exec(c, stmt, row).map_err(|e| Error::Query(format!("fila {}: {e}", first + i as u64)))?;
        }
    }
    Ok(())
}

/// A loaded column: the cell it takes, its kind, and hex or not.
#[derive(Debug, Clone)]
struct Slot {
    cell: usize,
    kind: Kind,
    hex: bool,
    name: String,
}

/// The columns loaded, of `names` (the batches' cells): identity columns
/// are left out without `keep_identity`. Also whether the INSERT needs
/// `OVERRIDING SYSTEM VALUE`.
fn loaded(cat: &[Col], names: &[String], table: &str, keep_identity: bool) -> Result<(Vec<Col>, Vec<usize>, bool)> {
    let mut cols = Vec::new();
    let mut cells = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let c = cat.iter().find(|c| &c.name == n).ok_or_else(|| Error::Query(format!("no existe la columna «{n}» en «{table}»")))?;
        if c.identity.is_some() && !keep_identity {
            continue; // Firebird generates it.
        }
        cols.push(c.clone());
        cells.push(i);
    }
    if cols.is_empty() {
        return Err(Error::Query(format!(
            "no quedan columnas para cargar en «{table}»: las de identidad las genera Firebird al no conservar sus valores"
        )));
    }
    let overriding = keep_identity && cols.iter().any(|c| c.identity == Some(true));
    Ok((cols, cells, overriding))
}

pub(crate) async fn bulk_load(s: &mut FirebirdSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if spec.columns.is_empty() {
        return Err(Error::Query("la carga no tiene columnas".into()));
    }
    let table = spec.table.name.clone();
    let names = spec.columns.clone();
    let keep_identity = spec.keep_identity;
    let conn = s.conn.clone();
    let (slots, stmts) = {
        let conn = conn.clone();
        tokio::task::spawn_blocking(move || -> Result<(Vec<Slot>, Statements)> {
            let mut c = conn.lock().map_err(|_| poisoned())?;
            let major = major(&mut c);
            let cat = catalog(&mut c, &table)?;
            let (cols, cells, overriding) = loaded(&cat, &names, &table, keep_identity)?;
            let stmts = statements(&table, &cols, major, overriding);
            let slots = cols
                .iter()
                .zip(cells)
                .map(|(c, cell)| Slot { cell, kind: c.kind, hex: c.hex(major), name: c.name.clone() })
                .collect();
            Ok((slots, stmts))
        })
        .await
        .map_err(join_err)??
    };
    let width = spec.columns.len();
    let slots = Arc::new(slots);
    let stmts = Arc::new(stmts);
    let max_rows = if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows };
    let max_bytes = if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes };
    let gate = Arc::new(Gate::default());
    let mut guard = Guard { conn: conn.clone(), gate: gate.clone(), armed: true };
    let mut state = Some(State {
        prepared: Prepared { single: None, block: None },
        pending: Vec::new(),
        window_rows: 0,
        window_bytes: 0,
        total: 0,
    });

    /// A step's result: the state to go on with, or its error; either way
    /// the totals it committed.
    type Step = (std::result::Result<State, Error>, Vec<u64>);

    // One blocking step per batch (`None`: the end).
    async fn step(
        conn: &Arc<Mutex<Conn>>,
        gate: &Arc<Gate>,
        st: State,
        batch: Option<Vec<Vec<Cell>>>,
        slots: &Arc<Vec<Slot>>,
        stmts: &Arc<Statements>,
        (width, max_rows, max_bytes): (usize, u64, u64),
    ) -> Result<Step> {
        let (conn, gate, slots, stmts) = (conn.clone(), gate.clone(), slots.clone(), stmts.clone());
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().map_err(|_| poisoned())?;
            {
                let mut g = gate.state();
                if g.closed {
                    // Queued before the load was dropped: only undo.
                    if !g.rolled_back {
                        rollback(&mut c);
                        g.rolled_back = true;
                    }
                    return Ok((Err(cancelled()), Vec::new()));
                }
                g.running = true;
            }
            let mut st = st;
            let mut commits = Vec::new();
            let r = (|| -> Result<()> {
                let Some(rows) = batch else {
                    drain(&mut c, &mut st, &stmts, &gate, true)?;
                    if st.window_rows > 0 {
                        gate.commit(&mut c)?;
                        st.total += st.window_rows;
                        st.window_rows = 0;
                        commits.push(st.total);
                    }
                    return Ok(());
                };
                for row in rows {
                    if row.len() != width {
                        return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {width} columnas", row.len())));
                    }
                    st.window_bytes += row.iter().map(Cell::size).sum::<usize>() as u64;
                    let params = slots
                        .iter()
                        .map(|sl| {
                            param(sl.kind, &row[sl.cell], sl.hex).map_err(|e| {
                                Error::Query(format!("fila {}, columna «{}»: {e}", st.total + st.window_rows + 1, sl.name))
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    st.pending.push(params);
                    st.window_rows += 1;
                    if st.window_rows >= max_rows || st.window_bytes >= max_bytes {
                        drain(&mut c, &mut st, &stmts, &gate, true)?;
                        gate.commit(&mut c)?;
                        st.total += st.window_rows;
                        (st.window_rows, st.window_bytes) = (0, 0);
                        commits.push(st.total);
                    }
                }
                drain(&mut c, &mut st, &stmts, &gate, false)
            })();
            // The gate stays locked until the connection is let go, so a
            // load dropped meanwhile finds either this step running or the
            // connection free (see `Guard`).
            let mut g = gate.state();
            g.running = false;
            let r = if g.closed { r.and(Err(cancelled())) } else { r };
            let out = match r {
                Ok(()) => Ok(st),
                Err(e) => {
                    st.prepared.free(&mut c);
                    rollback(&mut c);
                    g.rolled_back = g.closed;
                    Err(e)
                }
            };
            drop(c);
            drop(g);
            Ok((out, commits))
        })
        .await
        .map_err(join_err)?
    }

    let limits = (width, max_rows, max_bytes);
    loop {
        let batch = source.next().await;
        let end = batch.is_none();
        let st = state.take().expect("state");
        let (r, commits) = step(&conn, &gate, st, batch.map(|b| b.rows), &slots, &stmts, limits).await.inspect_err(|_| {
            // The step panicked or the connection is poisoned.
            guard.armed = false;
        })?;
        // Rows committed before a failure count too.
        for n in commits {
            progress(n);
        }
        match r {
            Ok(st) => state = Some(st),
            Err(e) => {
                // Already rolled back.
                guard.armed = false;
                return Err(e);
            }
        }
        if end {
            break;
        }
    }
    let mut st = state.take().expect("state");
    guard.armed = false;
    tokio::task::spawn_blocking(move || {
        if let Ok(mut c) = conn.lock() {
            st.prepared.free(&mut c);
        }
        st.total
    })
    .await
    .map_err(join_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SqlType` has no `PartialEq`.
    macro_rules! assert_sql {
        ($a:expr, $b:expr $(,)?) => {
            assert_eq!(format!("{:?}", $a), format!("{:?}", $b))
        };
    }

    fn col(name: &str, kind: Kind, length: i64) -> Col {
        Col { name: name.into(), type_name: String::new(), nullable: true, kind, computed: false, identity: None, length }
    }

    #[test]
    fn kinds_follow_the_catalog() {
        assert_eq!(kind_of(Some(8), 0, 0, None), Kind::Int);
        assert_eq!(kind_of(Some(8), 1, -2, None), Kind::Exact);
        assert_eq!(kind_of(Some(16), 2, 0, None), Kind::Exact);
        assert_eq!(kind_of(Some(26), 0, 0, None), Kind::Exact);
        assert_eq!(kind_of(Some(14), 0, 0, Some(1)), Kind::Octets);
        assert_eq!(kind_of(Some(37), 0, 0, Some(4)), Kind::Text);
        assert_eq!(kind_of(Some(261), 1, 0, Some(4)), Kind::TextBlob);
        assert_eq!(kind_of(Some(261), 0, 0, None), Kind::BinaryBlob);
        assert_eq!(kind_of(Some(29), 0, 0, None), Kind::TimestampTz);
        // CHARACTER SET NONE: bytes of unknown encoding.
        assert_eq!(kind_of(Some(37), 0, 0, Some(0)), Kind::Raw);
        assert_eq!(kind_of(Some(14), 0, 0, None), Kind::Raw);
        assert_eq!(kind_of(Some(261), 1, 0, Some(0)), Kind::RawBlob);
    }

    #[test]
    fn charset_none_is_never_decoded() {
        // Latin-1 'élev' isn't UTF-8: its bytes, not `0x…` text.
        assert_eq!(cell(Kind::Raw, SqlType::Text("E96C6576".into())), Cell::Bytes(vec![0xE9, 0x6C, 0x65, 0x76]));
        assert_eq!(cell(Kind::Raw, SqlType::Text("C3A9".into())), Cell::Text("é".into()));
        assert_eq!(cell(Kind::Raw, SqlType::Binary(vec![0xE9])), Cell::Bytes(vec![0xE9]));
        assert_eq!(cell(Kind::RawBlob, SqlType::Binary(b"ab".to_vec())), Cell::Text("ab".into()));
        assert_eq!(select_expr(&col("V", Kind::Raw, 10), 5), r#"HEX_ENCODE("V")"#);
        assert_eq!(select_expr(&col("V", Kind::Raw, 10), 3), r#"CAST("V" AS BLOB SUB_TYPE BINARY)"#);
        assert_eq!(select_expr(&col("B", Kind::RawBlob, 8), 5), r#"CAST("B" AS BLOB SUB_TYPE BINARY)"#);
        // Back into NONE: the same bytes, whatever the connection's charset.
        assert_sql!(param(Kind::Raw, &Cell::Bytes(vec![0xE9, 0x6C]), true).unwrap(), SqlType::Text("E96C".into()));
        assert_sql!(param(Kind::Raw, &Cell::Text("é".into()), true).unwrap(), SqlType::Text("C3A9".into()));
        assert_sql!(param(Kind::RawBlob, &Cell::Bytes(vec![0xE9]), false).unwrap(), SqlType::Binary(vec![0xE9]));
        assert_sql!(param(Kind::RawBlob, &Cell::Text("é".into()), false).unwrap(), SqlType::Binary(vec![0xC3, 0xA9]));
    }

    #[test]
    fn time_zones_keep_their_zone() {
        assert_eq!(select_expr(&col("TSZ", Kind::TimestampTz, 12), 5), r#"CAST("TSZ" AS VARCHAR(64))"#);
        assert_eq!(cell(Kind::TimestampTz, SqlType::Text("2024-07-01 10:00:00.0000 +02:00".into())), Cell::DateTimeTz("2024-07-01 10:00:00.0000+02:00".into()));
        assert_eq!(
            cell(Kind::TimestampTz, SqlType::Text("2024-01-01 10:00:00.0000 America/Sao_Paulo".into())),
            Cell::Text("2024-01-01 10:00:00.0000 America/Sao_Paulo".into())
        );
        assert_sql!(
            param(Kind::TimestampTz, &Cell::Text("2024-01-01 10:00:00.0000 America/Sao_Paulo".into()), false).unwrap(),
            SqlType::Text("2024-01-01 10:00:00.0000 America/Sao_Paulo".into())
        );
    }

    #[test]
    fn identity_columns() {
        let mut id = col("ID", Kind::Int, 8);
        id.identity = Some(true);
        let cat = vec![id, col("V", Kind::Text, 40)];
        let names = vec!["ID".to_string(), "V".to_string()];
        let (cols, cells, overriding) = loaded(&cat, &names, "T", true).unwrap();
        assert_eq!((cols.len(), cells, overriding), (2, vec![0, 1], true));
        let s = statements("T", &cols, 5, overriding);
        assert_eq!(s.single, r#"INSERT INTO "T" ("ID", "V") OVERRIDING SYSTEM VALUE VALUES (?, ?)"#);
        assert!(s.block.unwrap().0.contains(r#"INSERT INTO "T" ("ID", "V") OVERRIDING SYSTEM VALUE VALUES (:P0_0, :P0_1);"#));
        // Without keep_identity Firebird generates it: the column is left out.
        let (cols, cells, overriding) = loaded(&cat, &names, "T", false).unwrap();
        assert_eq!((cols[0].name.as_str(), cells, overriding), ("V", vec![1], false));
        // BY DEFAULT takes the values without OVERRIDING.
        let mut cat = cat;
        cat[0].identity = Some(false);
        assert!(!loaded(&cat, &names, "T", true).unwrap().2);
        assert!(loaded(&cat, &names[..1], "T", false).is_err());
        assert!(loaded(&cat, &["X".to_string()], "T", true).is_err());
    }

    #[test]
    fn block_errors_name_their_rows() {
        // 1 000 rows pushed, blocks of 133: after three blocks, the fourth
        // holds rows 400–532.
        let st = State { prepared: Prepared { single: None, block: None }, pending: vec![vec![]; 601], window_rows: 1_000, window_bytes: 0, total: 0 };
        assert_eq!(st.first_pending(), 400);
        let st = State { total: 2_000, ..st };
        assert_eq!(st.first_pending(), 2_400);
    }

    #[test]
    fn values_read_typed() {
        assert_eq!(cell(Kind::Exact, SqlType::Text("-.50".into())), Cell::Decimal("-0.50".into()));
        assert_eq!(cell(Kind::Exact, SqlType::Text("170141183460469231731687303715884105727".into())), Cell::Decimal("170141183460469231731687303715884105727".into()));
        assert_eq!(cell(Kind::DecFloat, SqlType::Text("1.5E+300".into())), Cell::Text("1.5E+300".into()));
        assert_eq!(cell(Kind::Date, SqlType::Text("2024-01-31".into())), Cell::Date("2024-01-31".into()));
        assert_eq!(cell(Kind::Timestamp, SqlType::Text("2024-01-31 13:45:00.1234".into())), Cell::DateTime("2024-01-31 13:45:00.1234".into()));
        assert_eq!(cell(Kind::TimestampTz, SqlType::Text("2024-01-31 13:45:00.1234 +00:00".into())), Cell::DateTimeTz("2024-01-31 13:45:00.1234+00:00".into()));
        assert_eq!(cell(Kind::Octets, SqlType::Text("00FF10".into())), Cell::Bytes(vec![0, 255, 16]));
        assert_eq!(cell(Kind::Int, SqlType::Integer(-5)), Cell::Int(-5));
        assert_eq!(cell(Kind::Bool, SqlType::Boolean(true)), Cell::Bool(true));
        assert_eq!(cell(Kind::BinaryBlob, SqlType::Binary(vec![1, 2])), Cell::Bytes(vec![1, 2]));
        assert_eq!(cell(Kind::Text, SqlType::Null), Cell::Null);
    }

    #[test]
    fn params_convert_for_the_column() {
        let p = |k: Kind, c: Cell| param(k, &c, matches!(k, Kind::Octets | Kind::Raw)).unwrap();
        assert_sql!(p(Kind::Int, Cell::Decimal("12.00".into())), SqlType::Integer(12));
        assert_sql!(p(Kind::Exact, Cell::Decimal("12.345".into())), SqlType::Text("12.345".into()));
        assert_sql!(p(Kind::Octets, Cell::Bytes(vec![0, 255])), SqlType::Text("00FF".into()));
        assert_sql!(param(Kind::Octets, &Cell::Bytes(vec![0, 255]), false).unwrap(), SqlType::Binary(vec![0, 255]));
        assert_sql!(param(Kind::Octets, &Cell::Text("ab".into()), false).unwrap(), SqlType::Binary(b"ab".to_vec()));
        assert_sql!(
            p(Kind::Octets, Cell::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into())),
            SqlType::Text("61F0C4045CB311E7907BA6006AD3DBA0".into())
        );
        assert_sql!(p(Kind::Timestamp, Cell::DateTime("2024-01-01T10:00:00.123456789".into())), SqlType::Text("2024-01-01 10:00:00.1234".into()));
        assert_sql!(p(Kind::Timestamp, Cell::DateTimeTz("2024-01-01 01:00:00+02:00".into())), SqlType::Text("2023-12-31 23:00:00".into()));
        assert_sql!(p(Kind::TimestampTz, Cell::DateTimeTz("2024-01-01 10:00:00.5-03:00".into())), SqlType::Text("2024-01-01 10:00:00.5 -03:00".into()));
        assert_sql!(p(Kind::Date, Cell::DateTimeTz("2024-01-01 10:00:00+00:00".into())), SqlType::Text("2024-01-01".into()));
        assert_sql!(p(Kind::Time, Cell::Time("10:00:00.1234567".into())), SqlType::Text("10:00:00.1234".into()));
        assert_sql!(p(Kind::Bool, Cell::Bool(true)), SqlType::Boolean(true));
        assert_sql!(p(Kind::Int, Cell::Bool(true)), SqlType::Integer(1));
        assert_sql!(p(Kind::Bool, Cell::Int(1)), SqlType::Boolean(true));
        assert_sql!(p(Kind::Bool, Cell::Text("0".into())), SqlType::Boolean(false));
        assert_sql!(p(Kind::Exact, Cell::UInt(u64::MAX)), SqlType::Text("18446744073709551615".into()));
        assert_sql!(p(Kind::TextBlob, Cell::Bytes(vec![0xff])), SqlType::Binary(vec![0xff]));
        assert!(param(Kind::Timestamp, &Cell::DateTime("nope".into()), false).is_err());
    }

    #[test]
    fn blocks_fit_the_message_limit() {
        let cols = vec![col("ID", Kind::Int, 8), col("NAME", Kind::Text, 400), col("BIN", Kind::Octets, 16)];
        let s = statements("T", &cols, 5, false);
        assert_eq!(s.single, r#"INSERT INTO "T" ("ID", "NAME", "BIN") VALUES (?, ?, HEX_DECODE(CAST(? AS VARCHAR(32) CHARACTER SET ASCII)))"#);
        let (sql, rows) = s.block.unwrap();
        assert_eq!(rows, (60_000 / (12 + 404 + 36)) as usize);
        assert!(sql.starts_with(r#"EXECUTE BLOCK (P0_0 TYPE OF COLUMN "T"."ID" = ?, P0_1 TYPE OF COLUMN "T"."NAME" = ?, P0_2 VARCHAR(32) CHARACTER SET ASCII = ?"#));
        assert!(sql.contains(r#"VALUES (:P0_0, :P0_1, HEX_DECODE(CAST(:P0_2 AS VARCHAR(32) CHARACTER SET ASCII)));"#));
        // Wide rows or blobs: single-row inserts only.
        assert!(statements("T", &[col("X", Kind::Text, 32_000)], 5, false).block.is_none());
        assert!(statements("T", &[col("X", Kind::TextBlob, 8)], 5, false).block.is_none());
        // Octets too wide for hex in a VARCHAR go as a blob parameter.
        let wide = statements("T", &[col("ID", Kind::Int, 8), col("W", Kind::Octets, 20_000)], 5, false);
        assert_eq!(wide.single, r#"INSERT INTO "T" ("ID", "W") VALUES (?, ?)"#);
        assert!(wide.block.is_none());
        assert!(col("W", Kind::Octets, MAX_HEX_BYTES).hex(4) && !col("W", Kind::Octets, MAX_HEX_BYTES + 1).hex(4));
        assert!(!col("W", Kind::Octets, 16).hex(3));
        // NONE without hex: through OCTETS (a blob into NONE is transliterated).
        let raw = statements("T", &[col("ID", Kind::Int, 8), col("N", Kind::Raw, 20)], 3, false);
        assert_eq!(raw.single, r#"INSERT INTO "T" ("ID", "N") VALUES (?, CAST(? AS VARCHAR(20) CHARACTER SET OCTETS))"#);
        assert!(raw.block.unwrap().0.contains("P0_1 VARCHAR(20) CHARACTER SET OCTETS = ?"));
    }
}

//! Bulk transfer (see `dbine_driver::transfer`) for Aurora DSQL.
//!
//! - **Read** ([`read_batches`]): the requested columns, in the requested
//!   order, over the simple query protocol (the one the session already
//!   uses on DSQL). DSQL ends any transaction after 5 minutes, so a table
//!   with a primary key is read in keyset pages (`WHERE (pk) > (last) ORDER
//!   BY pk LIMIT n`), each its own short statement; a page is buffered
//!   whole (at most [`PAGE_BYTES`]) before it goes to the sink, so a slow
//!   target never keeps a statement open. The pages are separate snapshots:
//!   rows changed while the copy runs may show up in their old or new form.
//!   The `SELECT` is prepared first only to learn each column's type, and
//!   every text value becomes a typed cell: integers, floats, exact
//!   decimals, whole binaries, dates and times in ISO form, UUIDs and JSON.
//!   What has no cell of its own (intervals, arrays, `24:00:00`…) stays as
//!   the server's text. `money` is read as `numeric`.
//! - **Load** ([`bulk_load`]): multi-row `INSERT … VALUES` with binds. DSQL
//!   has no `COPY FROM` path that fits its transaction limits (3,000 rows
//!   and 10 MiB modified per transaction, secondary indexes included), so
//!   every commit window is one autocommit `INSERT` (its own transaction) of
//!   at most `min(commit_rows, 3,000)` rows, `min(commit_bytes, 3 MiB)` of
//!   bound text and 65,535 binds. A window DSQL still finds too big (wide
//!   indexed columns: SQLSTATE class 54) is split in halves and each half
//!   sent again. Values are bound in PostgreSQL's text format and the
//!   server parses them for each target column's type (the same input
//!   functions as a literal: a too-long text or an out-of-range number is an
//!   error, never truncated); a value with a non-zero UTC offset bound for a
//!   `timestamp` without zone is refused instead of losing its offset.
//!   Windows run on up to [`PARALLEL`] connections at once (the session's
//!   plus fresh ones, signed with a new IAM token), fed through a one-slot
//!   queue so about 10 windows' worth of bytes (~30 MiB) are in memory at
//!   most. A window refused by DSQL's optimistic concurrency (`OC000`,
//!   `OC001`, `40001`) is retried with backoff, which is safe because a
//!   refused statement committed nothing. When anything fails, no new
//!   window starts and the ones already sent are awaited before returning,
//!   so no row commits after `bulk_load` returned its error; if the load is
//!   dropped instead (cancelled), the statements in flight get a cancel
//!   request. `table_lock` is ignored (DSQL has no table locks);
//!   `keep_identity` adds `OVERRIDING SYSTEM VALUE` when a loaded column is
//!   an identity, and after the load moves its sequence past the loaded
//!   maximum.

use crate::{err, open_session, DsqlSession, Reopen};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{ColumnInfo, Error, ObjectRef, Result, Session};
use futures::{pin_mut, StreamExt};
use postgres_native_tls::MakeTlsConnector;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio_postgres::types::{private::BytesMut, Format, IsNull, ToSql, Type};
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage, Statement};

/// DSQL's limit of rows modified by one transaction.
pub(crate) const MAX_TX_ROWS: u64 = 3_000;
/// Bound text per window: margin under DSQL's 10 MiB per transaction, and
/// with about 10 windows alive at once (see [`bulk_load`]), ~30 MiB per
/// table in memory.
pub(crate) const MAX_TX_BYTES: u64 = 3 * 1024 * 1024;
/// A read page closes at this many estimated bytes (it's held whole before
/// going to the sink)…
pub(crate) const PAGE_BYTES: usize = 8 * 1024 * 1024;
/// …or at its row limit: the first page's, then sized from the rows seen.
const FIRST_PAGE_ROWS: usize = 256;
const MAX_PAGE_ROWS: usize = 20_000;
/// Binds per statement (the protocol's 16-bit count).
const MAX_PARAMS: usize = 65_535;
/// Connections loading windows at the same time.
pub(crate) const PARALLEL: usize = 4;
/// Retries of a window refused by optimistic concurrency.
const RETRIES: u32 = 8;

fn table_name(t: &ObjectRef) -> String {
    qualified_name(Quote::Double, t.schema(), &t.name)
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

/// The catalog columns named in `wanted`, in that order (exact name first,
/// then ignoring case); all of them when `None`.
pub(crate) fn pick<'a>(catalog: &'a [ColumnInfo], wanted: Option<&[String]>, table: &str) -> Result<Vec<&'a ColumnInfo>> {
    let Some(wanted) = wanted else { return Ok(catalog.iter().collect()) };
    wanted
        .iter()
        .map(|n| {
            catalog
                .iter()
                .find(|c| &c.name == n)
                .or_else(|| catalog.iter().find(|c| c.name.eq_ignore_ascii_case(n)))
                .ok_or_else(|| Error::Query(format!("la tabla {table} no tiene la columna «{n}»")))
        })
        .collect()
}

/// The read's `SELECT`: the columns in order (`money` as `numeric`, exact)
/// and the filter, if any.
pub(crate) fn select_sql(table: &str, cols: &[&ColumnInfo], filter: Option<&str>) -> String {
    let exprs: Vec<String> = cols
        .iter()
        .map(|c| {
            let q = quote_ident(Quote::Double, &c.name);
            if c.data_type == "money" {
                format!("{q}::numeric AS {q}")
            } else {
                q
            }
        })
        .collect();
    let mut s = format!("SELECT {} FROM {table}", exprs.join(", "));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        let _ = write!(s, " WHERE ({f})");
    }
    s
}

/// How a column's text values become cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int,
    Float,
    Numeric,
    Text,
    Bytea,
    Uuid,
    Date,
    Time,
    Timestamp,
    Timestamptz,
    Json,
}

impl Kind {
    pub(crate) fn of(t: &Type) -> Kind {
        match *t {
            Type::BOOL => Kind::Bool,
            Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => Kind::Int,
            Type::FLOAT4 | Type::FLOAT8 => Kind::Float,
            Type::NUMERIC => Kind::Numeric,
            Type::BYTEA => Kind::Bytea,
            Type::UUID => Kind::Uuid,
            Type::DATE => Kind::Date,
            Type::TIME => Kind::Time,
            Type::TIMESTAMP => Kind::Timestamp,
            Type::TIMESTAMPTZ => Kind::Timestamptz,
            Type::JSON | Type::JSONB => Kind::Json,
            _ => Kind::Text,
        }
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DD` (no era suffix, 4-digit year).
fn iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && digits(&s[..4]) && digits(&s[5..7]) && digits(&s[8..])
}

/// `HH:MM:SS[.f…]` with the hour at most 23: PostgreSQL's `time` also
/// takes `24:00:00`, which other targets don't, so that stays text.
fn iso_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 8
        && b[2] == b':'
        && b[5] == b':'
        && digits(&s[..2])
        && &s[..2] < "24"
        && digits(&s[3..5])
        && digits(&s[6..8])
        && (b.len() == 8 || (b[8] == b'.' && digits(&s[9..])))
}

/// `YYYY-MM-DD HH:MM:SS[.f…]`.
fn iso_datetime(s: &str) -> bool {
    s.len() >= 19 && s.as_bytes()[10] == b' ' && iso_date(&s[..10]) && iso_time(&s[11..])
}

/// PostgreSQL's `timestamptz` text (`…+00`, `…-03:30`) with a `±HH:MM`
/// offset; `None` for what doesn't fit (BC dates, `infinity`, offsets with
/// seconds).
pub(crate) fn timestamptz(v: &str) -> Option<String> {
    let p = v.get(19..)?.rfind(['+', '-'])? + 19;
    let (base, off) = v.split_at(p);
    if !iso_datetime(base) {
        return None;
    }
    let hm = &off[1..];
    let off = match hm.len() {
        2 if digits(hm) => format!("{off}:00"),
        5 if hm.as_bytes()[2] == b':' && digits(&hm[..2]) && digits(&hm[3..]) => off.to_string(),
        _ => return None,
    };
    Some(format!("{base}{off}"))
}

/// Digits with an optional sign and point (no `NaN`, no exponent).
fn plain_decimal(v: &str) -> bool {
    let v = v.strip_prefix('-').unwrap_or(v);
    let mut parts = v.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    (int.is_empty() || digits(int)) && frac.is_none_or(|f| f.is_empty() || digits(f)) && v.bytes().any(|b| b.is_ascii_digit())
}

/// A text value of a column as a cell; what doesn't parse stays text.
pub(crate) fn text_cell(k: Kind, v: Option<&str>) -> Cell {
    let Some(v) = v else { return Cell::Null };
    let text = || Cell::Text(v.to_string());
    match k {
        Kind::Bool => match v {
            "t" | "true" => Cell::Bool(true),
            "f" | "false" => Cell::Bool(false),
            _ => text(),
        },
        Kind::Int => v.parse().map_or_else(|_| text(), Cell::Int),
        Kind::Float => v.parse().map_or_else(|_| text(), Cell::Float),
        Kind::Numeric if plain_decimal(v) => Cell::Decimal(v.to_string()),
        Kind::Bytea => v.strip_prefix("\\x").and_then(crate::unhex).map_or_else(text, Cell::Bytes),
        Kind::Uuid if v.len() == 36 => Cell::Uuid(v.to_ascii_lowercase()),
        Kind::Date if iso_date(v) => Cell::Date(v.to_string()),
        Kind::Time if iso_time(v) => Cell::Time(v.to_string()),
        Kind::Timestamp if iso_datetime(v) => Cell::DateTime(v.to_string()),
        Kind::Timestamptz => timestamptz(v).map_or_else(text, Cell::DateTimeTz),
        Kind::Json => Cell::Json(v.to_string()),
        _ => text(),
    }
}

/// Output settings every value above parses from: ISO dates and hex
/// binaries. DSQL may refuse a setting; the defaults are these anyway.
async fn portable_output(s: &DsqlSession) {
    for set in ["SET DateStyle = ISO, YMD", "SET bytea_output = hex", "SET extra_float_digits = 3"] {
        if let Err(e) = s.client.simple_query(set).await {
            tracing::debug!("dsql: {set}: {e}");
        }
    }
}

/// The primary key's columns in key order (`indkey`), or the catalog's
/// key columns in table order when the index can't be read.
async fn primary_key(s: &DsqlSession, t: &ObjectRef, catalog: &[ColumnInfo]) -> Vec<String> {
    let schema = t.schema().unwrap_or("public");
    let q = "SELECT a.attname::text, a.attnum::int4, i.indkey::text
             FROM pg_catalog.pg_index i
             JOIN pg_catalog.pg_class c ON c.oid = i.indrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY (i.indkey)
             WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2";
    match s.client.query(q, &[&schema, &t.name]).await {
        Ok(rows) if !rows.is_empty() => {
            let key: String = rows[0].get(2);
            let order: Vec<i32> = key.split_whitespace().filter_map(|n| n.parse().ok()).collect();
            let mut cols: Vec<(usize, String)> = rows
                .iter()
                .map(|r| (order.iter().position(|n| *n == r.get::<_, i32>(1)).unwrap_or(usize::MAX), r.get(0)))
                .collect();
            cols.sort();
            cols.into_iter().map(|(_, n)| n).collect()
        }
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::debug!("dsql: primary key of {}: {e}", t.name);
            catalog.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
        }
    }
}

/// A value as an escape string literal (`E'…'`): the same text whatever
/// `standard_conforming_strings` says.
pub(crate) fn literal(v: &str) -> String {
    format!("E'{}'", v.replace('\\', "\\\\").replace('\'', "''"))
}

/// One keyset page: `select` (the columns, the key's appended at the end
/// when not among them) after the row whose key is `after`.
pub(crate) fn page_sql(select: &str, filter: Option<&str>, key: &[String], after: Option<&[String]>, limit: usize) -> String {
    let mut s = select.to_string();
    let quoted: Vec<String> = key.iter().map(|k| quote_ident(Quote::Double, k)).collect();
    let mut conds = Vec::new();
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        conds.push(format!("({f})"));
    }
    if let Some(after) = after {
        let lits: Vec<String> = after.iter().map(|v| literal(v)).collect();
        conds.push(format!("({}) > ({})", quoted.join(", "), lits.join(", ")));
    }
    if !conds.is_empty() {
        let _ = write!(s, " WHERE {}", conds.join(" AND "));
    }
    let _ = write!(s, " ORDER BY {} LIMIT {limit}", quoted.join(", "));
    s
}

/// The next page's row limit: what fits in [`PAGE_BYTES`] at the average
/// row size seen, growing at most 4× per page.
pub(crate) fn next_limit(limit: usize, rows: usize, bytes: usize) -> usize {
    if rows == 0 {
        return limit;
    }
    let fit = PAGE_BYTES / (bytes / rows).max(1);
    fit.clamp(1, (limit * 4).min(MAX_PAGE_ROWS))
}

pub(crate) async fn read_batches(s: &mut DsqlSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let table = table_name(&spec.table);
    let catalog = s.columns(&spec.table).await?;
    if catalog.is_empty() {
        return Err(Error::Query(format!("no se encontró la tabla {table}")));
    }
    let cols = pick(&catalog, spec.columns.as_deref(), &table)?;
    let key = primary_key(s, &spec.table, &catalog).await;
    // Key columns not requested are read too, after the others, and not
    // handed on.
    let mut all = cols.clone();
    let mut key_at = Vec::new();
    for k in &key {
        match all.iter().position(|c| &c.name == k) {
            Some(i) => key_at.push(i),
            None => {
                let c = catalog.iter().find(|c| &c.name == k).ok_or_else(|| Error::State(format!("clave de {table}")))?;
                key_at.push(all.len());
                all.push(c);
            }
        }
    }
    let base = select_sql(&table, &all, None);
    let filter = spec.filter.as_deref();
    portable_output(s).await;
    let stmt = s.client.prepare(&select_sql(&table, &all, filter)).await.map_err(err)?;
    let kinds: Vec<Kind> = stmt.columns().iter().map(|c| Kind::of(c.type_())).collect();
    drop(stmt);
    let described: Vec<TransferColumn> = cols
        .iter()
        .map(|c| TransferColumn { name: c.name.clone(), type_name: c.data_type.clone(), nullable: c.nullable })
        .collect();
    sink.lock().map_err(lock_err)?.begin(&described)?;
    let n = cols.len();
    let mut builder = BatchBuilder::new();

    if key.is_empty() {
        // Nothing to page by: one statement, which DSQL ends after 5 minutes.
        tracing::warn!("dsql: {table} has no primary key; read in one statement");
        let stream = s.client.simple_query_raw(&select_sql(&table, &cols, filter)).await.map_err(err)?;
        pin_mut!(stream);
        while let Some(msg) = stream.next().await {
            if let SimpleQueryMessage::Row(r) = msg.map_err(err)? {
                let cells = kinds[..n].iter().enumerate().map(|(i, k)| text_cell(*k, r.get(i))).collect();
                builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
            }
        }
        builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
        return Ok(builder.rows);
    }

    let mut after: Option<Vec<String>> = None;
    let mut limit = FIRST_PAGE_ROWS;
    loop {
        let sql = page_sql(&base, filter, &key, after.as_deref(), limit);
        let mut page: Vec<Vec<Cell>> = Vec::new();
        let mut bytes = 0usize;
        let mut cut = false;
        {
            let stream = s.client.simple_query_raw(&sql).await.map_err(err)?;
            pin_mut!(stream);
            while let Some(msg) = stream.next().await {
                if let SimpleQueryMessage::Row(r) = msg.map_err(err)? {
                    let mut last = Vec::with_capacity(key_at.len());
                    for &i in &key_at {
                        last.push(r.get(i).ok_or_else(|| Error::State(format!("clave nula en {table}")))?.to_string());
                    }
                    after = Some(last);
                    let cells: Vec<Cell> = kinds[..n].iter().enumerate().map(|(i, k)| text_cell(*k, r.get(i))).collect();
                    bytes += cells.iter().map(Cell::size).sum::<usize>();
                    page.push(cells);
                    if bytes >= PAGE_BYTES {
                        // The rest of the page comes again from here.
                        cut = true;
                        break;
                    }
                }
            }
        }
        let rows = page.len();
        for cells in page {
            builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
        }
        if !cut && rows < limit {
            break;
        }
        limit = next_limit(limit, rows, bytes);
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

// ---------------------------------------------------------------- loading

/// A value bound in PostgreSQL's text format: the server parses it for the
/// target column's type, as it would a literal.
#[derive(Debug)]
pub(crate) struct TextParam(pub(crate) Option<String>);

impl ToSql for TextParam {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }
    tokio_postgres::types::to_sql_checked!();
}

/// A cell as PostgreSQL input text.
pub(crate) fn bind_text(c: &Cell) -> Option<String> {
    Some(match c {
        Cell::Null => return None,
        Cell::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_nan() => "NaN".into(),
        Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        // Shortest text that reads back as the same bits.
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("\\x");
            for x in b {
                let _ = write!(s, "{x:02x}");
            }
            s
        }
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

/// Rows and estimated bytes of one commit window: the spec's (0: no limit)
/// capped by DSQL's per-transaction limits and the binds per statement.
pub(crate) fn window_limits(spec: &LoadSpec, ncols: usize) -> (usize, u64) {
    let cap = |v: u64, max: u64| if v == 0 { max } else { v.min(max) };
    let rows = cap(spec.commit_rows, MAX_TX_ROWS) as usize;
    let rows = rows.min(MAX_PARAMS / ncols.max(1)).max(1);
    (rows, cap(spec.commit_bytes, MAX_TX_BYTES))
}

/// `INSERT INTO t (cols) [OVERRIDING SYSTEM VALUE] VALUES `.
pub(crate) fn insert_head(table: &str, cols: &[String], identity: bool) -> String {
    let list: Vec<String> = cols.iter().map(|c| quote_ident(Quote::Double, c)).collect();
    format!(
        "INSERT INTO {table} ({}){} VALUES ",
        list.join(", "),
        if identity { " OVERRIDING SYSTEM VALUE" } else { "" }
    )
}

/// The head plus `rows` rows of `ncols` binds each.
pub(crate) fn insert_sql(head: &str, ncols: usize, rows: usize) -> String {
    let mut s = String::with_capacity(head.len() + rows * ncols * 8);
    s.push_str(head);
    let mut n = 0;
    for r in 0..rows {
        s.push_str(if r == 0 { "(" } else { ", (" });
        for c in 0..ncols {
            n += 1;
            let _ = write!(s, "{}${n}", if c == 0 { "" } else { ", " });
        }
        s.push(')');
    }
    s
}

/// DSQL's optimistic concurrency refusals (and PostgreSQL's serialization
/// failure): the statement committed nothing and can run again.
pub(crate) fn retryable(code: Option<&str>) -> bool {
    matches!(code, Some("OC000" | "OC001" | "40001" | "40P01"))
}

/// A limit refused the statement (SQLSTATE class 54: DSQL's 10 MiB or
/// 3,000 rows per transaction, index entries included; PostgreSQL's index
/// row size): it committed nothing, and a smaller window may fit.
pub(crate) fn too_big(code: Option<&str>) -> bool {
    code.is_some_and(|c| c.starts_with("54"))
}

/// Rows of the first half when a window of `rows` is split.
pub(crate) fn half(rows: usize) -> usize {
    rows.div_ceil(2)
}

/// A `timestamp` column without time zone (`format_type`'s spelling).
pub(crate) fn plain_timestamp(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    t.starts_with("timestamp") && !t.ends_with("with time zone") || t.ends_with("without time zone")
}

/// A cell as the bind for its column: a value with a non-zero offset for a
/// `timestamp` without zone is refused (PostgreSQL would drop the offset).
pub(crate) fn bind_for(c: &Cell, plain_ts: bool, column: &str) -> Result<Option<String>> {
    if let (true, Cell::DateTimeTz(v)) = (plain_ts, c) {
        let off = v.get(v.len().saturating_sub(6)..).unwrap_or("");
        if off != "+00:00" && off != "-00:00" {
            return Err(Error::Query(format!(
                "la columna «{column}» es timestamp sin zona horaria y el valor {v} trae un desfase: se perdería al cargarlo. \
                 Usá timestamptz en el destino"
            )));
        }
        return Ok(Some(v[..v.len() - 6].to_string()));
    }
    Ok(bind_text(c))
}

fn backoff(attempt: u32) -> Duration {
    // 50 ms doubling up to ~3 s, with jitter so parallel windows spread out.
    let base = 50u64 << attempt.min(6);
    let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64) % base;
    Duration::from_millis(base + jitter)
}

/// A commit window: its rows' binds, in text, row after row.
type Window = Vec<TextParam>;

/// Sends a cancel request for the statement in flight when dropped armed:
/// the load was dropped (cancelled) while a window was on the wire.
struct CancelOnDrop {
    token: Option<CancelToken>,
    tls: MakeTlsConnector,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else { return };
        let tls = self.tls.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = token.cancel_query(tls).await {
                    tracing::debug!("dsql: cancel of a bulk load window failed: {e}");
                }
            });
        }
    }
}

/// Resolves once the load was told to stop.
async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// What the workers share.
struct Shared<'a> {
    rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Window>>,
    head: String,
    ncols: usize,
    done: Mutex<u64>,
    progress: Progress<'a>,
    stop: watch::Receiver<bool>,
}

/// A connection loading windows until the queue closes or the load stops.
async fn worker(client: &Client, cancel: (CancelToken, MakeTlsConnector), sh: &Shared<'_>) -> Result<()> {
    let mut prepared: Option<(usize, Statement)> = None;
    loop {
        let next = tokio::select! {
            biased;
            _ = stopped(sh.stop.clone()) => return Ok(()),
            w = async { sh.rx.lock().await.recv().await } => w,
        };
        let Some(w) = next else { return Ok(()) };
        // Halves of a window too big for one transaction wait here.
        let mut todo = vec![w];
        while let Some(mut params) = todo.pop() {
            let n = params.len() / sh.ncols;
            let stmt = match &prepared {
                Some((k, st)) if *k == n => st.clone(),
                _ => {
                    let st = client.prepare(&insert_sql(&sh.head, sh.ncols, n)).await.map_err(err)?;
                    prepared = Some((n, st.clone()));
                    st
                }
            };
            let mut attempt = 0;
            let split = loop {
                if *sh.stop.borrow() {
                    // Another window failed: nothing new starts.
                    return Ok(());
                }
                let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                let mut guard = CancelOnDrop { token: Some(cancel.0.clone()), tls: cancel.1.clone() };
                let res = client.execute(&stmt, &refs).await;
                guard.token = None;
                match res {
                    Ok(_) => break false,
                    Err(e) if attempt < RETRIES && retryable(e.code().map(|c| c.code())) => {
                        attempt += 1;
                        tracing::debug!("dsql: window of {n} rows refused ({e}), retry {attempt}");
                        tokio::select! {
                            _ = tokio::time::sleep(backoff(attempt)) => {}
                            _ = stopped(sh.stop.clone()) => return Ok(()),
                        }
                    }
                    Err(e) if n > 1 && too_big(e.code().map(|c| c.code())) => {
                        tracing::debug!("dsql: window of {n} rows too big ({e}), split");
                        break true;
                    }
                    Err(e) => return Err(err(e)),
                }
            };
            if split {
                let tail = params.split_off(half(n) * sh.ncols);
                todo.push(tail);
                todo.push(params);
                continue;
            }
            let mut total = sh.done.lock().map_err(|_| Error::State("progreso de la carga".into()))?;
            *total += n as u64;
            (sh.progress)(*total);
        }
    }
}

/// Another connection like the session's (a fresh IAM token when it logged
/// in through IAM; the test hook's password otherwise).
async fn another(r: &Reopen) -> Result<DsqlSession> {
    let password = if r.target.region.is_empty() { r.password.clone() } else { crate::auth_token(&r.cfg, &r.target).await? };
    open_session(&r.cfg, &r.target, &password, r.ssl).await
}

/// Moves an identity / serial column's sequence past the loaded values
/// (never back): the target's next ordinary `INSERT` gets a free value.
pub(crate) fn reseed_sql(table: &str, column: &str) -> String {
    let c = quote_ident(Quote::Double, column);
    format!(
        "SELECT pg_catalog.setval(q.s::regclass, CASE WHEN p.increment_by > 0 THEN x.hi ELSE x.lo END)
         FROM (SELECT pg_catalog.pg_get_serial_sequence($1, $2) AS s) q
         JOIN pg_catalog.pg_sequences p ON pg_catalog.format('%I.%I', p.schemaname, p.sequencename) = q.s
         CROSS JOIN (SELECT max({c})::int8 AS hi, min({c})::int8 AS lo FROM {table}) x
         WHERE x.hi IS NOT NULL
           AND CASE WHEN p.increment_by > 0 THEN p.last_value IS NULL OR p.last_value < x.hi
                    ELSE p.last_value IS NULL OR p.last_value > x.lo END"
    )
}

pub(crate) async fn bulk_load(s: &mut DsqlSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    let table = table_name(&spec.table);
    let ncols = spec.columns.len();
    if ncols == 0 {
        return Err(Error::Query("la carga masiva no tiene columnas".into()));
    }
    let catalog = s.columns(&spec.table).await?;
    if catalog.is_empty() {
        return Err(Error::Query(format!("no se encontró la tabla {table}")));
    }
    let cols = pick(&catalog, Some(&spec.columns), &table)?;
    let identity = spec.keep_identity && cols.iter().any(|c| c.auto_increment);
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let plain_ts: Vec<bool> = cols.iter().map(|c| plain_timestamp(&c.data_type)).collect();
    let (max_rows, max_bytes) = window_limits(spec, ncols);

    let mut extra = Vec::new();
    for _ in 1..PARALLEL {
        match another(&s.reopen).await {
            Ok(x) => extra.push(x),
            Err(e) => {
                tracing::debug!("dsql: bulk load goes on with {} connection(s): {e}", extra.len() + 1);
                break;
            }
        }
    }
    let conns: Vec<&DsqlSession> = std::iter::once(&*s).chain(extra.iter()).collect();
    // One queued window: with one being built and one per connection (its
    // binds plus the encoded message), about 10 windows' bytes in memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Window>(1);
    let (stop_tx, stop_rx) = watch::channel(false);
    let first: Mutex<Option<Error>> = Mutex::new(None);
    let fail = |e: Error| {
        if let Ok(mut f) = first.lock() {
            f.get_or_insert(e);
        }
        stop_tx.send_replace(true);
    };
    let sh = Shared { rx: tokio::sync::Mutex::new(rx), head: insert_head(&table, &names, identity), ncols, done: Mutex::new(0), progress, stop: stop_rx.clone() };

    let stop = stop_rx.clone();
    // Owns the queue's sender: it closes when this ends, so the workers
    // finish what's queued and end.
    let produce = async move {
        let closed = || Error::State("la carga masiva se detuvo".into());
        let send = |w: Window| {
            let tx = &tx;
            let stop = stop.clone();
            async move {
                tokio::select! {
                    r = tx.send(w) => r.map_err(|_| closed()),
                    _ = stopped(stop) => Err(closed()),
                }
            }
        };
        let mut params: Vec<TextParam> = Vec::new();
        let (mut rows, mut bytes) = (0usize, 0u64);
        loop {
            let batch = tokio::select! {
                b = source.next() => b,
                _ = stopped(stop.clone()) => return Err(closed()),
            };
            let Some(batch) = batch else { break };
            for row in batch.rows {
                if row.len() != ncols {
                    return Err(Error::Query(format!("una fila trae {} valores y la carga espera {ncols}", row.len())));
                }
                let mut binds = Vec::with_capacity(ncols);
                let mut size = 0u64;
                for (i, c) in row.iter().enumerate() {
                    let b = bind_for(c, plain_ts[i], &names[i])?;
                    size += 4 + b.as_ref().map_or(0, |b| b.len() as u64);
                    binds.push(TextParam(b));
                }
                drop(row);
                if rows > 0 && bytes + size > max_bytes {
                    send(std::mem::take(&mut params)).await?;
                    (rows, bytes) = (0, 0);
                }
                params.extend(binds);
                rows += 1;
                bytes += size;
                if rows >= max_rows {
                    send(std::mem::take(&mut params)).await?;
                    (rows, bytes) = (0, 0);
                }
            }
        }
        if rows > 0 {
            send(params).await?;
        }
        Ok::<(), Error>(())
    };
    let produce = async {
        let r = produce.await;
        if let Err(e) = r {
            fail(e);
        }
    };
    let workers = futures::future::join_all(conns.iter().map(|c| {
        let (sh, fail) = (&sh, &fail);
        async move {
            if let Err(e) = worker(&c.client, (c.client.cancel_token(), c.tls.clone()), sh).await {
                fail(e);
            }
        }
    }));
    // Both run to the end: a failure stops new windows, and the ones in
    // flight are awaited, so nothing commits after this returns.
    futures::join!(produce, workers);
    drop(conns);
    drop(extra);
    if let Some(e) = first.lock().map_err(|_| Error::State("error de la carga".into()))?.take() {
        return Err(e);
    }
    let total = *sh.done.lock().map_err(|_| Error::State("progreso de la carga".into()))?;

    if identity {
        for c in cols.iter().filter(|c| c.auto_increment) {
            let sql = reseed_sql(&table, &c.name);
            if let Err(e) = s.client.query(&sql, &[&table, &c.name]).await {
                return Err(Error::Query(format!(
                    "se cargaron las filas pero no se pudo mover la secuencia de «{}» más allá del máximo cargado: {}",
                    c.name,
                    err(e)
                )));
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, primary_key: false, auto_increment: false, default_value: None }
    }

    fn spec(rows: u64, bytes: u64) -> LoadSpec {
        LoadSpec {
            table: ObjectRef { kind: "table".into(), schema: Some("app".into()), name: "t".into() },
            columns: vec!["a".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows: rows,
            commit_bytes: bytes,
        }
    }

    #[test]
    fn picks_columns_in_the_requested_order() {
        let cat = vec![col("id", "integer"), col("Name", "text"), col("m", "money")];
        let got = pick(&cat, Some(&["m".into(), "name".into()]), "t").unwrap();
        assert_eq!(got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["m", "Name"]);
        assert_eq!(pick(&cat, None, "t").unwrap().len(), 3);
        assert!(pick(&cat, Some(&["zz".into()]), "t").is_err());
        let sel = select_sql("\"app\".\"t\"", &got, Some(" id > 3 "));
        assert_eq!(sel, "SELECT \"m\"::numeric AS \"m\", \"Name\" FROM \"app\".\"t\" WHERE (id > 3)");
        assert_eq!(select_sql("t", &got[1..], Some("  ")), "SELECT \"Name\" FROM t");
    }

    #[test]
    fn kinds_follow_types() {
        assert_eq!(Kind::of(&Type::INT8), Kind::Int);
        assert_eq!(Kind::of(&Type::JSONB), Kind::Json);
        assert_eq!(Kind::of(&Type::TIMESTAMPTZ), Kind::Timestamptz);
        assert_eq!(Kind::of(&Type::INTERVAL), Kind::Text);
        assert_eq!(Kind::of(&Type::VARCHAR), Kind::Text);
    }

    #[test]
    fn text_values_become_typed_cells() {
        assert_eq!(text_cell(Kind::Int, None), Cell::Null);
        assert_eq!(text_cell(Kind::Bool, Some("t")), Cell::Bool(true));
        assert_eq!(text_cell(Kind::Bool, Some("f")), Cell::Bool(false));
        assert_eq!(text_cell(Kind::Int, Some("-9223372036854775808")), Cell::Int(i64::MIN));
        assert_eq!(text_cell(Kind::Float, Some("0.1")), Cell::Float(0.1));
        assert!(matches!(text_cell(Kind::Float, Some("NaN")), Cell::Float(f) if f.is_nan()));
        assert_eq!(text_cell(Kind::Float, Some("-Infinity")), Cell::Float(f64::NEG_INFINITY));
        assert_eq!(text_cell(Kind::Numeric, Some("-12345678901234567890.0001")), Cell::Decimal("-12345678901234567890.0001".into()));
        assert_eq!(text_cell(Kind::Numeric, Some("NaN")), Cell::Text("NaN".into()));
        assert_eq!(text_cell(Kind::Bytea, Some("\\xcafe00")), Cell::Bytes(vec![0xca, 0xfe, 0]));
        assert_eq!(text_cell(Kind::Bytea, Some("\\x")), Cell::Bytes(vec![]));
        assert_eq!(text_cell(Kind::Uuid, Some("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11")), Cell::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()));
        assert_eq!(text_cell(Kind::Date, Some("2024-02-29")), Cell::Date("2024-02-29".into()));
        assert_eq!(text_cell(Kind::Date, Some("0044-03-15 BC")), Cell::Text("0044-03-15 BC".into()));
        assert_eq!(text_cell(Kind::Date, Some("infinity")), Cell::Text("infinity".into()));
        assert_eq!(text_cell(Kind::Time, Some("13:45:00.123456")), Cell::Time("13:45:00.123456".into()));
        assert_eq!(text_cell(Kind::Timestamp, Some("2024-01-31 13:45:00")), Cell::DateTime("2024-01-31 13:45:00".into()));
        assert_eq!(text_cell(Kind::Timestamp, Some("2024-01-31 13:45:00.5 BC")), Cell::Text("2024-01-31 13:45:00.5 BC".into()));
        assert_eq!(text_cell(Kind::Timestamptz, Some("2024-01-31 13:45:00+00")), Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()));
        assert_eq!(text_cell(Kind::Timestamptz, Some("2024-01-31 13:45:00.25-03:30")), Cell::DateTimeTz("2024-01-31 13:45:00.25-03:30".into()));
        assert_eq!(text_cell(Kind::Timestamptz, Some("1850-01-01 00:00:00-04:16:48")), Cell::Text("1850-01-01 00:00:00-04:16:48".into()));
        assert_eq!(text_cell(Kind::Json, Some("{\"a\": 1}")), Cell::Json("{\"a\": 1}".into()));
        assert_eq!(text_cell(Kind::Text, Some("hola")), Cell::Text("hola".into()));
    }

    #[test]
    fn decimals_are_plain_digits() {
        for ok in ["0", "-1.5", "12.", ".5", "100000000000000000000000000000.000001"] {
            assert!(plain_decimal(ok), "{ok}");
        }
        for bad in ["", "-", ".", "1e5", "NaN", "Infinity", "1.2.3", "--1"] {
            assert!(!plain_decimal(bad), "{bad}");
        }
    }

    #[test]
    fn cells_bind_as_postgres_input_text() {
        assert_eq!(bind_text(&Cell::Null), None);
        assert_eq!(bind_text(&Cell::Bool(true)).as_deref(), Some("true"));
        assert_eq!(bind_text(&Cell::Int(-5)).as_deref(), Some("-5"));
        assert_eq!(bind_text(&Cell::UInt(u64::MAX)).as_deref(), Some("18446744073709551615"));
        assert_eq!(bind_text(&Cell::Float(0.1)).as_deref(), Some("0.1"));
        assert_eq!(bind_text(&Cell::Float(1e300)).unwrap().parse::<f64>().unwrap(), 1e300);
        assert_eq!(bind_text(&Cell::Float(f64::NAN)).as_deref(), Some("NaN"));
        assert_eq!(bind_text(&Cell::Float(f64::NEG_INFINITY)).as_deref(), Some("-Infinity"));
        assert_eq!(bind_text(&Cell::Bytes(vec![0xCA, 0xFE, 0x01])).as_deref(), Some("\\xcafe01"));
        assert_eq!(bind_text(&Cell::Decimal("1.50".into())).as_deref(), Some("1.50"));
        assert_eq!(bind_text(&Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into())).as_deref(), Some("2024-01-31 13:45:00+00:00"));
        assert_eq!(bind_text(&Cell::Json("[1,2]".into())).as_deref(), Some("[1,2]"));
    }

    #[test]
    fn text_params_go_in_text_format() {
        let p = TextParam(Some("12".into()));
        assert!(matches!(p.encode_format(&Type::INT4), Format::Text));
        let mut buf = BytesMut::new();
        assert!(matches!(p.to_sql(&Type::INT4, &mut buf).unwrap(), IsNull::No));
        assert_eq!(&buf[..], b"12");
        assert!(matches!(TextParam(None).to_sql(&Type::INT4, &mut buf).unwrap(), IsNull::Yes));
        assert!(TextParam::accepts(&Type::BYTEA));
    }

    #[test]
    fn windows_respect_dsql_limits() {
        // The spec's window, capped at 3,000 rows and 4 MiB.
        assert_eq!(window_limits(&spec(100_000, 512 << 20), 5), (3_000, MAX_TX_BYTES));
        assert_eq!(window_limits(&spec(500, 1 << 20), 5), (500, 1 << 20));
        // 0: no limit of its own.
        assert_eq!(window_limits(&spec(0, 0), 5), (3_000, MAX_TX_BYTES));
        // Wide rows: the binds per statement bound it.
        assert_eq!(window_limits(&spec(3_000, 0), 100).0, 655);
        assert_eq!(window_limits(&spec(3_000, 0), 70_000).0, 1);
    }

    #[test]
    fn insert_statements() {
        let head = insert_head("\"app\".\"t\"", &["id".into(), "Nombre".into()], false);
        assert_eq!(head, "INSERT INTO \"app\".\"t\" (\"id\", \"Nombre\") VALUES ");
        assert_eq!(insert_sql(&head, 2, 3), format!("{head}($1, $2), ($3, $4), ($5, $6)"));
        assert_eq!(insert_sql("I ", 1, 1), "I ($1)");
        let id = insert_head("t", &["id".into()], true);
        assert_eq!(id, "INSERT INTO t (\"id\") OVERRIDING SYSTEM VALUE VALUES ");
    }

    #[test]
    fn occ_conflicts_are_retried() {
        assert!(retryable(Some("OC000")));
        assert!(retryable(Some("OC001")));
        assert!(retryable(Some("40001")));
        assert!(!retryable(Some("23505")));
        assert!(!retryable(None));
        assert!(backoff(1) >= Duration::from_millis(100) && backoff(1) < Duration::from_millis(200));
        assert!(backoff(20) < Duration::from_millis(6_400));
    }

    #[test]
    fn windows_too_big_are_split() {
        // DSQL's size / row limits and PostgreSQL's index row size: class 54.
        assert!(too_big(Some("54000")));
        assert!(too_big(Some("54001")));
        assert!(!too_big(Some("OC000")));
        assert!(!too_big(Some("23505")));
        assert!(!too_big(None));
        assert_eq!(half(3_000), 1_500);
        assert_eq!(half(3), 2);
        assert_eq!(half(2), 1);
    }

    #[test]
    fn time_24_00_stays_text() {
        assert_eq!(text_cell(Kind::Time, Some("24:00:00")), Cell::Text("24:00:00".into()));
        assert_eq!(text_cell(Kind::Time, Some("23:59:59.999999")), Cell::Time("23:59:59.999999".into()));
        assert_eq!(text_cell(Kind::Time, Some("00:00:00")), Cell::Time("00:00:00".into()));
    }

    #[test]
    fn reads_go_in_keyset_pages() {
        let base = "SELECT \"a\", \"id\" FROM t";
        let key = vec!["id".to_string()];
        assert_eq!(page_sql(base, None, &key, None, 256), "SELECT \"a\", \"id\" FROM t ORDER BY \"id\" LIMIT 256");
        assert_eq!(
            page_sql(base, Some(" a > 1 "), &key, Some(&["41".into()]), 10),
            "SELECT \"a\", \"id\" FROM t WHERE (a > 1) AND (\"id\") > (E'41') ORDER BY \"id\" LIMIT 10"
        );
        let key2 = vec!["k1".to_string(), "K 2".to_string()];
        assert_eq!(
            page_sql(base, None, &key2, Some(&["it's".into(), "a\\b".into()]), 5),
            "SELECT \"a\", \"id\" FROM t WHERE (\"k1\", \"K 2\") > (E'it''s', E'a\\\\b') ORDER BY \"k1\", \"K 2\" LIMIT 5"
        );
        // Pages sized to ~8 MiB, growing at most 4× at a time.
        assert_eq!(next_limit(256, 256, 256 * 100), 1_024);
        assert_eq!(next_limit(20_000, 20_000, 20_000 * 10), MAX_PAGE_ROWS);
        assert_eq!(next_limit(256, 2, 2 * (6 << 20)), 1);
        assert_eq!(next_limit(256, 256, 256 * (1 << 20)), 8);
        assert_eq!(next_limit(7, 0, 0), 7);
    }

    #[test]
    fn offsets_are_not_dropped_into_plain_timestamps() {
        assert!(plain_timestamp("timestamp without time zone"));
        assert!(plain_timestamp("timestamp(3) without time zone"));
        assert!(plain_timestamp("timestamp"));
        assert!(!plain_timestamp("timestamp with time zone"));
        assert!(!plain_timestamp("timestamp(6) with time zone"));
        assert!(!plain_timestamp("text"));
        let tz = |v: &str| Cell::DateTimeTz(v.into());
        assert_eq!(bind_for(&tz("2024-01-31 13:45:00+00:00"), true, "c").unwrap().as_deref(), Some("2024-01-31 13:45:00"));
        let e = bind_for(&tz("2024-01-31 13:45:00-03:00"), true, "c").unwrap_err();
        assert!(e.to_string().contains("timestamp sin zona"), "{e}");
        // A timestamptz column takes the offset.
        assert_eq!(bind_for(&tz("2024-01-31 13:45:00-03:00"), false, "c").unwrap().as_deref(), Some("2024-01-31 13:45:00-03:00"));
        assert_eq!(bind_for(&Cell::DateTime("2024-01-31 13:45:00".into()), true, "c").unwrap().as_deref(), Some("2024-01-31 13:45:00"));
    }

    #[test]
    fn identity_sequences_move_past_the_loaded_values() {
        let sql = reseed_sql("\"app\".\"t\"", "Id");
        assert!(sql.contains("pg_get_serial_sequence($1, $2)"), "{sql}");
        assert!(sql.contains("max(\"Id\")::int8 AS hi, min(\"Id\")::int8 AS lo FROM \"app\".\"t\""), "{sql}");
        assert!(sql.contains("p.last_value < x.hi"), "{sql}");
    }
}

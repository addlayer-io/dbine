//! Bulk transfer (see `dbine_driver::transfer`) for SQLite.
//!
//! - **Read** ([`read`]): one `SELECT` stepped on the session's blocking
//!   thread; each value becomes the cell of its storage class (INTEGER →
//!   `Int`, REAL → `Float`, TEXT → `Text`, BLOB → `Bytes`, whole). Text that
//!   isn't valid UTF-8 (SQLite doesn't check) goes as `Bytes` so nothing is
//!   lost. Declared types travel in the columns' `type_name`.
//! - **Load** ([`bulk_load`]): one prepared `INSERT` reused for every row,
//!   inside a `BEGIN IMMEDIATE … COMMIT` per commit window. SQLite locks the
//!   whole database for a writer, so `table_lock` needs nothing more, and an
//!   `INTEGER PRIMARY KEY` always takes the given value (`keep_identity`).
//!   Loads into the same file from this process take turns per window
//!   ([`Gate`]) instead of racing for the lock under the busy timeout, and
//!   no window keeps the file locked past [`HOLD`] (it commits early, also
//!   while it waits on a slow source), then the file stays free for
//!   [`GAP`]: other connections' writes (a finished table's indexes,
//!   another table emptied) get in before their busy timeout instead of
//!   failing with "database is locked". A `BEGIN IMMEDIATE` behind such a
//!   write waits it out and then lets the file stay free until a whole
//!   [`GAP`] goes by without another one ([`begin`]), so several of them
//!   waiting at once all get in. A dropped load commits nothing after
//!   the drop ([`Abort`]). A `NaN` is refused: SQLite would store it as
//!   NULL. A read of the same file open meanwhile (the source of a copy
//!   by batches inside one file) keeps a rollback-journal file from
//!   committing: the load fails saying so and that WAL mode avoids it.
//!   The only `PRAGMA` touched is `cache_size` (a bigger page cache while
//!   loading, restored at the end): it lives in the connection's memory and
//!   doesn't change durability. `synchronous` and `journal_mode` stay as
//!   they are, so a committed window survives a crash exactly as any
//!   other commit on that file.
//! - **Native copy** ([`copy_native`]): the target connection attaches the
//!   source file read-only (`mode=ro` URI) and runs
//!   `INSERT INTO … SELECT …` by rowid ranges; rows never leave SQLite.
//!   The ranges are sized to take about [`STEP`] each and go in windows
//!   under the same turns and [`HOLD`] bound as a load, each window's
//!   commit reported as progress. A single statement (SQLite's transfer
//!   optimisation) would keep the target file locked for the whole table.
//!   A source without a usable rowid (a view, a `WITHOUT ROWID` table)
//!   goes in one statement, and if that outlasts [`HOLD`] it's interrupted,
//!   rolled back and `Unsupported`. A read filter goes along when the
//!   source is in the target's file and both connections see the same
//!   files under the same names (its names resolve alike); otherwise it
//!   would resolve against the target's tables and it's `Unsupported`, as
//!   is a file SQLite can't attach: those go by batches. The ranges are read in separate
//!   transactions: rows another process writes into the source meanwhile
//!   may or may not be copied (a read by batches sees one snapshot).

use crate::{err, SqliteSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result, Session};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, ErrorCode, InterruptHandle, ToSql};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Batches queued between the async side and the loading thread.
const QUEUE: usize = 4;
/// The page cache while loading (negative: KiB), 64 MiB.
const LOAD_CACHE: i64 = -65_536;
/// Longest stretch this process's transfers keep a file write-locked
/// without a break: well under the sessions' busy timeout (5 s), so a
/// write from another connection that arrives meanwhile still gets in.
pub const HOLD: Duration = Duration::from_secs(2);
/// The break after a [`HOLD`] stretch: longer than the 100 ms between the
/// retries of SQLite's busy handler, so a waiting writer sees the file
/// free at least once.
pub const GAP: Duration = Duration::from_millis(150);
/// What a native copy's rowid range aims to take.
const STEP: Duration = Duration::from_millis(100);
/// How long a `BEGIN IMMEDIATE` keeps waiting on another connection's
/// write (an index being built can take minutes).
const BEGIN_PATIENCE: Duration = Duration::from_secs(600);

/// `"schema"."name"` (just `"name"` without a schema).
pub fn table_name(t: &ObjectRef) -> String {
    match t.schema() {
        Some(s) => format!("{}.{}", quote_ident(Quote::Double, s), quote_ident(Quote::Double, &t.name)),
        None => quote_ident(Quote::Double, &t.name),
    }
}

/// `"a", "b"`.
pub fn column_list(cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ")
}

/// `t."a" AS "a", t."b" AS "b"`: each column qualified by `table`. A bare
/// `"nope"` that names no column is read by SQLite as the string literal
/// `'nope'` (its double-quoted-string fallback); a qualified one is always
/// a column reference, so an unknown column is an error.
pub fn qualified_columns(table: &str, cols: &[String]) -> String {
    cols.iter()
        .map(|c| {
            let q = quote_ident(Quote::Double, c);
            format!("{table}.{q} AS {q}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The read's `SELECT`: the spec's columns (all of them when `None`) and
/// its filter.
pub fn select_sql(spec: &ReadSpec) -> String {
    let table = table_name(&spec.table);
    let cols = spec.columns.as_deref().map_or_else(|| "*".to_string(), |c| qualified_columns(&table, c));
    match spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("SELECT {cols} FROM {table} WHERE {f}"),
        None => format!("SELECT {cols} FROM {table}"),
    }
}

/// `INSERT INTO t (cols) VALUES (?, …)` with `rows` rows of placeholders.
pub fn insert_sql(spec: &LoadSpec, rows: usize) -> String {
    let one = format!("({})", vec!["?"; spec.columns.len()].join(", "));
    format!("INSERT INTO {} ({}) VALUES {}", table_name(&spec.table), column_list(&spec.columns), vec![one; rows].join(", "))
}

/// A SQLite value as a cell, whole.
pub fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Int(i),
        ValueRef::Real(f) => Cell::Float(f),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => Cell::Text(s.to_string()),
            Err(_) => Cell::Bytes(t.to_vec()),
        },
        ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
    }
}

/// A cell bound as a statement parameter: integers, reals and blobs as
/// such, everything else as text for the column's affinity to convert
/// (`Decimal` into a NUMERIC column becomes a number, a date stays ISO
/// text). A `UInt` above `i64::MAX` goes as its digits: SQLite has no
/// unsigned 64-bit integer and a REAL would round it.
pub struct Bind<'a>(pub &'a Cell);

impl ToSql for Bind<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        use rusqlite::types::Value;
        Ok(match self.0 {
            Cell::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Cell::Bool(b) => ToSqlOutput::Borrowed(ValueRef::Integer(*b as i64)),
            Cell::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Cell::UInt(u) => match i64::try_from(*u) {
                Ok(i) => ToSqlOutput::Borrowed(ValueRef::Integer(i)),
                Err(_) => ToSqlOutput::Owned(Value::Text(u.to_string())),
            },
            Cell::Float(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            Cell::Bytes(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Cell::Decimal(s)
            | Cell::Text(s)
            | Cell::Date(s)
            | Cell::Time(s)
            | Cell::DateTime(s)
            | Cell::DateTimeTz(s)
            | Cell::Uuid(s)
            | Cell::Json(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
        })
    }
}

/// `name → NOT NULL` of a table's columns (empty for a view or a query
/// SQLite can't describe). A primary key column only counts as NOT NULL
/// where SQLite enforces it: in a `WITHOUT ROWID` table, or as the rowid
/// alias (`INTEGER PRIMARY KEY`, the one key without a `pk` index). Any
/// other key of a rowid table accepts NULL.
fn not_null(c: &Connection, t: &ObjectRef) -> HashMap<String, bool> {
    let schema = t.schema().unwrap_or("main");
    let Ok(mut stmt) = c.prepare(
        "SELECT name, \"notnull\" OR (pk > 0 AND (
             coalesce((SELECT wr FROM pragma_table_list WHERE schema = ?2 AND name = ?1), 0)
             OR NOT EXISTS (SELECT 1 FROM pragma_index_list(?1, ?2) WHERE origin = 'pk')))
         FROM pragma_table_info(?1, ?2)",
    ) else {
        return HashMap::new();
    };
    stmt.query_map([t.name.as_str(), schema], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Read `spec` into `sink` on this (blocking) thread; the rows read.
pub(crate) fn read(c: &Connection, spec: &ReadSpec, sink: &BatchSinkRef) -> Result<u64> {
    let mut stmt = c.prepare(&select_sql(spec)).map_err(err)?;
    let not_null = not_null(c, &spec.table);
    let cols: Vec<TransferColumn> = stmt
        .columns()
        .iter()
        .map(|col| TransferColumn {
            name: col.name().to_string(),
            type_name: col.decl_type().unwrap_or("").to_string(),
            nullable: !not_null.get(col.name()).copied().unwrap_or(false),
        })
        .collect();
    let lock = || sink.lock().map_err(|_| Error::State("destino de lotes".into()));
    lock()?.begin(&cols)?;
    let n = cols.len();
    let mut builder = BatchBuilder::new();
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next().map_err(err)? {
        let cells = (0..n).map(|i| cell(row.get_ref_unwrap(i))).collect();
        builder.push(cells, &mut *lock()?)?;
    }
    builder.flush(&mut *lock()?)?;
    Ok(builder.rows)
}

/// Set once the caller gave up on a load or copy (its future was dropped).
/// Every `COMMIT` runs under this lock after checking it, and the drop
/// takes the lock to set it: once the drop returns, nothing commits.
#[derive(Default)]
struct Abort(Mutex<bool>);

impl Abort {
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_set(&self) -> bool {
        *self.lock()
    }

    /// `COMMIT`, unless the caller is gone (then the caller rolls back).
    /// It only finds the file busy when another connection keeps a read
    /// open on a rollback-journal file past the busy timeout.
    fn commit(&self, c: &Connection) -> Result<()> {
        let aborted = self.lock();
        if *aborted {
            return Err(Error::Cancelled);
        }
        c.execute_batch("COMMIT").map_err(|e| match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy) => Error::Query(format!(
                "no se pudo confirmar: otra conexión mantiene abierta una lectura de este archivo (por ejemplo, el origen de \
                 esta copia está en el mismo archivo y se lee por lotes). Con el diario de reversión (journal_mode DELETE, \
                 TRUNCATE o PERSIST) SQLite no confirma mientras dure esa lectura; en modo WAL sí (PRAGMA journal_mode = WAL) ({e})"
            )),
            _ => err(e),
        })
    }
}

/// Sets [`Abort`] when dropped (the future holding it was dropped, or it
/// ended), and interrupts the statement in flight while `interrupt` is set.
struct AbortOnDrop {
    abort: Arc<Abort>,
    interrupt: Option<Arc<InterruptHandle>>,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        *self.abort.lock() = true;
        if let Some(h) = &self.interrupt {
            h.interrupt();
        }
    }
}

/// The write turn on one database file. SQLite has a single writer per
/// file and the migration loads several tables into it at once, each on
/// its own connection: without a turn, a table's `BEGIN IMMEDIATE` waits
/// out the busy timeout behind another table's window and fails with
/// "database is locked". The turns are taken in arrival order.
///
/// Writes that don't go through the gate (another connection's
/// `CREATE INDEX`, `DELETE`…) still wait under their busy timeout, so the
/// turns in a row form a stretch of at most [`HOLD`] (each turn's
/// deadline) followed by a [`GAP`] with the file free.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

#[derive(Default)]
struct GateState {
    held: bool,
    next: u64,
    queue: VecDeque<u64>,
    /// When the current stretch of turns began.
    stretch: Option<Instant>,
    /// When the last turn ended.
    released: Option<Instant>,
}

/// The gate of `file` (none for an in-memory or temporary database, which
/// no other connection writes).
fn gate(file: &str) -> Option<Arc<Gate>> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<Gate>>>> = OnceLock::new();
    if file.is_empty() {
        return None;
    }
    let key = std::fs::canonicalize(file).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| file.to_string());
    let mut gates = GATES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    Some(gates.entry(key).or_default().clone())
}

/// A held write turn, released when dropped.
struct Turn {
    gate: Option<Arc<Gate>>,
    /// When the holder must have committed and let go (none without a
    /// gate: nobody else writes the database).
    deadline: Option<Instant>,
}

impl Turn {
    /// Wait for the write turn of `gate` (right away without one); gives
    /// up when `abort` is set. `fresh`: the turn starts a stretch of its
    /// own (the whole [`HOLD`]) instead of continuing the current one.
    fn take(gate: &Option<Arc<Gate>>, abort: &Abort, fresh: bool) -> Result<Turn> {
        let Some(g) = gate else { return Ok(Turn { gate: None, deadline: None }) };
        let mut s = g.state.lock().unwrap_or_else(|e| e.into_inner());
        let me = s.next;
        s.next += 1;
        s.queue.push_back(me);
        loop {
            let mut wait = Duration::from_millis(100);
            if !s.held && s.queue.front() == Some(&me) {
                let now = Instant::now();
                // The last turn ended less than a gap ago: the stretch goes on.
                let gap_left = s.released.and_then(|r| GAP.checked_sub(now.saturating_duration_since(r))).filter(|d| !d.is_zero());
                let start = match (gap_left, s.stretch) {
                    (Some(_), Some(start)) => start,
                    _ => now,
                };
                match gap_left {
                    Some(left) if fresh || now.saturating_duration_since(start) >= HOLD => wait = wait.min(left),
                    _ => {
                        s.queue.pop_front();
                        s.held = true;
                        s.stretch = Some(start);
                        return Ok(Turn { gate: Some(g.clone()), deadline: Some(start + HOLD) });
                    }
                }
            }
            if abort.is_set() {
                s.queue.retain(|&t| t != me);
                g.cv.notify_all();
                return Err(Error::Cancelled);
            }
            s = g.cv.wait_timeout(s, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The turn's time is up: commit and let go.
    fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// The holder only got the file now (it waited on other connections'
    /// writes): its stretch starts now.
    fn restart(&mut self) {
        if let Some(g) = &self.gate {
            let now = Instant::now();
            g.state.lock().unwrap_or_else(|e| e.into_inner()).stretch = Some(now);
            self.deadline = Some(now + HOLD);
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(g) = &self.gate {
            let mut s = g.state.lock().unwrap_or_else(|e| e.into_inner());
            s.held = false;
            s.released = Some(Instant::now());
            drop(s);
            g.cv.notify_all();
        }
    }
}

/// `BEGIN IMMEDIATE` on the file behind `schema`, waiting out the writes
/// other connections have in flight or waiting, until `abort` is set or
/// [`BEGIN_PATIENCE`] runs out. True when it had to wait.
///
/// SQLite's busy handler isn't first come, first served: right after
/// another connection's write, a `BEGIN` that just started polls more
/// often than writers that have waited a while, so it would take the file
/// ahead of all of them and keep them waiting a whole [`HOLD`] each, past
/// their busy timeout. So once it had to wait (the `BEGIN` blocked, or
/// another connection committed), it lets go and only keeps the file once
/// a whole [`GAP`] went by without another connection's commit: longer
/// than their busy handlers' longest sleep, so none was left waiting.
fn begin(c: &Connection, schema: &str, abort: &Abort) -> Result<bool> {
    let version_sql = format!("PRAGMA {}.data_version", quote_ident(Quote::Double, schema));
    // Changes when another connection commits into the file.
    let version = || c.query_row(&version_sql, [], |r| r.get::<_, i64>(0)).map_err(err);
    let start = Instant::now();
    let mut waited = false;
    let mut seen = version()?;
    loop {
        let t = Instant::now();
        match c.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                // Its busy handler's first sleep is 1 ms: a `BEGIN` that
                // took that long waited on someone.
                let blocked = t.elapsed() >= Duration::from_millis(1);
                if !blocked && version()? == seen {
                    return Ok(waited);
                }
                c.execute_batch("ROLLBACK").map_err(err)?;
            }
            Err(e) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
                if start.elapsed() >= BEGIN_PATIENCE {
                    return Err(Error::Query(format!(
                        "otra conexión mantiene bloqueado el archivo desde hace más de {} minutos: {e}",
                        BEGIN_PATIENCE.as_secs() / 60
                    )));
                }
            }
            Err(e) => return Err(err(e)),
        }
        waited = true;
        if abort.is_set() {
            return Err(Error::Cancelled);
        }
        seen = version()?;
        std::thread::sleep(GAP);
    }
}

/// What the async side hands the loading thread.
enum Msg {
    Batch(RowBatch),
    /// The source ended: commit what's pending. Without it (the load was
    /// dropped halfway), the open window rolls back.
    End,
}

/// Load on this (blocking) thread: `rx`'s batches through one prepared
/// `INSERT`, a transaction per window (holding the file's write turn,
/// committed early when the turn's time is up, even while waiting on the
/// source); each commit's running total goes to `committed`. Once `abort`
/// is set nothing more commits: the batches still queued are dropped and
/// the open window rolls back.
fn load(c: &Connection, spec: &LoadSpec, mut rx: mpsc::Receiver<Msg>, committed: mpsc::UnboundedSender<u64>, abort: &Abort) -> Result<u64> {
    let max_rows = if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows };
    let max_bytes = if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes };
    let ncols = spec.columns.len();
    let schema = spec.table.schema().unwrap_or("main");
    let gate = gate(&file_of(c, schema)?);
    let rt = tokio::runtime::Handle::current();
    let cache: i64 = c.query_row("PRAGMA cache_size", [], |r| r.get(0)).map_err(err)?;
    c.execute_batch(&format!("PRAGMA cache_size = {LOAD_CACHE}")).map_err(err)?;
    // The open window's write turn (`None`: no window open).
    let mut window: Option<Turn> = None;
    let mut run = || -> Result<u64> {
        let mut stmt = c.prepare(&insert_sql(spec, 1)).map_err(err)?;
        let (mut total, mut rows, mut bytes) = (0u64, 0u64, 0u64);
        loop {
            let deadline = window.as_ref().and_then(|t| t.deadline);
            let msg = match (rx.try_recv(), deadline) {
                (Ok(m), _) => Some(m),
                (Err(mpsc::error::TryRecvError::Disconnected), _) => None,
                // The file is locked meanwhile: wait no longer than the turn.
                (Err(_), Some(d)) => match rt.block_on(tokio::time::timeout_at(d.into(), rx.recv())) {
                    Ok(m) => m,
                    Err(_) => {
                        abort.commit(c)?;
                        window = None;
                        total += rows;
                        (rows, bytes) = (0, 0);
                        let _ = committed.send(total);
                        continue;
                    }
                },
                (Err(_), None) => rx.blocking_recv(),
            };
            let batch = match msg {
                Some(Msg::Batch(b)) => b,
                Some(Msg::End) => break,
                None => return Err(Error::Cancelled),
            };
            if abort.is_set() {
                return Err(Error::Cancelled);
            }
            for row in &batch.rows {
                let at = total + rows + 1;
                if row.len() != ncols {
                    return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {ncols} columnas", row.len())));
                }
                // SQLite stores a NaN REAL as NULL without complaint.
                if let Some(i) = row.iter().position(|v| matches!(v, Cell::Float(f) if f.is_nan())) {
                    return Err(Error::Query(format!(
                        "fila {at}, columna \"{}\": SQLite no admite NaN (lo guardaría como NULL)",
                        spec.columns[i]
                    )));
                }
                if window.is_none() {
                    let mut turn = Turn::take(&gate, abort, false)?;
                    if begin(c, schema, abort)? {
                        turn.restart();
                    }
                    window = Some(turn);
                }
                for (i, v) in row.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 1, Bind(v)).map_err(err)?;
                }
                stmt.raw_execute().map_err(|e| match err(e) {
                    Error::Query(m) => Error::Query(format!("fila {at}: {m}")),
                    other => other,
                })?;
                rows += 1;
                bytes += row.iter().map(Cell::size).sum::<usize>() as u64;
                if rows >= max_rows || bytes >= max_bytes || window.as_ref().is_some_and(Turn::expired) {
                    abort.commit(c)?;
                    window = None;
                    total += rows;
                    (rows, bytes) = (0, 0);
                    let _ = committed.send(total);
                }
            }
        }
        if window.is_some() {
            abort.commit(c)?;
            window = None;
            total += rows;
            let _ = committed.send(total);
        }
        Ok(total)
    };
    let r = run();
    if r.is_err() && !c.is_autocommit() {
        let _ = c.execute_batch("ROLLBACK");
    }
    drop(window);
    let _ = c.execute_batch(&format!("PRAGMA cache_size = {cache}"));
    r
}

/// Feed `source` to `load` running on a blocking thread, reporting its
/// commits to `progress` as they happen. Dropping this future (a failed
/// read or a cancellation) stops the load without committing anything
/// more.
pub(crate) async fn bulk_load(s: &SqliteSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    let (tx, rx) = mpsc::channel::<Msg>(QUEUE);
    let (ctx, mut crx) = mpsc::unbounded_channel::<u64>();
    let abort = Arc::new(Abort::default());
    let _abort_on_drop = AbortOnDrop { abort: abort.clone(), interrupt: None };
    let conn = s.conn.clone();
    let spec_owned = spec.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let c = conn.lock().map_err(|_| Error::State("conexión SQLite envenenada".into()))?;
        load(&c, &spec_owned, rx, ctx, &abort)
    });
    'feed: while let Some(batch) = source.next().await {
        // Report commits while waiting for room in the queue.
        loop {
            tokio::select! {
                permit = tx.reserve() => match permit {
                    Ok(p) => {
                        p.send(Msg::Batch(batch));
                        break;
                    }
                    // The loader stopped (an error): its result says why.
                    Err(_) => break 'feed,
                },
                Some(n) = crx.recv() => progress(n),
            }
        }
    }
    let _ = tx.send(Msg::End).await;
    drop(tx);
    while let Some(n) = crx.recv().await {
        progress(n);
    }
    worker.await.map_err(|e| Error::State(e.to_string()))?
}

/// `file:<path>?mode=ro`: the path as an SQLite URI, opened read-only.
fn read_only_uri(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    // Windows' `\\?\C:\x` and `\\?\UNC\server\share\x` spell the plain
    // `C:\x` and `\\server\share\x`.
    if let Some(rest) = p.strip_prefix("//?/UNC/") {
        p = format!("//{rest}");
    } else if let Some(rest) = p.strip_prefix("//?/") {
        p = rest.to_string();
    }
    // `C:/x` → `/C:/x`, as SQLite's URIs spell Windows drives.
    if p.as_bytes().get(1) == Some(&b':') {
        p.insert(0, '/');
    }
    // A UNC path (`//server/share/x`) after an empty authority:
    // `file://server/…` would name `server` as the URI's host, which SQLite
    // rejects.
    if p.starts_with("//") {
        p.insert_str(0, "//");
    }
    let mut out = String::from("file:");
    for ch in p.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            c => out.push(c),
        }
    }
    out.push_str("?mode=ro");
    out
}

fn table_columns(c: &Connection, schema: &str, name: &str) -> Result<Vec<String>> {
    let mut stmt = c.prepare("SELECT name FROM pragma_table_info(?1, ?2) ORDER BY cid").map_err(err)?;
    let rows = stmt.query_map([name, schema], |r| r.get::<_, String>(0)).map_err(err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)
}

fn session(s: &mut dyn Session) -> Option<&mut SqliteSession> {
    s.as_any()?.downcast_mut::<SqliteSession>()
}

/// The file behind `schema` on `c` (empty: in memory or temporary).
fn file_of(c: &Connection, schema: &str) -> Result<String> {
    c.query_row("SELECT file FROM pragma_database_list WHERE name = ?1", [schema], |r| r.get::<_, Option<String>>(0))
        .map(Option::unwrap_or_default)
        .map_err(err)
}

const SRC: &str = "dbine_copy_src";

/// What a read filter's names resolve against on `c`: its files by schema
/// name (`temp` aside), and whether it has temporary objects, which could
/// shadow them.
fn scope(c: &Connection) -> Result<(Vec<(String, String)>, bool)> {
    let mut stmt = c.prepare("SELECT name, coalesce(file, '') FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq").map_err(err)?;
    let dbs = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.collect::<rusqlite::Result<Vec<_>>>().map_err(err)?;
    let temp: i64 = c.query_row("SELECT count(*) FROM temp.sqlite_master", [], |r| r.get(0)).map_err(err)?;
    Ok((dbs, temp > 0))
}

/// `INSERT INTO target SELECT … FROM source` on the target's connection,
/// the source file attached read-only (see the module docs), by rowid
/// ranges in windows that hold the target file's write turn; each
/// window's commit goes to `progress`. Dropping this future interrupts the
/// statement and nothing commits after the drop.
pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: &str| Err(Error::Unsupported(format!("copia directa no disponible: {why}")));
    let filter = spec.source.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(str::to_string);
    let Some(src) = session(source) else { return unsupported("el origen no es una sesión SQLite") };
    let src_schema = spec.source.table.schema().unwrap_or("main").to_string();
    let schema = src_schema.clone();
    let (src_file, src_scope) = src.with(move |c| Ok((file_of(c, &schema)?, scope(c)?))).await?;
    if src_file.is_empty() {
        return unsupported("la base de origen está en memoria");
    }
    let Some(dst) = session(target) else { return unsupported("el destino no es una sesión SQLite") };
    let spec = spec.clone();
    let abort = Arc::new(Abort::default());
    let mut abort_on_drop = AbortOnDrop { abort: abort.clone(), interrupt: Some(dst.interrupt.clone()) };
    let interrupt = dst.interrupt.clone();
    let (ctx, mut crx) = mpsc::unbounded_channel::<u64>();
    let work = dst.with(move |c| {
        if !c.is_autocommit() {
            return Err(Error::Unsupported("copia directa no disponible: el destino tiene una transacción abierta".into()));
        }
        let dst_schema = spec.target.table.schema().unwrap_or("main");
        let dst_file = file_of(c, dst_schema)?;
        let same = |a: &str, b: &str| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        };
        // A file this connection already has open is read through its own
        // schema (the source's name if it names that file here, else the
        // target's): attaching it twice would have the connection wait on
        // its own lock.
        let own = if same(&src_file, &file_of(c, &src_schema).unwrap_or_default()) {
            Some(src_schema.clone())
        } else if same(&src_file, &dst_file) {
            Some(dst_schema.to_string())
        } else {
            None
        };
        let attached = if own.is_some() {
            None
        } else {
            // A file SQLite can't attach (a path its URIs can't spell, a
            // format it can't read) is read by batches instead.
            c.execute("ATTACH DATABASE ?1 AS dbine_copy_src", [read_only_uri(&src_file)])
                .map_err(|e| Error::Unsupported(format!("copia directa no disponible: no se pudo adjuntar el archivo de origen ({e})")))?;
            Some(SRC)
        };
        let gate = gate(&dst_file);
        let run = || -> Result<u64> {
            let from_schema = own.as_deref().or(attached).unwrap_or(SRC);
            let from = format!("{}.{}", quote_ident(Quote::Double, from_schema), quote_ident(Quote::Double, &spec.source.table.name));
            // A filter runs here, on the target's connection: only where
            // its names resolve as on the source's, and only as one
            // condition (it's combined with the ranges).
            let filter = match &filter {
                None => None,
                Some(f) => {
                    let (dbs, temp) = scope(c)?;
                    let alike = own.as_deref() == Some(src_schema.as_str())
                        && !temp
                        && !src_scope.1
                        && dbs.len() == src_scope.0.len()
                        && dbs.iter().zip(&src_scope.0).all(|((n, f), (sn, sf))| n == sn && !f.is_empty() && same(f, sf));
                    if !alike {
                        return Err(Error::Unsupported(
                            "copia directa no disponible: la lectura tiene un filtro, que se evalúa leyendo el origen por lotes".into(),
                        ));
                    }
                    // Alone and in parentheses: a `)` that would close them
                    // early doesn't parse alone.
                    let parses = |w: &str| c.prepare(&format!("SELECT 1 FROM {from} WHERE {w}")).map(drop);
                    if let Err(e) = parses(f).and_then(|()| parses(&format!("({f})"))) {
                        return Err(Error::Unsupported(format!(
                            "copia directa no disponible: el filtro no es una sola condición ({e})"
                        )));
                    }
                    Some(f.as_str())
                }
            };
            let into = format!("{}.{}", quote_ident(Quote::Double, dst_schema), quote_ident(Quote::Double, &spec.target.table.name));
            let src_all = table_columns(c, from_schema, &spec.source.table.name)?;
            let dst_all = table_columns(c, dst_schema, &spec.target.table.name)?;
            if src_all.is_empty() || dst_all.is_empty() {
                return Err(Error::Unsupported("copia directa no disponible: origen o destino no es una tabla".into()));
            }
            let src_cols = spec.source.columns.clone().unwrap_or_else(|| src_all.clone());
            if src_cols.len() != spec.target.columns.len() {
                return Err(Error::Query(format!(
                    "el origen lee {} columnas y el destino carga {}",
                    src_cols.len(),
                    spec.target.columns.len()
                )));
            }
            let insert = if src_cols == src_all && spec.target.columns == dst_all && src_all.len() == dst_all.len() {
                // The columns by name, not `*`: `*` also brings the
                // computed (GENERATED) ones, which `table_info` leaves out
                // and INSERT doesn't take.
                format!("INSERT INTO {into} SELECT {} FROM {from}", qualified_columns(&from, &src_cols))
            } else {
                format!(
                    "INSERT INTO {into} ({}) SELECT {} FROM {from}",
                    column_list(&spec.target.columns),
                    qualified_columns(&from, &src_cols)
                )
            };
            // A rowid table goes by ranges of the rowid under a name no
            // column takes over.
            let (kind, without_rowid) = c
                .query_row(
                    "SELECT type, wr FROM pragma_table_list WHERE schema = ?1 AND name = ?2",
                    [from_schema, spec.source.table.name.as_str()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
                )
                .map_err(err)?;
            let rowid = ["_rowid_", "rowid", "oid"].into_iter().find(|n| !src_all.iter().any(|col| col.eq_ignore_ascii_case(n)));
            let rowid = match (kind.as_str(), without_rowid, rowid) {
                ("table", false, Some(r)) => r,
                _ => {
                    let what = match (kind.as_str(), without_rowid) {
                        ("view", _) => "el origen es una vista",
                        (_, true) => "la tabla de origen es WITHOUT ROWID",
                        _ => "la tabla de origen tiene columnas que ocultan su rowid",
                    };
                    let insert = match filter {
                        Some(f) => format!("{insert} WHERE ({f})"),
                        None => insert,
                    };
                    return whole(c, dst_schema, &insert, &gate, &abort, &interrupt, what, &ctx);
                }
            };
            let bounds = format!("SELECT min({rowid}), max({rowid}) FROM {from}");
            let (Some(mut next), Some(last)) = c.query_row(&bounds, [], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))).map_err(err)? else {
                return Ok(0);
            };
            let cond = filter.map(|f| format!("({f}) AND ")).unwrap_or_default();
            let mut stmt = c.prepare(&format!("{insert} WHERE {cond}{from}.{rowid} BETWEEN ?1 AND ?2")).map_err(err)?;
            let (mut span, mut total) = (1_024i64, 0u64);
            loop {
                let mut turn = Turn::take(&gate, &abort, false)?;
                if begin(c, dst_schema, &abort)? {
                    turn.restart();
                }
                let done = loop {
                    if abort.is_set() {
                        return Err(Error::Cancelled);
                    }
                    let end = next.saturating_add(span - 1).min(last);
                    let t = Instant::now();
                    total += stmt.execute([next, end]).map_err(err)? as u64;
                    let took = t.elapsed();
                    span = if took < STEP / 4 {
                        span.saturating_mul(4)
                    } else if took < STEP / 2 {
                        span.saturating_mul(2)
                    } else if took > STEP * 2 {
                        (span / 2).max(1)
                    } else {
                        span
                    };
                    if end >= last {
                        break true;
                    }
                    next = end + 1;
                    if turn.expired() {
                        break false;
                    }
                };
                abort.commit(c)?;
                drop(turn);
                let _ = ctx.send(total);
                if done {
                    return Ok(total);
                }
            }
        };
        let r = run();
        if r.is_err() && !c.is_autocommit() {
            let _ = c.execute_batch("ROLLBACK");
        }
        if attached.is_some() {
            let _ = c.execute_batch("DETACH DATABASE dbine_copy_src");
        }
        r
    });
    tokio::pin!(work);
    let rows = loop {
        tokio::select! {
            r = &mut work => break r,
            Some(n) = crx.recv() => progress(n),
        }
    };
    // Done: nothing left to interrupt.
    abort_on_drop.interrupt = None;
    while let Ok(n) = crx.try_recv() {
        progress(n);
    }
    rows
}

/// `insert` (a source without rowid ranges) in one statement and one turn
/// of its own: interrupted and `Unsupported` when it would keep the file
/// locked past the turn's deadline.
#[allow(clippy::too_many_arguments)]
fn whole(
    c: &Connection,
    schema: &str,
    insert: &str,
    gate: &Option<Arc<Gate>>,
    abort: &Abort,
    interrupt: &Arc<InterruptHandle>,
    what: &str,
    committed: &mpsc::UnboundedSender<u64>,
) -> Result<u64> {
    let mut turn = Turn::take(gate, abort, true)?;
    if begin(c, schema, abort)? {
        turn.restart();
    }
    let overran = Arc::new(AtomicBool::new(false));
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let watchdog = turn.deadline.map(|d| {
        let (overran, interrupt) = (overran.clone(), interrupt.clone());
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(d.saturating_duration_since(Instant::now())) {
                overran.store(true, Ordering::SeqCst);
                interrupt.interrupt();
            }
        })
    });
    let r = c.execute(insert, []);
    drop(stop);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let n = match r {
        Ok(n) => n as u64,
        Err(_) if overran.load(Ordering::SeqCst) && !abort.is_set() => {
            return Err(Error::Unsupported(format!(
                "copia directa no disponible: {what} y copiarla en una sola transacción bloquearía el archivo de destino más de {} s",
                HOLD.as_secs()
            )))
        }
        Err(e) => return Err(err(e)),
    };
    abort.commit(c)?;
    drop(turn);
    let _ = committed.send(n);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_read_only_and_escaped() {
        assert_eq!(read_only_uri("/tmp/a b%?#.db"), "file:/tmp/a b%25%3f%23.db?mode=ro");
        assert_eq!(read_only_uri("C:\\data\\x.db"), "file:/C:/data/x.db?mode=ro");
        assert_eq!(read_only_uri("\\\\server\\share\\x.db"), "file:////server/share/x.db?mode=ro");
        assert_eq!(read_only_uri("\\\\?\\UNC\\server\\share\\x.db"), "file:////server/share/x.db?mode=ro");
        assert_eq!(read_only_uri("\\\\?\\C:\\x.db"), "file:/C:/x.db?mode=ro");
    }

    #[test]
    fn statements() {
        let t = ObjectRef { kind: "table".into(), schema: Some("main".into()), name: "t\"x".into() };
        let spec = ReadSpec { table: t.clone(), columns: Some(vec!["a".into()]), filter: Some(" a > 1 ".into()) };
        assert_eq!(select_sql(&spec), "SELECT \"main\".\"t\"\"x\".\"a\" AS \"a\" FROM \"main\".\"t\"\"x\" WHERE a > 1");
        let load = LoadSpec { table: t, columns: vec!["a".into(), "b".into()], table_lock: false, keep_identity: false, commit_rows: 1, commit_bytes: 1 };
        assert_eq!(insert_sql(&load, 2), "INSERT INTO \"main\".\"t\"\"x\" (\"a\", \"b\") VALUES (?, ?), (?, ?)");
    }
}

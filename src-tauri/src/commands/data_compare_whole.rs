//! Data compare over whole tables (docs/data-compare.md, issue #88).
//!
//! Each side is read whole with its driver's batched read
//! ([`Session::read_batches`], the one table copies use: cursors, scrolls
//! and paging where the engine has them, never one huge `LIMIT`), in no
//! particular order, both sides at once, each on its own session. The
//! compared columns are known before reading (from each side's catalog), so
//! every row is projected to them (the key first) as it arrives.
//!
//! - While both sides are small (each under [`Options::in_memory_rows`] and
//!   [`Options::in_memory_bytes`]) the projected rows stay in memory and are
//!   compared there, as before.
//! - Once a side passes either, the rows of both sides go to a scratch
//!   DuckDB on disk (`local_duckdb`: encrypted file and temp files,
//!   extensions off, configuration locked), with a key made for this compare
//!   and kept in memory only. It starts opening at half the limits, so the
//!   reads rarely wait for it. Each row goes as its canonical key (`k`), an
//!   order key for the lists (`o`), a 128-bit hash of its canonical values
//!   (`h1`, `h2`) and the row itself as JSON text (`v`, an exact round
//!   trip). The counts come from one join over the whole tables; only the
//!   rows that differ come back, up to [`Options::max_diff_rows`] in key
//!   order, and the scratch files are deleted when the compare ends, however
//!   it ends.
//!
//! A read waits (its sink blocks) while the scratch DuckDB opens and while
//! the loader is behind. That wait never holds a driver host's reader
//! thread: a host's batches reach the sink on the task that reads
//! (`RemoteSession::read_batches`), with the host's window as the
//! backpressure, so the scratch DuckDB's replies, on the same host or not,
//! keep coming.
//!
//! Both paths give the same result: the same equality ([`canon`]), the first
//! row read of a repeated key compared, and the lists in the same key order.

use super::{as_bool, canon, is_bool_type, sort_value, ChangedRow, Counts, SortValue};
use crate::error::{CommandError, CommandResult};
use crate::local_duckdb;
use dbine_driver::transfer::{BatchSource, Cell, LoadSpec, RowBatch, TransferColumn};
use dbine_driver::{
    BatchSink, ColumnInfo, Driver, ObjectRef, QueryOutcome, ReadSpec, ResultColumn, RowSink,
    RowSinkRef, Session,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Rows per batch handed to the scratch DuckDB…
const BATCH_ROWS: usize = 1_000;
/// …or bytes, whichever comes first.
const BATCH_BYTES: usize = 8 << 20;
/// Bytes handed to the loader and not yet taken: past this, the reads wait
/// for it (a fast source doesn't fill the memory faster than DuckDB writes).
const QUEUE_BYTES: u64 = 64 << 20;
/// The scratch database's file and the alias it's attached as.
const FILE: &str = "compare.duckdb";
/// What the scratch DuckDB may hold in memory; past it, it works on its
/// encrypted temp files.
pub(crate) const MEMORY_LIMIT: &str = "512MB";
const ALIAS: &str = "cmp";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Control: cancel and stop

/// A compare's stop switch: the user's Cancelar ([`Ctl::cancel`]) or a
/// failure that makes going on pointless ([`Ctl::fail`]). Stopping runs the
/// stoppers (the reads' `cancel_session`, the scratch DuckDB's interrupter)
/// and wakes every wait.
#[derive(Default)]
pub(crate) struct Ctl {
    stopped: AtomicBool,
    notify: Notify,
    stoppers: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    failure: Mutex<Option<CommandError>>,
}

impl Ctl {
    /// The user's Cancelar: the compare returns `Cancelled`.
    pub(crate) fn cancel(&self) {
        self.stop(None);
    }

    /// Stop because of `why`, which the compare then returns (unless it was
    /// already stopped).
    pub(crate) fn fail(&self, why: CommandError) {
        self.stop(Some(why));
    }

    fn stop(&self, why: Option<CommandError>) {
        if let Some(e) = why {
            let mut f = lock(&self.failure);
            if f.is_none() && !self.is_stopped() {
                *f = Some(e);
            }
        }
        self.stopped.store(true, Ordering::SeqCst);
        let stoppers = lock(&self.stoppers).clone();
        for s in stoppers {
            s();
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Something to run on a stop (right away if it already stopped).
    pub(crate) fn add_stopper(&self, f: Arc<dyn Fn() + Send + Sync>) {
        lock(&self.stoppers).push(f.clone());
        if self.is_stopped() {
            f();
        }
    }

    /// Resolves once stopped.
    pub(crate) async fn stopped(&self) {
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            n.await;
        }
    }

    /// What a stopped compare returns.
    pub(crate) fn error(&self) -> CommandError {
        lock(&self.failure)
            .clone()
            .unwrap_or(CommandError::Cancelled)
    }
}

// ---------------------------------------------------------------------------
// Inputs and outputs

pub(crate) struct Options {
    /// Rows of a side kept in memory…
    pub in_memory_rows: usize,
    /// …and their estimated bytes: past either, both sides go to DuckDB.
    pub in_memory_bytes: usize,
    /// Differing rows brought back (all kinds together); the counts are
    /// exact past it.
    pub max_diff_rows: usize,
    /// Where the scratch DuckDBs go (each in a folder of its own). `None`:
    /// a compare that needs one fails.
    pub scratch: Option<PathBuf>,
    /// The DuckDB driver the scratch database opens on. `None`: the app's
    /// own ([`local_duckdb::open_session`]); a test passes a driver served
    /// by a host process.
    pub duckdb: Option<Arc<dyn Driver>>,
}

/// What to compare: the key, the columns asked for (empty: all both sides
/// have) and each side's columns from its catalog (names, types, booleans).
pub(crate) struct Input {
    pub key: Vec<String>,
    pub wanted: Vec<String>,
    pub cols: [Vec<ColumnInfo>; 2],
}

/// One side of a compare: its session and table.
pub(crate) struct Side<'a> {
    pub session: &'a mut dyn Session,
    pub object: &'a ObjectRef,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Reading,
    /// A read waits while the scratch DuckDB opens.
    Preparing,
    Comparing,
    LoadingDifferences,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Phase::Reading => "reading",
            Phase::Preparing => "preparing",
            Phase::Comparing => "comparing",
            Phase::LoadingDifferences => "loading_differences",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Progress {
    /// Rows read on each side.
    pub rows: [u64; 2],
    pub phase: Phase,
    /// Differing rows brought back so far (while loading them).
    pub differences: u64,
}

/// Which columns are compared, by their name on each side.
#[derive(Debug)]
pub(crate) struct Projection {
    /// (left name, right name) of each compared column, the key first, in
    /// the left's order.
    pub columns: Vec<(String, String)>,
    pub nk: usize,
    /// Per side: the compared columns that hold booleans.
    bools: [Vec<bool>; 2],
    pub only_left_columns: Vec<String>,
    pub only_right_columns: Vec<String>,
}

impl Projection {
    /// The columns both sides have (by name, ignoring case), the key first.
    fn new(input: &Input, lnames: &[String], rnames: &[String]) -> CommandResult<Projection> {
        let find = |names: &[String], n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n));
        let wanted: Vec<&String> = if input.wanted.is_empty() {
            lnames.iter().collect()
        } else {
            input.wanted.iter().collect()
        };
        let mut columns: Vec<(String, String)> = Vec::new();
        for k in &input.key {
            let (Some(li), Some(ri)) = (find(lnames, k), find(rnames, k)) else {
                return Err(CommandError::BadRequest(format!(
                    "la columna clave «{k}» no está en las dos tablas"
                )));
            };
            columns.push((lnames[li].clone(), rnames[ri].clone()));
        }
        for n in wanted {
            if input.key.iter().any(|k| k.eq_ignore_ascii_case(n)) {
                continue;
            }
            if let (Some(li), Some(ri)) = (find(lnames, n), find(rnames, n)) {
                if !columns.iter().any(|c| c.0 == lnames[li]) {
                    columns.push((lnames[li].clone(), rnames[ri].clone()));
                }
            }
        }
        let booleans = |cols: &[ColumnInfo], name: &str| {
            cols.iter()
                .any(|c| c.name.eq_ignore_ascii_case(name) && is_bool_type(&c.data_type))
        };
        let bools = [
            columns
                .iter()
                .map(|c| booleans(&input.cols[0], &c.0))
                .collect(),
            columns
                .iter()
                .map(|c| booleans(&input.cols[1], &c.1))
                .collect(),
        ];
        Ok(Projection {
            nk: input.key.len(),
            only_left_columns: lnames
                .iter()
                .filter(|n| find(rnames, n).is_none())
                .cloned()
                .collect(),
            only_right_columns: rnames
                .iter()
                .filter(|n| find(lnames, n).is_none())
                .cloned()
                .collect(),
            columns,
            bools,
        })
    }

    /// The compared columns' names on `side`.
    fn names(&self, side: usize) -> Vec<String> {
        self.columns
            .iter()
            .map(|c| if side == 0 { c.0.clone() } else { c.1.clone() })
            .collect()
    }
}

/// A finished compare.
pub(crate) struct Compared {
    pub projection: Arc<Projection>,
    pub counts: Counts,
    pub only_left: Vec<Vec<Value>>,
    pub only_right: Vec<Vec<Value>>,
    pub changed: Vec<ChangedRow>,
    /// Rows whose key was repeated on a side (compared once).
    pub duplicate_keys: u64,
    /// More rows differ than the lists hold (the counts are exact).
    pub diff_truncated: bool,
    /// A one-column key's greatest value on each side (NULL keys aside).
    pub last_keys: Option<(Option<Value>, Option<Value>)>,
    /// The rows went through the scratch DuckDB.
    pub spilled: bool,
}

// ---------------------------------------------------------------------------
// Rows as compared

/// A read's cell as the value compared, kept in the lists and written by
/// the sync script: [`Cell::to_json`] (the grid's shapes: decimals, dates
/// and UUIDs as text, binaries as `0x…`), without copying the text.
pub(crate) fn value_of(c: Cell) -> Value {
    match c {
        Cell::Decimal(s)
        | Cell::Text(s)
        | Cell::Date(s)
        | Cell::Time(s)
        | Cell::DateTime(s)
        | Cell::DateTimeTz(s)
        | Cell::Uuid(s) => Value::String(s),
        Cell::Json(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other.to_json(),
    }
}

/// A projected row's key as compared: the canonical values of its key
/// columns.
pub(crate) fn key_of(p: &[Value], nk: usize) -> String {
    p[..nk].iter().map(canon).collect::<Vec<_>>().join("\u{1}")
}

/// Text that sorts (byte by byte) as the key's values do in the UI's sort
/// ([`sort_value`]): numbers by value, then text without case, then NULL,
/// column by column. Both paths order their lists by it (then by
/// [`key_of`]), so they cut a long list at the same row.
pub(crate) fn order_key(key: &[Value]) -> String {
    let mut o = String::new();
    for v in key {
        match sort_value(v) {
            SortValue::Number(f) => {
                // The order of `f64::total_cmp`, as an unsigned number.
                let b = f.to_bits();
                let b = if b >> 63 == 1 { !b } else { b | (1 << 63) };
                let _ = write!(o, "0{b:016x}");
            }
            SortValue::Text(t) => {
                o.push('1');
                o.extend(t.chars().map(|c| if c < '\u{2}' { '\u{2}' } else { c }));
            }
            SortValue::Null => o.push('2'),
        }
        o.push('\u{1}');
    }
    o
}

/// 128 bits of SHA-256 over the canonical non-key values: two rows of the
/// same key are equal exactly when their hashes are.
fn hash_of(p: &[Value], nk: usize) -> (i64, i64) {
    let mut h = Sha256::new();
    for v in &p[nk..] {
        let c = canon(v);
        h.update((c.len() as u64).to_le_bytes());
        h.update(c.as_bytes());
    }
    let d = h.finalize();
    let half = |r: std::ops::Range<usize>| {
        i64::from_le_bytes(d[r].try_into().expect("8 bytes of a 32-byte digest"))
    };
    (half(0..8), half(8..16))
}

/// A projected row as the scratch table's cells.
fn cells(side: usize, n: u64, p: &[Value], nk: usize) -> Vec<Cell> {
    let (h1, h2) = hash_of(p, nk);
    vec![
        Cell::Int(side as i64),
        Cell::Int(n as i64),
        Cell::Text(key_of(p, nk)),
        Cell::Text(order_key(&p[..nk])),
        Cell::Int(h1),
        Cell::Int(h2),
        Cell::Text(serde_json::to_string(p).unwrap_or_else(|_| "null".into())),
    ]
}

const COLUMNS: [(&str, &str); 7] = [
    ("s", "BIGINT"),
    ("n", "BIGINT"),
    ("k", "VARCHAR"),
    ("o", "VARCHAR"),
    ("h1", "BIGINT"),
    ("h2", "BIGINT"),
    ("v", "VARCHAR"),
];

// ---------------------------------------------------------------------------
// The two reads and what they share

/// One side's rows while it's read.
#[derive(Default)]
struct Buf {
    /// Projected rows in the order read, while in memory…
    rows: Vec<Vec<Value>>,
    /// …and their estimated size ([`Cell::size`] of what was read).
    bytes: usize,
    /// Rows read so far: the next row's number.
    seq: u64,
    /// Once spilling: where the rows go, and the batch being filled.
    tx: Option<mpsc::UnboundedSender<RowBatch>>,
    batch: Vec<Vec<Cell>>,
    batch_bytes: usize,
    /// The greatest non-NULL key so far (one-column keys).
    last: Option<(SortValue, Value)>,
}

struct Job {
    ctl: Arc<Ctl>,
    projection: Arc<Projection>,
    in_memory_rows: usize,
    in_memory_bytes: usize,
    bufs: [Mutex<Buf>; 2],
    read: [AtomicU64; 2],
    phase: AtomicU8,
    /// Reads waiting for the scratch DuckDB to open.
    preparing: AtomicUsize,
    differences: AtomicU64,
    /// A side passed half the limits: the scratch DuckDB starts opening.
    spill_soon: AtomicBool,
    /// A side passed the limits: both go to DuckDB.
    spill_wanted: AtomicBool,
    /// The scratch DuckDB takes the rows now.
    installed: AtomicBool,
    reads_done: AtomicBool,
    /// The loader stopped taking batches.
    loader_gone: AtomicBool,
    queued: AtomicU64,
    /// Wakes the spiller: spill soon or wanted, reads done.
    wake: Notify,
    /// Orders `install` against `finish_reads`.
    order: Mutex<()>,
}

impl Job {
    fn new(ctl: Arc<Ctl>, projection: Arc<Projection>, opts: &Options) -> Job {
        Job {
            ctl,
            projection,
            in_memory_rows: opts.in_memory_rows.max(1),
            in_memory_bytes: opts.in_memory_bytes.max(1),
            bufs: Default::default(),
            read: Default::default(),
            phase: AtomicU8::new(0),
            preparing: AtomicUsize::new(0),
            differences: AtomicU64::new(0),
            spill_soon: AtomicBool::new(false),
            spill_wanted: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            reads_done: AtomicBool::new(false),
            loader_gone: AtomicBool::new(false),
            queued: AtomicU64::new(0),
            wake: Notify::new(),
            order: Mutex::new(()),
        }
    }

    fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::SeqCst);
    }

    fn progress(&self) -> Progress {
        let phase = match self.phase.load(Ordering::SeqCst) {
            0 if self.preparing.load(Ordering::SeqCst) > 0 => Phase::Preparing,
            0 | 1 => Phase::Reading,
            2 => Phase::Comparing,
            _ => Phase::LoadingDifferences,
        };
        Progress {
            rows: [
                self.read[0].load(Ordering::Relaxed),
                self.read[1].load(Ordering::Relaxed),
            ],
            phase,
            differences: self.differences.load(Ordering::Relaxed),
        }
    }

    /// Past the limits (`share` 1) or half of them (`share` 2).
    fn over(&self, rows: usize, bytes: usize, share: usize) -> bool {
        rows > self.in_memory_rows / share || bytes > self.in_memory_bytes / share
    }

    /// A batch of `side`'s projected rows (with their estimated sizes), from
    /// its sink.
    fn take(&self, side: usize, rows: Vec<(Vec<Value>, usize)>) -> std::io::Result<()> {
        let nk = self.projection.nk;
        let n = rows.len() as u64;
        let mut b = lock(&self.bufs[side]);
        for (row, size) in rows {
            let seq = b.seq;
            b.seq += 1;
            track_last(&mut b, &row, nk);
            if b.tx.is_none() {
                b.rows.push(row);
                b.bytes += size;
                continue;
            }
            let c = cells(side, seq, &row, nk);
            b.batch_bytes += c.iter().map(Cell::size).sum::<usize>();
            b.batch.push(c);
            if b.batch.len() >= BATCH_ROWS || b.batch_bytes >= BATCH_BYTES {
                self.send_batch(&mut b)?;
            }
        }
        self.read[side].fetch_add(n, Ordering::Relaxed);
        let (held, bytes, spilling) = (b.rows.len(), b.bytes, b.tx.is_some());
        drop(b);
        if spilling {
            self.wait_while(|| {
                self.queued.load(Ordering::SeqCst) > QUEUE_BYTES
                    && !self.loader_gone.load(Ordering::SeqCst)
            });
        } else {
            self.held_in_memory(side, held, bytes);
        }
        Ok(())
    }

    /// Hand the batch being filled to the loader.
    fn send_batch(&self, b: &mut Buf) -> std::io::Result<()> {
        if b.batch.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut b.batch);
        let bytes = std::mem::take(&mut b.batch_bytes);
        let Some(tx) = &b.tx else {
            return Ok(());
        };
        self.queued.fetch_add(bytes as u64, Ordering::SeqCst);
        tx.send(RowBatch { rows, bytes })
            .map_err(|_| std::io::Error::other("la base local dejó de recibir filas"))
    }

    /// `held` rows (`bytes`) of `side` are in memory: from half the limits
    /// the scratch DuckDB opens; past them the rows go to it, and the read
    /// waits until they can (memory holds no more than the limits).
    fn held_in_memory(&self, side: usize, held: usize, bytes: usize) {
        if self.over(held, bytes, 2) && !self.spill_soon.swap(true, Ordering::SeqCst) {
            self.wake.notify_waiters();
        }
        if !self.over(held, bytes, 1) {
            return;
        }
        if !self.spill_wanted.swap(true, Ordering::SeqCst) {
            self.wake.notify_waiters();
        }
        self.preparing.fetch_add(1, Ordering::SeqCst);
        self.wait_while(|| {
            let b = lock(&self.bufs[side]);
            !self.installed.load(Ordering::SeqCst) && self.over(b.rows.len(), b.bytes, 1)
        });
        self.preparing.fetch_sub(1, Ordering::SeqCst);
    }

    /// Hold the reading thread while `busy` (and nothing stopped). Where the
    /// runtime can't block (a current-thread runtime) it doesn't wait.
    fn wait_while(&self, busy: impl Fn() -> bool) {
        if !busy() || self.ctl.is_stopped() {
            return;
        }
        let handle = tokio::runtime::Handle::try_current();
        if matches!(&handle, Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread)
        {
            return;
        }
        let wait = || {
            while busy() && !self.ctl.is_stopped() {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        if handle.is_ok() {
            tokio::task::block_in_place(wait);
        } else {
            wait();
        }
    }

    /// The scratch DuckDB is ready: what's in memory goes to it, and the
    /// rows read from now on go straight to `tx`.
    async fn install(&self, tx: mpsc::UnboundedSender<RowBatch>) -> CommandResult<()> {
        let nk = self.projection.nk;
        let mut taken: [Vec<Vec<Value>>; 2] = Default::default();
        {
            let _order = lock(&self.order);
            let done = self.reads_done.load(Ordering::SeqCst);
            for (side, t) in taken.iter_mut().enumerate() {
                let mut b = lock(&self.bufs[side]);
                *t = std::mem::take(&mut b.rows);
                b.bytes = 0;
                if !done {
                    b.tx = Some(tx.clone());
                }
            }
            self.installed.store(true, Ordering::SeqCst);
        }
        // Outside the buffers' locks: the reads go on meanwhile. Rows in
        // memory are the first ones read, numbered from 0. Batch by batch,
        // as the loader (polled alongside) takes them.
        let lost = || CommandError::Internal("la base local dejó de recibir filas".into());
        for (side, rows) in taken.into_iter().enumerate() {
            let mut batch = Buf {
                tx: Some(tx.clone()),
                ..Default::default()
            };
            for (n, row) in rows.into_iter().enumerate() {
                let c = cells(side, n as u64, &row, nk);
                batch.batch_bytes += c.iter().map(Cell::size).sum::<usize>();
                batch.batch.push(c);
                if batch.batch.len() >= BATCH_ROWS || batch.batch_bytes >= BATCH_BYTES {
                    self.send_batch(&mut batch).map_err(|_| lost())?;
                    self.room().await?;
                }
            }
            self.send_batch(&mut batch).map_err(|_| lost())?;
        }
        Ok(())
    }

    /// Yield to the loader, and wait while it has more than enough to take.
    async fn room(&self) -> CommandResult<()> {
        tokio::task::yield_now().await;
        while self.queued.load(Ordering::SeqCst) > QUEUE_BYTES
            && !self.loader_gone.load(Ordering::SeqCst)
        {
            if self.ctl.is_stopped() {
                return Err(self.ctl.error());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if self.ctl.is_stopped() {
            return Err(self.ctl.error());
        }
        Ok(())
    }

    /// Both reads ended (or one failed): the batches being filled go, and
    /// the loader's channel closes once nothing else holds it.
    fn finish_reads(&self) {
        let _order = lock(&self.order);
        self.reads_done.store(true, Ordering::SeqCst);
        for side in 0..2 {
            let mut b = lock(&self.bufs[side]);
            if b.tx.is_some() {
                if let Err(e) = self.send_batch(&mut b) {
                    tracing::debug!(%e, "data compare: last batch");
                }
                b.tx = None;
            }
        }
        self.wake.notify_waiters();
    }

    /// Wait until `ready` (or a stop).
    async fn wait_for(&self, ready: impl Fn() -> bool) {
        loop {
            let n = self.wake.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if ready() || self.ctl.is_stopped() {
                return;
            }
            tokio::select! {
                () = n => {}
                () = self.ctl.stopped() => return,
            }
        }
    }
}

/// Keep the greatest non-NULL key of a one-column key.
fn track_last(b: &mut Buf, row: &[Value], nk: usize) {
    if nk != 1 || row[0].is_null() {
        return;
    }
    let v = sort_value(&row[0]);
    let greater = b
        .last
        .as_ref()
        .is_none_or(|(l, _)| v.cmp_with(l, false) == std::cmp::Ordering::Greater);
    if greater {
        b.last = Some((v, row[0].clone()));
    }
}

/// A side's batches as its driver reads them: each row projected to the
/// compared columns (found by name, ignoring case, in the read's columns).
struct SideSink {
    side: usize,
    job: Arc<Job>,
    /// The read's position of each compared column, and whether an earlier
    /// compared column already took it (a key also asked as a column).
    pick: Option<Vec<(usize, Option<usize>)>>,
}

impl SideSink {
    fn refuse(&self, e: CommandError) -> std::io::Error {
        let io = std::io::Error::other(e.to_string());
        self.job.ctl.fail(e);
        io
    }
}

impl BatchSink for SideSink {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        if self.pick.is_some() {
            return Ok(());
        }
        let p = &self.job.projection;
        let mut pick: Vec<(usize, Option<usize>)> = Vec::with_capacity(p.columns.len());
        for (i, name) in p.names(self.side).iter().enumerate() {
            let Some(at) = columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
            else {
                return Err(self.refuse(CommandError::BadRequest(if i < p.nk {
                    format!("la columna clave «{name}» no está en las dos tablas")
                } else {
                    format!("la lectura de la tabla no trae la columna «{name}»")
                })));
            };
            let earlier = pick.iter().position(|(x, _)| *x == at);
            pick.push((at, earlier));
        }
        self.pick = Some(pick);
        Ok(())
    }

    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        if self.job.ctl.is_stopped() {
            return Err(std::io::Error::other("la comparación se detuvo"));
        }
        let Some(pick) = &self.pick else {
            return Err(self.refuse(CommandError::Internal(
                "la lectura dio filas sin decir sus columnas".into(),
            )));
        };
        let bools = &self.job.projection.bools[self.side];
        let rows = batch
            .rows
            .into_iter()
            .map(|mut r| {
                let mut size = 0;
                let mut out: Vec<Value> = Vec::with_capacity(pick.len());
                for (i, &(at, earlier)) in pick.iter().enumerate() {
                    let v = match earlier {
                        Some(e) => out[e].clone(),
                        None => {
                            let c = r
                                .get_mut(at)
                                .map_or(Cell::Null, |c| std::mem::replace(c, Cell::Null));
                            size += c.size();
                            value_of(c)
                        }
                    };
                    out.push(if bools[i] { as_bool(v) } else { v });
                }
                (out, size)
            })
            .collect();
        self.job.take(self.side, rows)
    }
}

/// A side's columns: its catalog's, or (a catalog that lists none) those of
/// a one-row read.
async fn names_of(
    ctl: &Ctl,
    s: &mut dyn Session,
    obj: &ObjectRef,
    cols: &[ColumnInfo],
) -> CommandResult<Vec<String>> {
    if !cols.is_empty() {
        return Ok(cols.iter().map(|c| c.name.clone()).collect());
    }
    let sql = s.browse_query(obj, 1);
    let mut out = QueryOutcome::default();
    tokio::select! {
        r = s.execute(&sql, 1, &mut out) => r?,
        () = ctl.stopped() => return Err(ctl.error()),
    };
    if let Some(e) = out.error.take() {
        return Err(CommandError::Sql(e));
    }
    Ok(out
        .results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.columns.into_iter().map(|c| c.name).collect())
        .unwrap_or_default())
}

/// Read a side whole into its sink.
async fn read_side(
    job: &Arc<Job>,
    side: usize,
    s: &mut dyn Session,
    obj: &ObjectRef,
) -> CommandResult<()> {
    let sink = Arc::new(Mutex::new(SideSink {
        side,
        job: job.clone(),
        pick: None,
    }));
    let spec = ReadSpec {
        table: obj.clone(),
        columns: Some(job.projection.names(side)),
        filter: None,
    };
    let r = tokio::select! {
        r = s.read_batches(&spec, sink) => r,
        () = job.ctl.stopped() => return Err(job.ctl.error()),
    };
    if job.ctl.is_stopped() {
        return Err(job.ctl.error());
    }
    r?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The scratch DuckDB

/// A compare's own encrypted DuckDB, in a folder of its own that goes when
/// it closes (or, if it's dropped without closing, right then).
struct Scratch {
    session: Option<Box<dyn Session>>,
    dir: PathBuf,
}

/// What the user reads when a big table can't go to DuckDB.
fn too_big(why: &str) -> CommandError {
    CommandError::State(format!(
        "la tabla es demasiado grande para compararla en memoria y no se pudo abrir DuckDB, la base local donde DBine compara las tablas grandes: {why}"
    ))
}

impl Scratch {
    /// Open it in a new folder of `root`, on `driver` (the app's DuckDB when
    /// `None`), DuckDB taking at most `memory_limit` of memory.
    async fn open(
        root: Option<&Path>,
        ctl: &Ctl,
        driver: Option<&Arc<dyn Driver>>,
        memory_limit: &str,
    ) -> CommandResult<Scratch> {
        let root = root.ok_or_else(|| too_big("no hay una carpeta de datos de DBine"))?;
        let dir = root.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(dir.join("tmp")).map_err(|e| too_big(&e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [root, dir.as_path()] {
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
        }
        let mut scratch = Scratch {
            session: None,
            dir: dir.clone(),
        };
        // A key for this compare only, in memory only.
        let opened = async move {
            let key = local_duckdb::new_key().map_err(|e| e.to_string())?;
            let mut s = match driver {
                Some(d) => d.connect(&local_duckdb::config(), None).await,
                None => local_duckdb::open_session().await,
            }
            .map_err(|e| e.to_string())?;
            let set = async {
                local_duckdb::configure_with(s.as_mut(), &dir, &key, memory_limit).await?;
                local_duckdb::attach(s.as_mut(), &dir, FILE, ALIAS, &key).await?;
                local_duckdb::lock(s.as_mut(), ALIAS, &key).await
            }
            .await;
            set.map_err(|e| local_duckdb::redact(e.reason(), &key))?;
            Ok::<_, String>(s)
        };
        let opened = tokio::select! {
            r = opened => r,
            () = ctl.stopped() => {
                scratch.close().await;
                return Err(ctl.error());
            }
        };
        match opened {
            Ok(s) => {
                if let Some(i) = s.interrupter() {
                    ctl.add_stopper(i);
                }
                scratch.session = Some(s);
                Ok(scratch)
            }
            Err(why) => {
                tracing::warn!(%why, "data compare: the scratch DuckDB didn't open");
                scratch.close().await;
                Err(too_big(&why))
            }
        }
    }

    fn session(&mut self) -> &mut dyn Session {
        self.session
            .as_deref_mut()
            .expect("an open scratch database")
    }

    /// Run `sql` unless the compare stops.
    async fn exec(&mut self, ctl: &Ctl, sql: &str) -> CommandResult<QueryOutcome> {
        let s = self.session();
        tokio::select! {
            r = local_duckdb::exec(s, sql) => r,
            () = ctl.stopped() => Err(ctl.error()),
        }
    }

    async fn create(&mut self, ctl: &Ctl) -> CommandResult<()> {
        let defs: Vec<String> = COLUMNS.iter().map(|(n, t)| format!("{n} {t}")).collect();
        self.exec(
            ctl,
            &format!("CREATE TABLE main.rows ({})", defs.join(", ")),
        )
        .await?;
        Ok(())
    }

    /// Load every batch until the channel closes.
    async fn load(&mut self, ctl: &Ctl, source: &mut Feed) -> CommandResult<u64> {
        let spec = LoadSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: Some("main".into()),
                name: "rows".into(),
            },
            columns: COLUMNS.iter().map(|(n, _)| n.to_string()).collect(),
            table_lock: false,
            keep_identity: false,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let tcols: Vec<TransferColumn> = COLUMNS
            .iter()
            .map(|(n, t)| TransferColumn {
                name: n.to_string(),
                type_name: t.to_string(),
                nullable: false,
            })
            .collect();
        let s = self.session();
        tokio::select! {
            r = s.bulk_load(&spec, &tcols, source, &|_| {}) => Ok(r?),
            () = ctl.stopped() => Err(ctl.error()),
        }
    }

    /// Each side's rows and distinct keys, then the counts over the whole
    /// tables (the first row of a repeated key, by the order read).
    async fn counts(&mut self, ctl: &Ctl) -> CommandResult<(Counts, u64)> {
        let out = self
            .exec(
                ctl,
                "SELECT s, count(*), count(DISTINCT k) FROM main.rows GROUP BY s",
            )
            .await?;
        let mut per = [(0u64, 0u64); 2];
        for r in rows_of(&out) {
            if let (Some(s), Some(n), Some(d)) = (
                r.first().and_then(as_u64),
                r.get(1).and_then(as_u64),
                r.get(2).and_then(as_u64),
            ) {
                if let Some(slot) = per.get_mut(s as usize) {
                    *slot = (n, d);
                }
            }
        }
        for (side, view) in ["l", "r"].into_iter().enumerate() {
            let first = if per[side].0 > per[side].1 {
                " QUALIFY row_number() OVER (PARTITION BY k ORDER BY n) = 1"
            } else {
                ""
            };
            self.exec(
                ctl,
                &format!(
                    "CREATE TEMP VIEW {view} AS SELECT k, o, h1, h2, v FROM {ALIAS}.main.rows WHERE s = {side}{first}"
                ),
            )
            .await?;
        }
        let out = self
            .exec(
                ctl,
                "SELECT \
                 count(*) FILTER (WHERE l.k IS NOT NULL AND r.k IS NOT NULL AND l.h1 = r.h1 AND l.h2 = r.h2), \
                 count(*) FILTER (WHERE l.k IS NOT NULL AND r.k IS NOT NULL AND (l.h1 <> r.h1 OR l.h2 <> r.h2)), \
                 count(*) FILTER (WHERE r.k IS NULL), \
                 count(*) FILTER (WHERE l.k IS NULL) \
                 FROM l FULL OUTER JOIN r ON l.k = r.k",
            )
            .await?;
        let row = local_duckdb::first_row(&out);
        let n = |i: usize| row.get(i).and_then(as_u64).unwrap_or(0);
        let counts = Counts {
            left: per[0].0,
            right: per[1].0,
            same: n(0),
            changed: n(1),
            only_left: n(2),
            only_right: n(3),
        };
        let dups = (per[0].0 - per[0].1) + (per[1].0 - per[1].1);
        Ok((counts, dups))
    }

    /// The first `max` differing rows in key order, back as projected rows.
    async fn differences(
        &mut self,
        ctl: &Ctl,
        job: &Arc<Job>,
        nk: usize,
        max: usize,
    ) -> CommandResult<Lists> {
        self.exec(
            ctl,
            &format!(
                "CREATE TEMP TABLE d AS \
                 SELECT CASE WHEN r.k IS NULL THEN 0 WHEN l.k IS NULL THEN 1 ELSE 2 END AS kind, \
                 coalesce(l.o, r.o) AS o, coalesce(l.k, r.k) AS k \
                 FROM l FULL OUTER JOIN r ON l.k = r.k \
                 WHERE r.k IS NULL OR l.k IS NULL OR l.h1 <> r.h1 OR l.h2 <> r.h2 \
                 ORDER BY o, k LIMIT {max}"
            ),
        )
        .await?;
        let sink = Arc::new(Mutex::new(DiffSink {
            nk,
            job: job.clone(),
            lists: Lists::default(),
            error: None,
        }));
        let mut out = QueryOutcome {
            sink: Some(RowSinkRef(sink.clone())),
            ..Default::default()
        };
        let s = self.session();
        let r = tokio::select! {
            r = s.execute(
                "SELECT d.kind, l.v, r.v FROM d LEFT JOIN l ON l.k = d.k LEFT JOIN r ON r.k = d.k",
                usize::MAX,
                &mut out,
            ) => r,
            () = ctl.stopped() => return Err(ctl.error()),
        };
        out.sink = None;
        r?;
        if let Some(e) = out.sink_error.take().or(out.error.take()) {
            return Err(CommandError::Sql(e));
        }
        let mut sink = lock(&sink);
        if let Some(e) = sink.error.take() {
            return Err(CommandError::Internal(e));
        }
        Ok(std::mem::take(&mut sink.lists))
    }

    /// Close the session and delete the folder (database, WAL, temp files).
    async fn close(mut self) {
        drop(self.session.take());
        let dir = std::mem::take(&mut self.dir);
        if !local_duckdb::remove_dir(&dir).await {
            tracing::warn!(
                ?dir,
                "data compare: the scratch folder couldn't be deleted yet"
            );
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(self.session.take());
        if !self.dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// The loader's source: the batches the reads hand over.
struct Feed {
    rx: mpsc::UnboundedReceiver<RowBatch>,
    job: Arc<Job>,
}

#[dbine_driver::async_trait]
impl BatchSource for Feed {
    async fn next(&mut self) -> Option<RowBatch> {
        let b = self.rx.recv().await?;
        self.job.queued.fetch_sub(b.bytes as u64, Ordering::SeqCst);
        Some(b)
    }
}

/// The differing rows as they come back from DuckDB.
struct DiffSink {
    nk: usize,
    job: Arc<Job>,
    lists: Lists,
    error: Option<String>,
}

impl RowSink for DiffSink {
    fn begin(&mut self, _: usize, _: &[ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }

    fn row(&mut self, _: usize, row: &[Value]) -> std::io::Result<()> {
        if self.job.ctl.is_stopped() {
            return Err(std::io::Error::other("la comparación se detuvo"));
        }
        let parse =
            |i: usize| -> Option<Vec<Value>> { serde_json::from_str(row.get(i)?.as_str()?).ok() };
        let kind = row.first().and_then(as_u64);
        let ok = match kind {
            Some(0) => parse(1).map(|p| self.lists.only_left.push(p)),
            Some(1) => parse(2).map(|p| self.lists.only_right.push(p)),
            Some(2) => parse(1).zip(parse(2)).map(|(l, r)| {
                let diff = (self.nk..l.len())
                    .filter(|&i| canon(&l[i]) != canon(&r[i]))
                    .collect();
                self.lists.changed.push(ChangedRow {
                    left: l,
                    right: r,
                    diff,
                });
            }),
            _ => None,
        };
        if ok.is_none() && self.error.is_none() {
            self.error = Some("una fila distinta no se pudo leer de la base local".into());
        }
        self.job.differences.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn rows_of(out: &QueryOutcome) -> &[Vec<Value>] {
    out.results
        .iter()
        .find(|r| !r.columns.is_empty())
        .map_or(&[], |r| &r.rows[..])
}

// ---------------------------------------------------------------------------
// Comparing

#[derive(Default)]
struct Lists {
    only_left: Vec<Vec<Value>>,
    only_right: Vec<Vec<Value>>,
    changed: Vec<ChangedRow>,
}

/// The comparison in memory (both sides small): by key, the first row read
/// of a repeated key compared.
fn in_memory(nk: usize, left: Vec<Vec<Value>>, right: Vec<Vec<Value>>) -> (Counts, Lists, u64) {
    let mut duplicate_keys = 0;
    let mut counts = Counts {
        left: left.len() as u64,
        right: right.len() as u64,
        ..Default::default()
    };
    let mut rmap: HashMap<String, Vec<Value>> = HashMap::with_capacity(right.len());
    for p in right {
        match rmap.entry(key_of(&p, nk)) {
            std::collections::hash_map::Entry::Occupied(_) => duplicate_keys += 1,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(p);
            }
        }
    }
    let mut lists = Lists::default();
    let mut seen = HashSet::with_capacity(left.len());
    for p in left {
        let k = key_of(&p, nk);
        if !seen.insert(k.clone()) {
            duplicate_keys += 1;
            continue;
        }
        match rmap.remove(&k) {
            None => lists.only_left.push(p),
            Some(rp) => {
                let diff: Vec<usize> = (nk..p.len())
                    .filter(|&i| canon(&p[i]) != canon(&rp[i]))
                    .collect();
                if diff.is_empty() {
                    counts.same += 1;
                } else {
                    lists.changed.push(ChangedRow {
                        left: p,
                        right: rp,
                        diff,
                    });
                }
            }
        }
    }
    lists.only_right = rmap.into_values().collect();
    counts.only_left = lists.only_left.len() as u64;
    counts.only_right = lists.only_right.len() as u64;
    counts.changed = lists.changed.len() as u64;
    (counts, lists, duplicate_keys)
}

/// Each list in key order and, past `max` rows in all, only the first
/// `max` of the three together in that order.
fn finish(nk: usize, lists: Lists, max: usize) -> Lists {
    type Keyed<T> = Vec<((String, String), T)>;
    fn keyed<T>(rows: Vec<T>, nk: usize, row: impl Fn(&T) -> &[Value]) -> Keyed<T> {
        let mut k: Keyed<T> = rows
            .into_iter()
            .map(|r| {
                let p = row(&r);
                ((order_key(&p[..nk]), key_of(p, nk)), r)
            })
            .collect();
        k.sort_by(|a, b| a.0.cmp(&b.0));
        k
    }
    let mut ol = keyed(lists.only_left, nk, |r| &r[..]);
    let mut or = keyed(lists.only_right, nk, |r| &r[..]);
    let mut ch = keyed(lists.changed, nk, |r| &r.left[..]);
    if ol.len() + or.len() + ch.len() > max {
        let (mut i, mut j, mut c) = (0, 0, 0);
        for _ in 0..max {
            let a = ol.get(i).map(|x| &x.0);
            let b = or.get(j).map(|x| &x.0);
            let d = ch.get(c).map(|x| &x.0);
            let least = [a, b, d].into_iter().flatten().min();
            match least {
                Some(m) if Some(m) == a => i += 1,
                Some(m) if Some(m) == b => j += 1,
                Some(_) => c += 1,
                None => break,
            }
        }
        ol.truncate(i);
        or.truncate(j);
        ch.truncate(c);
    }
    let strip = |k: Keyed<Vec<Value>>| k.into_iter().map(|(_, r)| r).collect();
    Lists {
        only_left: strip(ol),
        only_right: strip(or),
        changed: ch.into_iter().map(|(_, r)| r).collect(),
    }
}

/// Compare two tables whole. `progress` gets the rows read, the phase and
/// the differences brought back, about four times a second while it changes.
pub(crate) async fn compare(
    left: Side<'_>,
    right: Side<'_>,
    input: Input,
    opts: &Options,
    ctl: &Arc<Ctl>,
    progress: &(dyn Fn(&Progress) + Send + Sync),
) -> CommandResult<Compared> {
    // The compared columns, before reading: each side then projects its
    // rows as they come, whatever the other side is doing.
    let names = async {
        tokio::try_join!(
            names_of(ctl, left.session, left.object, &input.cols[0]),
            names_of(ctl, right.session, right.object, &input.cols[1])
        )
    };
    let (lnames, rnames) = names.await?;
    let projection = Arc::new(Projection::new(&input, &lnames, &rnames)?);
    let job = Arc::new(Job::new(ctl.clone(), projection, opts));
    let mut scratch: Option<Scratch> = None;
    let ticker = async {
        let mut last: Option<Progress> = None;
        loop {
            let p = job.progress();
            if last.as_ref() != Some(&p) {
                progress(&p);
                last = Some(p);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    let work = run(&job, left, right, opts, &mut scratch);
    let r = tokio::select! {
        r = work => r,
        _ = ticker => unreachable!("the ticker never ends"),
    };
    progress(&job.progress());
    if let Some(s) = scratch.take() {
        s.close().await;
    }
    r
}

async fn run(
    job: &Arc<Job>,
    left: Side<'_>,
    right: Side<'_>,
    opts: &Options,
    scratch: &mut Option<Scratch>,
) -> CommandResult<Compared> {
    let ctl = &job.ctl;
    let reads = async {
        let r = tokio::try_join!(
            read_side(job, 0, left.session, left.object),
            read_side(job, 1, right.session, right.object)
        );
        if let Err(e) = &r {
            ctl.fail(e.clone());
        }
        job.finish_reads();
        r.map(|_| ())
    };
    // A task of its own: a read waiting for room (its sink holds the reading
    // thread) must not hold up the loader that makes that room.
    let spilling = tokio::spawn(spill(
        job.clone(),
        opts.scratch.clone(),
        opts.duckdb.clone(),
    ));
    let (read, spilled) = tokio::join!(reads, spilling);
    let (opened, spilled) =
        spilled.unwrap_or_else(|e| (None, Err(CommandError::Internal(e.to_string()))));
    *scratch = opened;
    if ctl.is_stopped() {
        return Err(ctl.error());
    }
    read?;
    let spilled = spilled?;
    let projection = job.projection.clone();
    let nk = projection.nk;
    job.set_phase(Phase::Comparing);
    let (counts, lists, duplicate_keys) = match scratch.as_mut().filter(|_| spilled) {
        Some(s) => {
            let (counts, dups) = s.counts(ctl).await?;
            let read = job.progress().rows;
            if [counts.left, counts.right] != read {
                return Err(CommandError::Internal(format!(
                    "la base local recibió {}/{} filas de {}/{} leídas",
                    counts.left, counts.right, read[0], read[1]
                )));
            }
            job.set_phase(Phase::LoadingDifferences);
            let lists = s.differences(ctl, job, nk, opts.max_diff_rows).await?;
            (counts, lists, dups)
        }
        None => {
            let take = |side: usize| std::mem::take(&mut lock(&job.bufs[side]).rows);
            let (l, r) = (take(0), take(1));
            in_memory(nk, l, r)
        }
    };
    let total = counts.only_left + counts.only_right + counts.changed;
    let lists = finish(nk, lists, opts.max_diff_rows);
    let last = |side: usize| lock(&job.bufs[side]).last.as_ref().map(|(_, v)| v.clone());
    Ok(Compared {
        last_keys: (nk == 1).then(|| (last(0), last(1))),
        counts,
        only_left: lists.only_left,
        only_right: lists.only_right,
        changed: lists.changed,
        duplicate_keys,
        diff_truncated: total > opts.max_diff_rows as u64,
        spilled,
        projection,
    })
}

/// From half the limits: open the scratch DuckDB; past them: hand it what's
/// in memory, then load the rest as it's read. `false`: it stayed in memory
/// (a scratch database that didn't open then doesn't matter). The scratch
/// database comes back whatever happened, for the caller to close.
async fn spill(
    job: Arc<Job>,
    root: Option<PathBuf>,
    duckdb: Option<Arc<dyn Driver>>,
) -> (Option<Scratch>, CommandResult<bool>) {
    let ctl = &job.ctl;
    let flag = |f: &AtomicBool| f.load(Ordering::SeqCst);
    job.wait_for(|| flag(&job.spill_soon) || flag(&job.spill_wanted) || flag(&job.reads_done))
        .await;
    if ctl.is_stopped() || !(flag(&job.spill_soon) || flag(&job.spill_wanted)) {
        return (None, Ok(false));
    }
    // Reads that end in memory while it opens (DuckDB downloading on first
    // use…) don't wait for it: it's dropped, folder and all.
    let open = Scratch::open(root.as_deref(), ctl, duckdb.as_ref(), MEMORY_LIMIT);
    let in_memory = job.wait_for(|| flag(&job.reads_done) && !flag(&job.spill_wanted));
    let opened = tokio::select! {
        r = open => r,
        () = in_memory => return (None, Ok(false)),
    };
    job.wait_for(|| flag(&job.spill_wanted) || flag(&job.reads_done))
        .await;
    if ctl.is_stopped() {
        return (opened.ok(), Err(ctl.error()));
    }
    if !flag(&job.spill_wanted) {
        return (opened.ok(), Ok(false));
    }
    let mut scratch = match opened {
        Ok(s) => s,
        Err(e) => {
            ctl.fail(e.clone());
            job.loader_gone.store(true, Ordering::SeqCst);
            return (None, Err(e));
        }
    };
    let r = async {
        scratch.create(ctl).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut feed = Feed {
            rx,
            job: job.clone(),
        };
        // What was in memory goes while the load runs.
        let install = async {
            let r = job.install(tx).await;
            if let Err(e) = &r {
                ctl.fail(e.clone());
            }
            r
        };
        let (installed, loaded) = tokio::join!(install, scratch.load(ctl, &mut feed));
        let loaded = installed.and(loaded.map(|_| ()));
        job.loader_gone.store(true, Ordering::SeqCst);
        drop(feed);
        loaded
    }
    .await;
    match r {
        Ok(()) => (Some(scratch), Ok(true)),
        Err(e) => {
            ctl.fail(e.clone());
            job.loader_gone.store(true, Ordering::SeqCst);
            (Some(scratch), Err(e))
        }
    }
}

#[cfg(test)]
#[path = "data_compare_whole_tests.rs"]
mod tests;

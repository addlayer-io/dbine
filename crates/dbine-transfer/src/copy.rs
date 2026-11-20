//! One table's rows: the driver's own copy, or a reader and a writer joined
//! by a channel of at most [`CHANNEL_BATCHES`] batches.
//!
//! The reader runs `read_batches` on a blocking thread, because a
//! [`BatchSink`] may block while the writer is behind (that's the
//! backpressure that bounds memory). A failure on either side stops the
//! other one.

use crate::event::{CopyPath, CopyStats, LogLevel};
use crate::job::TransferJob;
use crate::{lock, panic_text};
use dbine_driver::transfer::{BatchSinkRef, Progress};
use dbine_driver::{
    async_trait, BatchSink, BatchSource, Cell, CopySpec, DeltaSpec, Driver, Error, LoadSpec,
    QueryOutcome, Result, ResultColumn, RowBatch, Session, TransferColumn,
};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, error::TryRecvError};

/// Batches in flight per table, between the reader and the writer (queued
/// or being written). Each closes at `CHUNK_BYTES`: ~32 MiB per table.
pub const CHANNEL_BATCHES: usize = 16;

/// What a copy needs besides its sessions.
pub(crate) struct CopyInput<'a> {
    pub job: &'a TransferJob,
    pub source_driver: &'a dyn Driver,
    pub target_driver: &'a dyn Driver,
    /// Try [`Driver::copy_native`] first.
    pub native: bool,
    pub progress: Progress<'a>,
    pub log: &'a (dyn Fn(LogLevel, String) + Send + Sync),
    /// Sync by rows: the rows go to [`Session::delta_apply`] instead of a
    /// load.
    pub delta: Option<DeltaWrite<'a>>,
}

/// A sync's write: the target's spec, its buckets (empty: all of them) and
/// the source filter that reads their rows (`None`: every row).
pub(crate) struct DeltaWrite<'a> {
    pub spec: &'a DeltaSpec,
    pub buckets: &'a [i64],
    pub filter: Option<String>,
}

/// Copy the table's rows from `src` into `tgt`.
pub(crate) async fn copy_table(
    input: CopyInput<'_>,
    src: Box<dyn Session>,
    tgt: &mut dyn Session,
) -> Result<CopyStats> {
    copy_table_typed(input, &[], src, tgt).await
}

/// [`copy_table`] knowing the target columns' types (by name; empty:
/// untyped): where the rows go as the driver's insert script, it writes
/// each value as its column takes it ([`Driver::insert_script_typed`]).
pub(crate) async fn copy_table_typed(
    input: CopyInput<'_>,
    types: &[ResultColumn],
    mut src: Box<dyn Session>,
    tgt: &mut dyn Session,
) -> Result<CopyStats> {
    let job = input.job;
    if input.native && input.delta.is_none() {
        let spec = CopySpec {
            source: job.source.clone(),
            target: job.target.clone(),
        };
        let start = Instant::now();
        match input
            .source_driver
            .copy_native(&mut *src, tgt, &spec, input.progress)
            .await
        {
            Ok(rows) => {
                after_load(job, tgt).await?;
                return Ok(CopyStats::measure(
                    CopyPath::Native,
                    rows,
                    start.elapsed(),
                    Duration::ZERO,
                    Duration::ZERO,
                ));
            }
            Err(Error::Unsupported(why)) => (input.log)(
                LogLevel::Info,
                format!(
                    "{}: sin copia directa ({why}); se copia por lotes",
                    job.name
                ),
            ),
            Err(e) => return Err(e),
        }
    }

    let start = Instant::now();
    let window = Window::new(CHANNEL_BATCHES);
    // However the copy ends (error, cancel, panic), a reader waiting for
    // room stops waiting.
    let _close = CloseOnDrop(window.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    let sink = Arc::new(Mutex::new(ChannelSink {
        tx: Some(tx),
        window: window.clone(),
        waited: Duration::ZERO,
        began: false,
    }));
    let src_interrupt = src.interrupter();

    let mut spec = job.source.clone();
    if let Some(d) = &input.delta {
        spec.filter = d.filter.clone();
    }
    let handle = tokio::runtime::Handle::current();
    let mut reader = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let dyn_sink: BatchSinkRef = sink.clone();
            let r = src.read_batches(&spec, dyn_sink).await;
            drop(src);
            let mut s = lock(&sink);
            if r.is_ok() {
                if let Some(tx) = &s.tx {
                    let _ = tx.send(Msg::End);
                }
            }
            // Without `End`, the writer never takes a closed channel for the
            // end of the table.
            s.tx = None;
            r.map(|_| s.waited)
        })
    });

    let mut source = ChannelSource {
        rx,
        first: None,
        held: None,
        ended: false,
        waited: Duration::ZERO,
        rows: 0,
        bits: Vec::new(),
    };
    let (target_driver, progress, delta) = (input.target_driver, input.progress, input.delta);
    let delta_path = delta.is_some();
    let writer = async move {
        let columns = match source.columns().await {
            Some(c) => c,
            None => fallback_columns(job),
        };
        // A load without columns takes the read's, whatever they turn out
        // to be (a whole-document copy: every field the documents have).
        let named;
        let target = if job.target.columns.is_empty() {
            named = LoadSpec {
                columns: columns.iter().map(|c| c.name.clone()).collect(),
                ..job.target.clone()
            };
            &named
        } else {
            &job.target
        };
        // A sync by rows is between two databases of one driver: its rows
        // keep their engine's form.
        if !delta_path {
            source.bits = bit_vector_columns(job, &target.columns, &columns, types);
        }
        let mut result = None;
        let rows = if let Some(d) = &delta {
            result = Some(
                tgt.delta_apply(d.spec, d.buckets, &columns, &mut source, progress)
                    .await?,
            );
            source.rows
        } else if target_driver.supports_bulk_load() {
            tgt.bulk_load(target, &columns, &mut source, progress)
                .await?
        } else {
            insert_batches(
                job,
                target,
                types,
                target_driver,
                tgt,
                &mut source,
                progress,
            )
            .await?
        };
        after_load(job, tgt).await?;
        Ok::<_, Error>((rows, source.waited, result))
    };
    tokio::pin!(writer);

    let (rows, waited_src, waited_dst, result) = tokio::select! {
        r = &mut reader => {
            // A failed read drops the writer: the load's open batch is
            // rolled back with it.
            let waited_dst = joined(r)?;
            let (rows, waited_src, result) = writer.await?;
            (rows, waited_src, waited_dst, result)
        }
        w = &mut writer => match w {
            Ok((rows, waited_src, result)) => {
                // The writer only ends after the reader's `End`; if a driver
                // stopped consuming early, the reader fails instead of waiting.
                window.close();
                let waited_dst = joined(reader.await)?;
                (rows, waited_src, waited_dst, result)
            }
            Err(e) => {
                // The reader ends at its next batch (or now, interrupted);
                // it's left to finish on its own.
                window.close();
                if let Some(stop) = &src_interrupt {
                    stop();
                }
                return Err(e);
            }
        }
    };
    let path = if delta_path {
        CopyPath::Delta
    } else if target_driver.supports_bulk_load() {
        CopyPath::BulkLoad
    } else {
        CopyPath::InsertScript
    };
    let mut stats = CopyStats::measure(path, rows, start.elapsed(), waited_src, waited_dst);
    stats.delta = result;
    Ok(stats)
}

/// The job's `after` statements, whatever path loaded the rows: they fix
/// up the table after explicit key values were written (PostgreSQL's
/// sequence resync, Oracle's identity restart, SQL Server's
/// `IDENTITY_INSERT … OFF`, harmless when it wasn't on).
async fn after_load(job: &TransferJob, tgt: &mut dyn Session) -> Result<()> {
    if job.after.trim().is_empty() {
        return Ok(());
    }
    exec(tgt, &job.after).await
}

/// The reader's outcome, a panic included.
fn joined(r: std::result::Result<Result<Duration>, tokio::task::JoinError>) -> Result<Duration> {
    match r {
        Ok(r) => r,
        Err(e) if e.is_panic() => Err(Error::State(format!(
            "error inesperado leyendo el origen: {}",
            panic_text(&*e.into_panic())
        ))),
        Err(_) => Err(Error::Cancelled),
    }
}

/// Columns for the load when the reader didn't announce them.
fn fallback_columns(job: &TransferJob) -> Vec<TransferColumn> {
    if !job.expected_columns.is_empty() {
        return job.expected_columns.clone();
    }
    job.target
        .columns
        .iter()
        .map(|c| TransferColumn {
            name: c.clone(),
            type_name: String::new(),
            nullable: true,
        })
        .collect()
}

/// The path without a native bulk load: each batch as the driver's
/// `insert_script`, after the job's `before` (its `after` runs on every
/// path, in [`copy_table`]).
async fn insert_batches(
    job: &TransferJob,
    target: &LoadSpec,
    types: &[ResultColumn],
    driver: &dyn Driver,
    tgt: &mut dyn Session,
    source: &mut ChannelSource,
    progress: Progress<'_>,
) -> Result<u64> {
    if !job.before.trim().is_empty() {
        exec(tgt, &job.before).await?;
    }
    let columns = typed_columns(&target.columns, types);
    let vectors: Vec<bool> = target
        .columns
        .iter()
        .zip(&columns)
        .map(|(c, t)| {
            job.vector_columns.iter().any(|v| v.eq_ignore_ascii_case(c))
                || t.vector.as_ref().is_some_and(|m| !m.sparse)
        })
        .collect();
    // Types that say nothing are no types: the untyped script, as before.
    let typed = columns
        .iter()
        .any(|c| !c.type_name.trim().is_empty() || c.vector.is_some());
    let mut done = 0u64;
    while let Some(batch) = source.next().await {
        let rows: Vec<Vec<serde_json::Value>> = batch
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if vectors.get(i).copied().unwrap_or(false) {
                            script_vector(c)
                        } else {
                            c.to_json()
                        }
                    })
                    .collect()
            })
            .collect();
        let sql = if typed {
            driver.insert_script_typed(&target.table, &columns, &rows)?
        } else {
            driver.insert_script(&target.table, &target.columns, &rows)?
        };
        exec(tgt, &sql).await?;
        done += batch.len() as u64;
        progress(done);
    }
    Ok(done)
}

/// The type of each of `names` in `types` (by name: as written, then
/// without case), named as `names` says; a name `types` lacks is untyped.
pub(crate) fn typed_columns(names: &[String], types: &[ResultColumn]) -> Vec<ResultColumn> {
    names
        .iter()
        .map(|n| {
            types
                .iter()
                .find(|c| &c.name == n)
                .or_else(|| types.iter().find(|c| c.name.eq_ignore_ascii_case(n)))
                .map_or_else(
                    || ResultColumn::new(n.clone(), ""),
                    |c| ResultColumn {
                        name: n.clone(),
                        ..c.clone()
                    },
                )
        })
        .collect()
}

/// The load's columns (`names`, in the order of the cells) that take bit
/// strings as binary vectors: the read says the column is a bit string
/// (PostgreSQL's `bit(n)` / `varbit`, pgvector's binary vectors, read as
/// `1011…`) and the target column holds vectors (the job's
/// `vector_columns`, or its type says so). Their cells go as the
/// contract's binary vector, a JSON array of 0 and 1, which every vector
/// loader and insert script takes (libSQL's `F1BIT_BLOB`, MongoDB's bit
/// vectors, Oracle's `VECTOR(n, BINARY)`…); a bit string is no vector to
/// them. The reader can't do it: it doesn't know where the rows go, and a
/// `bit(n)` that isn't a vector on the target stays a bit string.
fn bit_vector_columns(
    job: &TransferJob,
    names: &[String],
    read: &[TransferColumn],
    types: &[ResultColumn],
) -> Vec<bool> {
    let typed = typed_columns(names, types);
    let flags: Vec<bool> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let source = if read.len() == names.len() {
                read.get(i)
            } else {
                read.iter().find(|c| c.name.eq_ignore_ascii_case(name))
            };
            let vector = job
                .vector_columns
                .iter()
                .any(|v| v.eq_ignore_ascii_case(name))
                || typed[i].vector.as_ref().is_some_and(|m| !m.sparse);
            vector && source.is_some_and(|c| is_bit_string_type(&c.type_name))
        })
        .collect();
    if flags.contains(&true) {
        flags
    } else {
        Vec::new()
    }
}

/// A bit string type, as a read names it: `bit`, `bit(16)`, `varbit`,
/// `bit varying(16)` (case and blanks don't matter).
fn is_bit_string_type(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    matches!(base, "bit" | "varbit" | "bit varying")
}

/// A bit string cell (`1011`) as the JSON array of its bits (`[1,0,1,1]`);
/// any other cell (NULL, an array already) as it is.
fn bits_as_array(c: &mut Cell) {
    if let Cell::Text(s) = c {
        if !s.is_empty() && s.bytes().all(|b| b == b'0' || b == b'1') {
            let mut out = String::with_capacity(2 * s.len() + 1);
            out.push('[');
            for (i, b) in s.bytes().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push(b as char);
            }
            out.push(']');
            *c = Cell::Json(out);
        }
    }
}

/// A vector column's cell as `insert_script` takes it: the JSON array of
/// its numbers (from the JSON-array text a vector travels as); a cell that
/// isn't one (NULL, a sparse vector's own text) as it is.
fn script_vector(c: &Cell) -> serde_json::Value {
    let v = c.to_json();
    if matches!(v, serde_json::Value::Array(_)) {
        return v;
    }
    match dbine_driver::vector::parse_value(&v) {
        Some(nums) => serde_json::Value::Array(
            nums.into_iter()
                .map(dbine_driver::model::json_f64)
                .collect(),
        ),
        None => v,
    }
}

/// Run a script; a statement's error as an `Err`.
pub(crate) async fn exec(s: &mut dyn Session, sql: &str) -> Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 0, &mut out).await?;
    match out.error {
        Some(e) => Err(Error::Query(e)),
        None => Ok(()),
    }
}

// -- the channel --------------------------------------------------------------------------------

enum Msg {
    Begin(Vec<TransferColumn>),
    Batch(RowBatch, WindowSlot),
    /// The read finished well.
    End,
}

/// Counts batches in flight; the reader blocks while it's full.
struct Window {
    state: Mutex<WindowState>,
    freed: Condvar,
    cap: usize,
}

struct WindowState {
    used: usize,
    closed: bool,
}

impl Window {
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(Window {
            state: Mutex::new(WindowState {
                used: 0,
                closed: false,
            }),
            freed: Condvar::new(),
            cap,
        })
    }

    /// Wait for room; `None` once closed.
    fn acquire(self: &Arc<Self>) -> Option<WindowSlot> {
        let mut st = lock(&self.state);
        while st.used >= self.cap && !st.closed {
            st = self.freed.wait(st).unwrap_or_else(|p| p.into_inner());
        }
        if st.closed {
            return None;
        }
        st.used += 1;
        Some(WindowSlot(self.clone()))
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.freed.notify_all();
    }
}

/// One batch's room in the window, freed when the writer is done with it.
struct WindowSlot(Arc<Window>);

impl Drop for WindowSlot {
    fn drop(&mut self) {
        lock(&self.0.state).used -= 1;
        self.0.freed.notify_one();
    }
}

struct CloseOnDrop(Arc<Window>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The reader's end.
struct ChannelSink {
    tx: Option<mpsc::UnboundedSender<Msg>>,
    window: Arc<Window>,
    /// Time blocked waiting for room: the destination was behind.
    waited: Duration,
    began: bool,
}

fn stopped() -> io::Error {
    io::Error::other("se detuvo la escritura en el destino")
}

impl ChannelSink {
    fn send(&self, m: Msg) -> io::Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(stopped)?
            .send(m)
            .map_err(|_| stopped())
    }
}

impl BatchSink for ChannelSink {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        if self.began {
            return Ok(());
        }
        self.began = true;
        self.send(Msg::Begin(columns.to_vec()))
    }

    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let t = Instant::now();
        let slot = self.window.acquire().ok_or_else(stopped)?;
        self.waited += t.elapsed();
        self.send(Msg::Batch(batch, slot))
    }
}

/// The writer's end. Cancel safe: `next` only awaits the channel.
struct ChannelSource {
    rx: mpsc::UnboundedReceiver<Msg>,
    /// A batch that came before any `Begin`.
    first: Option<(RowBatch, WindowSlot)>,
    /// The batch the load has now; its room frees on the next call.
    held: Option<WindowSlot>,
    ended: bool,
    /// Time waiting for batches: the source was behind.
    waited: Duration,
    /// Rows handed to the writer.
    rows: u64,
    /// The cells (by position) whose bit strings go as binary vectors
    /// ([`bit_vector_columns`]); empty: none.
    bits: Vec<bool>,
}

impl ChannelSource {
    /// A batch as the load takes it: its bit strings for vectors as arrays.
    fn shaped(&self, mut b: RowBatch) -> RowBatch {
        if !self.bits.is_empty() {
            for row in &mut b.rows {
                for (c, bit) in row.iter_mut().zip(&self.bits) {
                    if *bit {
                        bits_as_array(c);
                    }
                }
            }
        }
        b
    }

    /// The next message. If the reader went away without `End` (it failed)
    /// this never returns: the copy ends through the reader's error.
    async fn recv(&mut self) -> Msg {
        let msg = match self.rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) => {
                let t = Instant::now();
                let m = self.rx.recv().await;
                self.waited += t.elapsed();
                m
            }
            Err(TryRecvError::Disconnected) => None,
        };
        match msg {
            Some(m) => m,
            None => std::future::pending::<Msg>().await,
        }
    }

    /// The read's columns, when it announces them before any batch.
    async fn columns(&mut self) -> Option<Vec<TransferColumn>> {
        match self.recv().await {
            Msg::Begin(c) => Some(c),
            Msg::Batch(b, s) => {
                self.first = Some((b, s));
                None
            }
            Msg::End => {
                self.ended = true;
                None
            }
        }
    }
}

#[async_trait]
impl BatchSource for ChannelSource {
    async fn next(&mut self) -> Option<RowBatch> {
        self.held = None;
        if let Some((b, s)) = self.first.take() {
            self.held = Some(s);
            self.rows += b.len() as u64;
            return Some(self.shaped(b));
        }
        loop {
            if self.ended {
                return None;
            }
            match self.recv().await {
                Msg::Batch(b, s) => {
                    self.held = Some(s);
                    self.rows += b.len() as u64;
                    return Some(self.shaped(b));
                }
                Msg::Begin(_) => {}
                Msg::End => self.ended = true,
            }
        }
    }
}

//! The run: tables in parallel (the limit changes live), a live queue,
//! retries, cancellation and resume.
//!
//! Per table, in this order (see `docs/transferencia-masiva.md`, 2.6):
//! 1. one worker per table, checked before anything else;
//! 2. a table already copied is never emptied: only its `post` runs;
//! 3. the target's columns are checked against the expected ones;
//! 4. a table that may hold rows of an earlier attempt is emptied first; a
//!    preexisting one with rows of its own is not copied into;
//! 5. copy, mark it copied, run its `post`, mark it done.
//!
//! A sync by rows ([`TransferMode::Delta`]) skips 4: it is never emptied,
//! not on resume, retry or cancel (its merge is one transaction).

use crate::check::column_differences;
use crate::copy::{self, exec, CopyInput};
use crate::delta::{self, DeltaInput};
use crate::event::{rate, CopyStats, Event, LogLevel, Phase, RunSummary};
use crate::job::{sort_jobs, RunOptions, TransferJob, TransferMode};
use crate::retry::{backoff, is_transient};
use crate::slots::{Slot, Slots};
use crate::state::{RunSpec, RunStatus, Store, TableState, TableStatus};
use crate::{lock, panic_text};
use dbine_driver::{async_trait, Driver, Error, ObjectRef, QueryOutcome, Result, Session};
use futures::FutureExt;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinSet;

/// A table's progress reaches the UI and the state at most this often.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(5);

/// The two databases of a run, as the app reaches them.
#[async_trait]
pub trait Endpoints: Send + Sync {
    fn source_driver(&self) -> Arc<dyn Driver>;
    fn target_driver(&self) -> Arc<dyn Driver>;
    /// A new connection to the source, only for reading (the app wraps it
    /// read-only). One per table and attempt.
    async fn open_source(&self) -> Result<Box<dyn Session>>;
    /// A new connection to the target. One per table and attempt.
    async fn open_target(&self) -> Result<Box<dyn Session>>;
    /// Whether to try [`Driver::copy_native`] (both ends in one driver).
    fn native_copy_allowed(&self) -> bool {
        self.source_driver().supports_native_copy(self.target_driver().info().id)
    }
}

/// How a run ended, table by table.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub summary: RunSummary,
    pub tables: Vec<TableState>,
}

type EventFn = Arc<dyn Fn(Event) + Send + Sync>;
type Interrupter = Arc<dyn Fn() + Send + Sync>;

/// Runs one transfer (by id): its first run, and its resumes.
pub struct Engine {
    shared: Arc<Shared>,
}

/// Changes a run while it goes. Cheap to clone.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

struct Shared {
    run_id: String,
    store: Arc<Store>,
    slots: Arc<Slots>,
    queue: Mutex<Queue>,
    /// Wakes the dispatcher: queue changes, boosts, cancel.
    wake: Notify,
    /// Tables with a worker (the in-flight check).
    running: Mutex<HashMap<String, Arc<TableCtl>>>,
    stop: AtomicBool,
    stopped_by_failure: AtomicBool,
    events: Mutex<Option<EventFn>>,
}

struct Queue {
    items: VecDeque<TransferJob>,
    /// "Ejecutar ahora": started without a slot.
    boost: VecDeque<TransferJob>,
    /// Closed once the run ends: nothing added later is lost silently.
    open: bool,
}

/// A running table's handles.
#[derive(Default)]
struct TableCtl {
    cancelled: AtomicBool,
    notify: Notify,
    interrupters: Mutex<Vec<Interrupter>>,
}

impl TableCtl {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait_cancel(&self) {
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.cancelled.load(Ordering::SeqCst) {
                return;
            }
            n.await;
        }
    }

    fn add_interrupter(&self, i: Option<Interrupter>) {
        if let Some(i) = i {
            lock(&self.interrupters).push(i);
        }
    }

    fn interrupt(&self) {
        for i in lock(&self.interrupters).drain(..) {
            i();
        }
    }
}

/// What a run's workers share.
struct Env {
    endpoints: Arc<dyn Endpoints>,
    source_driver: Arc<dyn Driver>,
    target_driver: Arc<dyn Driver>,
    events: EventFn,
    options: RunOptions,
    native: bool,
    same_engine: bool,
}

impl Env {
    fn emit(&self, e: Event) {
        (self.events)(e);
    }

    fn log(&self, level: LogLevel, text: String) {
        match level {
            LogLevel::Info => tracing::info!("{text}"),
            LogLevel::Warn => tracing::warn!("{text}"),
            LogLevel::Error => tracing::error!("{text}"),
        }
        self.emit(Event::Log { level, text });
    }

    fn phase(&self, table: &str, phase: Phase) {
        self.emit(Event::TablePhase { table: table.to_string(), phase });
    }
}

/// How a worker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Cancelled,
    /// Not run: already done, already running, or the run stopped.
    Skipped,
}

impl Engine {
    /// An engine for the run `run_id`, with its state in `store`.
    pub fn new(store: Arc<Store>, run_id: impl Into<String>) -> Self {
        Engine {
            shared: Arc::new(Shared {
                run_id: run_id.into(),
                store,
                slots: Slots::new(RunOptions::default().parallel),
                queue: Mutex::new(Queue { items: VecDeque::new(), boost: VecDeque::new(), open: true }),
                wake: Notify::new(),
                running: Mutex::new(HashMap::new()),
                stop: AtomicBool::new(false),
                stopped_by_failure: AtomicBool::new(false),
                events: Mutex::new(None),
            }),
        }
    }

    pub fn control(&self) -> Control {
        Control { shared: self.shared.clone() }
    }

    pub fn run_id(&self) -> &str {
        &self.shared.run_id
    }

    /// Copy `jobs`. Returns when every table ended (or the run was
    /// cancelled); the tables' outcomes are in the report and the state.
    pub async fn run(
        &self,
        jobs: Vec<TransferJob>,
        options: RunOptions,
        endpoints: Arc<dyn Endpoints>,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<RunReport> {
        let sh = &self.shared;
        let options = options.normalized();
        sh.store.begin_run(&sh.run_id, &RunSpec { jobs: jobs.clone(), options: options.clone() })?;
        for j in &jobs {
            sh.store.add_table(&sh.run_id, &j.name, j.row_estimate)?;
        }
        self.dispatch(jobs, options, endpoints, Arc::new(events)).await
    }

    /// Go on with a run after a cut or a cancel: every table not done is
    /// dispatched again, by the state rules (copied: only its `post`; may
    /// hold rows: emptied and copied again). `options`: `None` keeps the
    /// saved ones.
    pub async fn resume(
        &self,
        options: Option<RunOptions>,
        endpoints: Arc<dyn Endpoints>,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<RunReport> {
        let sh = &self.shared;
        let spec = sh.store.run_spec(&sh.run_id)?.ok_or_else(|| Error::State("la transferencia no existe".into()))?;
        let options = options.unwrap_or(spec.options).normalized();
        let states: HashMap<String, TableState> = sh.store.tables(&sh.run_id)?.into_iter().map(|t| (t.name.clone(), t)).collect();
        let mut jobs = Vec::new();
        for j in spec.jobs {
            match states.get(&j.name) {
                Some(t) if t.status == TableStatus::Done => continue,
                Some(_) => {}
                None => sh.store.add_table(&sh.run_id, &j.name, j.row_estimate)?,
            }
            jobs.push(j);
        }
        sh.store.resume_run(&sh.run_id)?;
        self.dispatch(jobs, options, endpoints, Arc::new(events)).await
    }

    /// "Reintentar las que fallaron": failed and cancelled tables go back to
    /// pending and the run goes on.
    pub async fn retry_failed(
        &self,
        options: Option<RunOptions>,
        endpoints: Arc<dyn Endpoints>,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<RunReport> {
        self.shared.store.requeue_failed(&self.shared.run_id)?;
        self.resume(options, endpoints, events).await
    }

    async fn dispatch(&self, mut jobs: Vec<TransferJob>, options: RunOptions, endpoints: Arc<dyn Endpoints>, events: EventFn) -> Result<RunReport> {
        let sh = &self.shared;
        let started = Instant::now();
        sh.stop.store(false, Ordering::SeqCst);
        sh.stopped_by_failure.store(false, Ordering::SeqCst);
        *lock(&sh.events) = Some(events.clone());
        sh.slots.set(options.parallel);
        sort_jobs(&mut jobs, options.order);
        let source_driver = endpoints.source_driver();
        let target_driver = endpoints.target_driver();
        let env = Arc::new(Env {
            native: endpoints.native_copy_allowed(),
            same_engine: source_driver.info().id == target_driver.info().id,
            endpoints,
            source_driver,
            target_driver,
            events,
            options,
        });
        let total = jobs.len();
        {
            let mut q = lock(&sh.queue);
            q.open = true;
            // Tables queued before the run started go after the planned ones.
            let early: Vec<TransferJob> = q.items.drain(..).collect();
            q.items.extend(jobs);
            q.items.extend(early);
        }
        env.emit(Event::RunStarted { run_id: sh.run_id.clone(), tables: total, parallel: sh.slots.target() });

        let mut set: JoinSet<Outcome> = JoinSet::new();
        let mut names: HashMap<tokio::task::Id, String> = HashMap::new();
        loop {
            while let Some(r) = set.try_join_next_with_id() {
                self.joined(r, &mut names, &env);
            }
            if sh.stop.load(Ordering::SeqCst) {
                break;
            }
            let wake = sh.wake.notified();
            let next = {
                let mut q = lock(&sh.queue);
                if let Some(j) = q.boost.pop_front() {
                    Next::Boosted(Box::new(j))
                } else if !q.items.is_empty() {
                    // The table stays queued while it waits for a slot, so
                    // `cancel_table` and `run_now` still find it.
                    Next::Queued
                } else if set.is_empty() {
                    // Nothing queued, nothing running: close under the same
                    // lock `enqueue` takes, so no table is lost.
                    q.open = false;
                    Next::Closed
                } else {
                    Next::Idle
                }
            };
            match next {
                Next::Boosted(job) => {
                    let name = job.name.clone();
                    let h = set.spawn(worker(sh.clone(), env.clone(), *job, None));
                    names.insert(h.id(), name);
                }
                Next::Queued => {
                    tokio::select! {
                        slot = sh.slots.acquire() => {
                            // Taken only now: while waiting it may have been
                            // cancelled or boosted (then the slot goes back).
                            let job = if sh.stop.load(Ordering::SeqCst) { None } else { lock(&sh.queue).items.pop_front() };
                            if let Some(job) = job {
                                let name = job.name.clone();
                                let h = set.spawn(worker(sh.clone(), env.clone(), job, Some(slot)));
                                names.insert(h.id(), name);
                            }
                        }
                        _ = wake => {}
                    }
                }
                Next::Closed => break,
                Next::Idle => {
                    tokio::select! {
                        _ = wake => {}
                        Some(r) = set.join_next_with_id() => self.joined(r, &mut names, &env),
                    }
                }
            }
        }
        {
            // What's left stays pending in the state (a resume rebuilds it
            // from the spec): a later dispatch must not queue it again.
            let mut q = lock(&sh.queue);
            q.open = false;
            q.items.clear();
            q.boost.clear();
        }
        while let Some(r) = set.join_next_with_id().await {
            self.joined(r, &mut names, &env);
        }

        let status = if sh.stopped_by_failure.load(Ordering::SeqCst) {
            RunStatus::Failed
        } else if sh.stop.load(Ordering::SeqCst) {
            RunStatus::Cancelled
        } else {
            RunStatus::Finished
        };
        sh.store.finish_run(&sh.run_id, status)?;
        let tables = sh.store.tables(&sh.run_id)?;
        let count = |s: TableStatus| tables.iter().filter(|t| t.status == s).count();
        let summary = RunSummary {
            run_id: sh.run_id.clone(),
            status,
            done: count(TableStatus::Done),
            failed: count(TableStatus::Failed),
            cancelled: count(TableStatus::Cancelled),
            pending: count(TableStatus::Pending) + count(TableStatus::Copied) + count(TableStatus::Running),
            rows: tables.iter().map(|t| t.rows_done).sum(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        env.emit(Event::RunFinished { summary: summary.clone() });
        Ok(RunReport { summary, tables })
    }

    /// A worker ended.
    fn joined(&self, r: std::result::Result<(tokio::task::Id, Outcome), tokio::task::JoinError>, names: &mut HashMap<tokio::task::Id, String>, env: &Env) {
        let sh = &self.shared;
        let outcome = match r {
            Ok((id, o)) => {
                names.remove(&id);
                o
            }
            Err(e) => {
                // The worker catches its own panics; this is one in its
                // bookkeeping.
                let name = names.remove(&e.id()).unwrap_or_default();
                let text = if e.is_panic() { format!("error inesperado: {}", panic_text(&*e.into_panic())) } else { "se interrumpió".into() };
                let _ = sh.store.finish_table(&sh.run_id, &name, TableStatus::Failed, Some(&text), None);
                env.emit(Event::TableFailed { table: name, error: text });
                Outcome::Failed
            }
        };
        if outcome == Outcome::Failed && env.options.fail_fast && !sh.stop.load(Ordering::SeqCst) {
            env.log(LogLevel::Warn, "Se detiene la transferencia: falló una tabla".into());
            sh.stopped_by_failure.store(true, Ordering::SeqCst);
            sh.stop_all();
        }
    }
}

impl Shared {
    fn stop_all(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for ctl in lock(&self.running).values() {
            ctl.cancel();
        }
        self.wake.notify_one();
    }

    fn emit(&self, e: Event) {
        let f = lock(&self.events).clone();
        if let Some(f) = f {
            f(e);
        }
    }
}

impl Control {
    /// Tables at once, from now on (1 to 32). Lowering it doesn't stop the
    /// running ones: it takes effect as they end.
    pub fn set_parallel(&self, n: usize) {
        self.shared.slots.set(n);
        self.shared.wake.notify_one();
    }

    pub fn parallel(&self) -> usize {
        self.shared.slots.target()
    }

    /// Tables with a worker now.
    pub fn running(&self) -> Vec<String> {
        lock(&self.shared.running).keys().cloned().collect()
    }

    /// Cancel a table: a running one stops (its rows are removed unless it
    /// was already copied); a queued one leaves the queue. `false`: not
    /// found.
    pub fn cancel_table(&self, name: &str) -> bool {
        let sh = &self.shared;
        if let Some(ctl) = lock(&sh.running).get(name) {
            ctl.cancel();
            return true;
        }
        let removed = {
            let mut q = lock(&sh.queue);
            let before = q.items.len() + q.boost.len();
            q.items.retain(|j| j.name != name);
            q.boost.retain(|j| j.name != name);
            before != q.items.len() + q.boost.len()
        };
        if removed {
            let _ = sh.store.finish_table(&sh.run_id, name, TableStatus::Cancelled, None, None);
            sh.emit(Event::TableCancelled { table: name.to_string() });
            // The dispatcher may be waiting for a slot for it.
            sh.wake.notify_one();
        }
        removed
    }

    /// Cancel the run: running tables stop, queued ones stay pending (a
    /// resume picks them up).
    pub fn cancel_all(&self) {
        self.shared.stop_all();
    }

    /// Add a table to the running run (it's registered as pending first).
    /// `Ok(false)`: the run already ended.
    pub fn enqueue(&self, job: TransferJob) -> Result<bool> {
        let sh = &self.shared;
        let mut q = lock(&sh.queue);
        if !q.open || sh.stop.load(Ordering::SeqCst) {
            return Ok(false);
        }
        sh.store.add_table(&sh.run_id, &job.name, job.row_estimate)?;
        sh.store.add_job(&sh.run_id, &job)?;
        q.items.push_back(job);
        drop(q);
        sh.wake.notify_one();
        Ok(true)
    }

    /// "Ejecutar ahora": start a queued table at once, without waiting for a
    /// slot. `false`: it isn't queued.
    pub fn run_now(&self, name: &str) -> bool {
        let sh = &self.shared;
        let mut q = lock(&sh.queue);
        let Some(i) = q.items.iter().position(|j| j.name == name) else {
            return false;
        };
        if let Some(job) = q.items.remove(i) {
            q.boost.push_back(job);
        }
        drop(q);
        sh.wake.notify_one();
        true
    }
}

/// What the dispatcher does next.
enum Next {
    /// "Ejecutar ahora": start it without a slot.
    Boosted(Box<TransferJob>),
    /// The queue's head waits for a slot (it stays queued meanwhile).
    Queued,
    /// Nothing queued, nothing running: the run ends.
    Closed,
    /// Nothing queued: wait for a worker or a change.
    Idle,
}

/// Takes the table out of the in-flight set when its worker ends.
struct InFlight {
    shared: Arc<Shared>,
    name: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&self.shared.running).remove(&self.name);
    }
}

/// One table, start to end.
async fn worker(sh: Arc<Shared>, env: Arc<Env>, mut job: TransferJob, slot: Option<Slot>) -> Outcome {
    let name = job.name.clone();
    // At most one worker per table, checked before emptying or touching
    // its state.
    let ctl = {
        let mut running = lock(&sh.running);
        if running.contains_key(&name) {
            drop(running);
            env.log(LogLevel::Warn, format!("{name}: ya se está copiando"));
            return Outcome::Skipped;
        }
        let ctl = Arc::new(TableCtl::default());
        running.insert(name.clone(), ctl.clone());
        ctl
    };
    let _in_flight = InFlight { shared: sh.clone(), name: name.clone() };
    if sh.stop.load(Ordering::SeqCst) {
        return Outcome::Skipped;
    }
    match sh.store.table(&sh.run_id, &name) {
        Ok(Some(t)) if t.status == TableStatus::Done => return Outcome::Skipped,
        Ok(_) => {}
        Err(e) => return fail(&sh, &env, &name, &e),
    }
    env.options.apply(&mut job.target);

    let mut work = Box::pin(AssertUnwindSafe(attempts(&sh, &env, &job, &ctl)).catch_unwind());
    let result = tokio::select! {
        r = &mut work => Some(r),
        _ = ctl.wait_cancel() => None,
    };
    match result {
        Some(Ok(Ok(stats))) => {
            if let Err(e) = sh.store.finish_table(&sh.run_id, &name, TableStatus::Done, None, Some(&stats)) {
                return fail(&sh, &env, &name, &e);
            }
            for note in stats.delta.iter().flat_map(|d| d.notes.iter()) {
                env.emit(Event::Log { level: crate::event::LogLevel::Warn, text: format!("{name}: {note}") });
            }
            env.emit(Event::TableDone { table: name, rows: stats.rows, stats });
            Outcome::Done
        }
        Some(Ok(Err(e))) => fail(&sh, &env, &name, &e),
        Some(Err(panic)) => fail(&sh, &env, &name, &Error::State(format!("error inesperado: {}", panic_text(&*panic)))),
        None => {
            // Stop the statements in flight, close the connections, free
            // the slot, then clean up.
            ctl.interrupt();
            drop(work);
            drop(slot);
            cancelled(&sh, &env, &job).await;
            Outcome::Cancelled
        }
    }
}

fn fail(sh: &Shared, env: &Env, name: &str, e: &Error) -> Outcome {
    let text = e.to_string();
    if let Err(se) = sh.store.finish_table(&sh.run_id, name, TableStatus::Failed, Some(&text), None) {
        env.log(LogLevel::Error, format!("{name}: {se}"));
    }
    env.emit(Event::TableFailed { table: name.to_string(), error: text });
    Outcome::Failed
}

/// After a cancel: a copied table keeps its rows; any other one that may
/// hold rows of this run is emptied (best effort).
async fn cancelled(sh: &Shared, env: &Env, job: &TransferJob) {
    let name = &job.name;
    let st = sh.store.table(&sh.run_id, name).ok().flatten().unwrap_or_default();
    if st.copied {
        env.log(LogLevel::Info, format!("{name}: cancelada; sus filas ya estaban copiadas y se conservan"));
    } else if st.delta || job.mode != TransferMode::Copy {
        env.log(LogLevel::Info, format!("{name}: cancelada; la sincronización se deshizo y la tabla queda como estaba"));
    } else if st.attempts > 0 {
        match job.truncate.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(sql) => match env.endpoints.open_target().await {
                Ok(mut t) => match exec(&mut *t, sql).await {
                    Ok(()) => {
                        let _ = sh.store.set_rows_done(&sh.run_id, name, 0);
                    }
                    Err(e) => env.log(LogLevel::Warn, format!("{name}: no se pudo vaciar al cancelar: {e}")),
                },
                Err(e) => env.log(LogLevel::Warn, format!("{name}: no se pudo vaciar al cancelar: {e}")),
            },
            None => env.log(LogLevel::Warn, format!("{name}: cancelada a medias y sin forma de vaciarla")),
        }
    }
    let _ = sh.store.finish_table(&sh.run_id, name, TableStatus::Cancelled, None, None);
    env.emit(Event::TableCancelled { table: name.clone() });
}

/// Attempts at a table, retrying transient errors.
async fn attempts(sh: &Shared, env: &Env, job: &TransferJob, ctl: &TableCtl) -> Result<CopyStats> {
    let mut retry = 0u32;
    loop {
        match attempt(sh, env, job, ctl).await {
            Ok(stats) => return Ok(stats),
            Err(e) if retry < env.options.max_retries && is_transient(&e) => {
                retry += 1;
                let wait = backoff(env.options.backoff_ms, retry);
                env.log(
                    LogLevel::Warn,
                    format!("{}: {e}. Reintento {retry} de {} en {:.1} s", job.name, env.options.max_retries, wait.as_secs_f64()),
                );
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// One attempt, by the table's state.
async fn attempt(sh: &Shared, env: &Env, job: &TransferJob, ctl: &TableCtl) -> Result<CopyStats> {
    let (run, name) = (&sh.run_id, &job.name);
    let st = sh.store.table(run, name)?.unwrap_or_default();
    sh.store.set_status(run, name, TableStatus::Running)?;
    env.emit(Event::TableStarted { table: name.clone(), attempt: st.attempts + 1 });
    lock(&ctl.interrupters).clear();
    let mut tgt = env.endpoints.open_target().await?;
    ctl.add_interrupter(tgt.interrupter());

    let stats = if st.copied {
        env.log(LogLevel::Info, format!("{name}: ya estaba copiada; solo faltan sus índices"));
        CopyStats::already(st.rows_done)
    } else if let TransferMode::Delta { key, depth, max_cores } = &job.mode {
        delta::check_support(&*env.source_driver, &*env.target_driver)?;
        check_columns(env, job, &mut *tgt).await?;
        // Flagged before anything is written: never emptied from here on.
        sh.store.begin_delta(run, name)?;
        let src = env.endpoints.open_source().await?;
        ctl.add_interrupter(src.interrupter());
        let tracker = Tracker::new(sh, env, name, None);
        let progress = |rows: u64| tracker.update(rows);
        let total = |rows: u64| tracker.set_total(rows);
        let phase = |p: Phase| env.phase(name, p);
        let log = |level: LogLevel, text: String| env.log(level, text);
        let input = DeltaInput {
            job,
            key,
            depth: *depth,
            max_cores: *max_cores,
            source_driver: &*env.source_driver,
            target_driver: &*env.target_driver,
            phase: &phase,
            total: &total,
            progress: &progress,
            log: &log,
        };
        let stats = delta::sync_table(input, src, &mut *tgt).await?;
        sh.store.set_copied(run, name, stats.rows)?;
        tracker.publish(stats.rows);
        stats
    } else {
        check_columns(env, job, &mut *tgt).await?;
        if st.delta {
            return Err(Error::State("la tabla se sincronizó por filas; no se vacía para copiarla entera".into()));
        }
        // A copy started before (this run or an earlier one) may have left
        // rows: the table is all or nothing, so it starts over.
        let mut may_hold_rows = st.attempts > 0 || st.rows_done > 0;
        // A table this run created that already holds rows can only hold
        // ours: the state that said so was lost (a power cut can drop its
        // last writes). Never copy on top of them.
        if !may_hold_rows && !job.empty_first && !job.preexisting {
            may_hold_rows = has_rows(&mut *tgt, &job.target.table).await.unwrap_or_else(|e| {
                tracing::warn!("{name}: no se pudo ver si ya tiene filas: {e}");
                false
            });
        }
        if may_hold_rows || job.empty_first {
            let sql = job.truncate.as_deref().filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                Error::State(if may_hold_rows { "quedó a medias y no hay cómo vaciarla" } else { "no hay cómo vaciarla" }.into())
            })?;
            env.phase(name, Phase::Truncate);
            exec(&mut *tgt, sql).await?;
            sh.store.set_rows_done(run, name, 0)?;
        } else if job.preexisting && has_rows(&mut *tgt, &job.target.table).await? {
            return Err(Error::State("la tabla de destino ya tiene filas; para copiar encima, elegí «vaciar y copiar»".into()));
        }
        // From here on the table may hold rows of this run.
        sh.store.begin_copy(run, name)?;
        let src = env.endpoints.open_source().await?;
        ctl.add_interrupter(src.interrupter());
        env.phase(name, Phase::Copy);
        let tracker = Tracker::new(sh, env, name, job.row_estimate);
        let progress = |rows: u64| tracker.update(rows);
        let log = |level: LogLevel, text: String| env.log(level, text);
        let input = CopyInput {
            job,
            source_driver: &*env.source_driver,
            target_driver: &*env.target_driver,
            native: env.native,
            progress: &progress,
            log: &log,
            delta: None,
        };
        let stats = copy::copy_table(input, src, &mut *tgt).await?;
        sh.store.set_copied(run, name, stats.rows)?;
        tracker.publish(stats.rows);
        stats
    };

    if !job.post.is_empty() {
        env.phase(name, Phase::Indexes);
        for sql in &job.post {
            exec(&mut *tgt, sql).await.map_err(|e| match e {
                Error::Query(m) => Error::Query(format!("índices: {m}")),
                other => other,
            })?;
        }
    }
    Ok(stats)
}

/// Never load into a different structure: the target's columns against
/// the expected ones.
async fn check_columns(env: &Env, job: &TransferJob, tgt: &mut dyn Session) -> Result<()> {
    if job.expected_columns.is_empty() {
        return Ok(());
    }
    env.phase(&job.name, Phase::Check);
    let actual = tgt.columns(&job.target.table).await?;
    let diffs = column_differences(&job.expected_columns, &job.target.columns, &actual, env.same_engine);
    if !diffs.is_empty() {
        return Err(Error::State(format!("la tabla de destino no coincide: {}", diffs.join("; "))));
    }
    Ok(())
}

/// The target table has at least one row (its browse query, one row).
async fn has_rows(s: &mut dyn Session, table: &ObjectRef) -> Result<bool> {
    let q = s.browse_query(table, 1);
    let mut out = QueryOutcome::default();
    s.execute(&q, 1, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    Ok(out.results.iter().any(|r| r.total_rows > 0 || !r.rows.is_empty()))
}

/// A table's progress, throttled for the UI and the state.
struct Tracker<'a> {
    sh: &'a Shared,
    env: &'a Env,
    name: &'a str,
    total: Mutex<Option<u64>>,
    start: Instant,
    last: Mutex<Instant>,
    rows: AtomicU64,
}

impl<'a> Tracker<'a> {
    fn new(sh: &'a Shared, env: &'a Env, name: &'a str, total: Option<u64>) -> Self {
        let now = Instant::now();
        Tracker { sh, env, name, total: Mutex::new(total), start: now, last: Mutex::new(now), rows: AtomicU64::new(0) }
    }

    /// The rows to review, once known (sync by rows).
    fn set_total(&self, rows: u64) {
        *lock(&self.total) = Some(rows);
    }

    /// Committed rows so far.
    fn update(&self, rows: u64) {
        self.rows.store(rows, Ordering::Relaxed);
        {
            let mut last = lock(&self.last);
            if last.elapsed() < PROGRESS_EVERY {
                return;
            }
            *last = Instant::now();
        }
        if let Err(e) = self.sh.store.set_rows_done(&self.sh.run_id, self.name, rows) {
            tracing::warn!("{}: {e}", self.name);
        }
        self.emit(rows);
    }

    /// The final figure (the state already has it).
    fn publish(&self, rows: u64) {
        self.emit(rows);
    }

    fn emit(&self, rows: u64) {
        self.env.emit(Event::TableProgress {
            table: self.name.to_string(),
            rows_done: rows,
            // Never a total below what's done.
            rows_total: lock(&self.total).map(|t| t.max(rows)),
            rows_per_s: rate(rows, self.start.elapsed()),
        });
    }
}

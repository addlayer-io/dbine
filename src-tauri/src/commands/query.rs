use crate::error::CommandResult;
use crate::state::{AppState, SessionEntry};
use dbine_driver::sql::{self, ScriptMode, ScriptStatement, StatementKind};
use dbine_driver::{
    Driver, Error, Message, MessageLevel, MessageSinkRef, ProgressSinkRef, QueryOutcome, ScriptError, Session, StatementEnd, StatementResult, TxState,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

/// Rows kept per result set when the UI doesn't say.
const DEFAULT_MAX_ROWS: usize = 5_000;

#[derive(Deserialize)]
pub struct ExecuteArgs {
    /// The editor tab's session: its connection persists between runs.
    pub session_id: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub max_rows: Option<usize>,
    /// Saved query being run, to record when it last ran.
    pub query_id: Option<String>,
    /// The project file being run (its project and path), for its timeline.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub plan: PlanMode,
    /// Keep it in the history (what the user ran from the editor; not a
    /// table's data being browsed).
    #[serde(default)]
    pub record: bool,
    /// How the script runs (see [`RunMode`]). Every caller but the editor
    /// leaves it out: one call to the driver, as always (Users and
    /// permissions, Backups, the designer…).
    #[serde(default)]
    pub mode: RunMode,
    /// Go on after a failed statement; `None`: the engine's default
    /// ([`Driver::script_defaults`]). Only when the script runs statement
    /// by statement.
    #[serde(default)]
    pub continue_on_error: Option<bool>,
    /// The user confirmed running UPDATE / DELETE without WHERE.
    #[serde(default)]
    pub confirmed_unsafe: bool,
    /// The tab's transactions: `false` = manual (autocommit off), `true` =
    /// automatic. `None`: leave the session as it is.
    #[serde(default)]
    pub autocommit: Option<bool>,
}

/// How `execute_query` runs a script.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// One `execute` with the whole text, no checks: the driver splits it and
    /// stops at the first error (what every caller got before).
    #[default]
    Whole,
    /// As the driver says ([`Driver::script_mode`]): the editor. Also asks
    /// before UPDATE/DELETE without WHERE, reports each statement as it
    /// ends (`query-progress`), streams messages (`query-message`) and
    /// reports the transaction state.
    Auto,
    /// Statement by statement whatever the driver says.
    PerStatement,
    /// Batch by batch (the driver's dialect batches: T-SQL `GO [N]`).
    Batches,
}

/// What `execute_query` returns: the outcome, or what must be confirmed
/// before anything runs.
#[derive(Serialize, Debug)]
pub struct ExecuteResponse {
    #[serde(flatten)]
    pub outcome: QueryOutcome,
    /// Nothing ran: these UPDATE / DELETE have no WHERE. Run again with
    /// `confirmed_unsafe` once the user agrees.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub needs_confirmation: Vec<UnsafeDml>,
    /// A cancel closed the tab's session: its open transaction (manual
    /// mode) was rolled back by the server, and the next run opens a new one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_closed: bool,
}

/// An UPDATE / DELETE without WHERE. Offsets are JS string indices
/// (UTF-16) into the `sql` sent.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct UnsafeDml {
    pub keyword: String,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

/// `query-progress`: a statement of an editor run ended.
#[derive(Serialize, Clone, Debug)]
pub struct QueryProgress {
    pub session_id: String,
    /// Index among [`split_script`]'s units, or among the driver's own
    /// statements when it splits the script itself (`Whole`).
    pub statement: usize,
    /// `0`: unknown (the driver splits the script itself).
    pub total: usize,
    /// The statement in the `sql` sent (UTF-16 indices) and its line.
    pub start: usize,
    pub end: usize,
    pub line: u32,
    /// 1-based run of a `GO N` batch, and N.
    pub iteration: u32,
    pub repeat: u32,
    pub elapsed_ms: u64,
    /// What this statement added to the outcome.
    pub results: Vec<StatementResult>,
    pub log: Vec<Message>,
    pub errors: Vec<ScriptError>,
}

/// `query-message`: a message while a statement runs.
#[derive(Serialize, Clone, Debug)]
pub struct QueryMessage {
    pub session_id: String,
    pub message: Message,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// Just run it.
    #[default]
    None,
    /// The estimated plan; nothing runs.
    Estimated,
    /// Run it and bring the actual plan with the results.
    Actual,
}

/// Run a script. Server errors don't fail the call: they come back in
/// `error` / `errors` next to whatever ran. Connection problems do fail it.
#[tauri::command(rename_all = "camelCase")]
pub async fn execute_query(app: AppHandle, state: State<'_, AppState>, args: ExecuteArgs) -> CommandResult<ExecuteResponse> {
    let driver = state.store.get_connection(&args.connection_id).ok().flatten().and_then(|c| dbine_drivers::find(&c.config.driver));
    let units = match (args.plan, driver) {
        (PlanMode::None, Some(d)) => script_units(args.mode, d.as_ref(), &args.sql),
        _ => None,
    };
    let editor = args.mode != RunMode::Whole;
    let pos = Utf16::new(&args.sql);
    // The actual plan runs the script too (EXPLAIN ANALYZE, SET STATISTICS
    // XML…): only the estimated one is exempt.
    if let (Some(d), false, true) = (driver, args.confirmed_unsafe, args.plan != PlanMode::Estimated) {
        let found = if editor { unsafe_dml(d.as_ref(), &args.sql) } else { Vec::new() };
        if !found.is_empty() {
            let needs_confirmation =
                found.into_iter().map(|u| UnsafeDml { keyword: u.keyword, start: pos.at(u.start), end: pos.at(u.end), line: u.line }).collect();
            return Ok(ExecuteResponse { outcome: QueryOutcome::default(), needs_confirmation, session_closed: false });
        }
    }
    let entry = state.session(&args.session_id, &args.connection_id, &args.database).await?;
    if let Some(id) = &args.query_id {
        if let Err(e) = state.store.mark_query_run(id) {
            tracing::warn!(%e, "could not record query run");
        }
        // A run is a moment worth keeping in the query's timeline.
        if args.record {
            if let Ok(Some(q)) = state.store.get_query(id) {
                if let Err(e) = state.store.add_query_version(id, &q.sql, &chrono::Utc::now().to_rfc3339(), None) {
                    tracing::warn!(%e, "could not record the query's version");
                }
            }
        }
    }
    let started = std::time::Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();
    let max_rows = args.max_rows.unwrap_or(DEFAULT_MAX_ROWS).max(1);
    let mut out = QueryOutcome::default();
    let mut session = entry.session.lock().await;
    // A cancel left over from an earlier run (the session outlives it now).
    entry.cancelled.store(false, Ordering::SeqCst);
    if let Some(on) = args.autocommit {
        if entry.autocommit.load(Ordering::SeqCst) != on {
            match session.set_autocommit(on).await {
                Ok(()) => entry.autocommit.store(on, Ordering::SeqCst),
                Err(e) => {
                    // Nothing runs: in manual mode the user expects to commit
                    // it themselves.
                    out.push_error(e.to_script_error());
                    out.elapsed_ms = started.elapsed().as_millis() as u64;
                    return Ok(ExecuteResponse { outcome: out, needs_confirmation: Vec::new(), session_closed: false });
                }
            }
        }
    }
    let shared_units = units.map(Arc::new);
    if let Some(units) = &shared_units {
        // Messages reach the UI as they arrive, with lines of the script.
        let (app, id, units) = (app.clone(), args.session_id.clone(), units.clone());
        out.message_sink = Some(MessageSinkRef(Arc::new(move |m: &Message| {
            let mut message = m.clone();
            rebase_line(&mut message.line, m.statement.and_then(|i| units.get(i)));
            let _ = app.emit("query-message", QueryMessage { session_id: id.clone(), message });
        })));
    }
    let continue_on_error = args.continue_on_error.unwrap_or_else(|| driver.is_some_and(|d| d.script_defaults().continue_on_error));
    if editor && args.plan == PlanMode::None && shared_units.is_none() {
        // The driver splits the script itself (`Whole`): it reports each
        // statement as it ends and its messages as they arrive (lines of
        // the text it got: the script), and goes on after a failure when
        // the run does. Every other caller leaves these unset: one call
        // that stops at the first error, as always.
        out.continue_on_error = Some(continue_on_error);
        let (app2, id) = (app.clone(), args.session_id.clone());
        out.message_sink = Some(MessageSinkRef(Arc::new(move |m: &Message| {
            let _ = app2.emit("query-message", QueryMessage { session_id: id.clone(), message: m.clone() });
        })));
        let (app2, id, sql) = (app.clone(), args.session_id.clone(), Arc::new(args.sql.clone()));
        out.progress_sink = Some(ProgressSinkRef(Arc::new(move |p: &StatementEnd| {
            let pos = Utf16::new(&sql);
            let at = pos.at(p.offset);
            let _ = app2.emit(
                "query-progress",
                QueryProgress {
                    session_id: id.clone(),
                    statement: p.statement,
                    total: 0,
                    start: at,
                    end: at,
                    line: p.line,
                    iteration: 1,
                    repeat: 1,
                    elapsed_ms: p.elapsed_ms,
                    results: p.results.iter().cloned().map(|mut r| {
                        r.offset = r.offset.map(|o| pos.at(o));
                        r
                    }).collect(),
                    log: p.log.clone(),
                    errors: p.errors.iter().cloned().map(|mut e| {
                        e.offset = e.offset.map(|o| pos.at(o));
                        e
                    }).collect(),
                },
            );
        })));
    }
    let progress = |p: Progress<'_>| {
        let _ = app.emit(
            "query-progress",
            QueryProgress {
                session_id: args.session_id.clone(),
                statement: p.statement,
                total: p.total,
                start: pos.at(p.unit.start),
                end: pos.at(p.unit.end),
                line: p.unit.line,
                iteration: p.iteration,
                repeat: p.unit.repeat.max(1),
                elapsed_ms: p.elapsed_ms,
                results: p.results.iter().cloned().map(|mut r| {
                    r.offset = r.offset.map(|o| pos.at(o));
                    r
                }).collect(),
                log: p.log.to_vec(),
                errors: p.errors.iter().cloned().map(|mut e| {
                    e.offset = e.offset.map(|o| pos.at(o));
                    e
                }).collect(),
            },
        );
    };
    let run = async {
        match (args.plan, &shared_units) {
            (PlanMode::None, Some(units)) => {
                let script = ScriptRun { units, continue_on_error, max_rows, cancelled: &entry.cancelled, progress: &progress };
                run_script(&mut **session, script, &mut out).await
            }
            (PlanMode::None, None) => session.execute(&args.sql, max_rows, &mut out).await,
            (PlanMode::Estimated, _) => session.explain(&args.sql, false, max_rows, &mut out).await,
            (PlanMode::Actual, _) => session.explain(&args.sql, true, max_rows, &mut out).await,
        }
    };
    let registered = || state.sessions.get(&args.session_id).is_some_and(|e| Arc::ptr_eq(&e, &entry));
    let finished = run_cancellable(run, &entry, CANCEL_GRACE, registered).await;
    let usable = matches!(finished, Run::Finished(_) | Run::Cancelled { keep: true, .. });
    if editor && usable {
        out.transaction = session.transaction_state().await.unwrap_or_else(|e| {
            tracing::debug!(%e, "transaction state unknown");
            None
        });
    }
    drop(session);
    out.message_sink = None;
    out.progress_sink = None;
    // A statement switched the database (`USE`): the tab follows it on this
    // same session (see `AppState::session`).
    if let Some(db) = out.database.as_deref().filter(|d| !d.is_empty()) {
        if usable {
            *entry.switched_to.lock().unwrap_or_else(|p| p.into_inner()) = (db != entry.database).then(|| db.to_string());
        } else {
            out.database = None;
        }
    }
    let mut session_closed = false;
    match finished {
        Run::Finished(Ok(())) => {}
        // The statement loop already recorded its errors.
        Run::Finished(Err(e)) if shared_units.is_some() && !matches!(e, Error::Cancelled) => {}
        // The driver recorded them itself (several per call: SQL Server
        // batches) and returned one to say it failed.
        Run::Finished(Err(e)) if recorded(&out.errors, &e) => {}
        Run::Finished(Err(e)) => {
            out.error = None;
            out.push_error(e.to_script_error());
        }
        Run::Cancelled { keep, completed } => {
            let driver = state.store.get_connection(&args.connection_id).ok().flatten().map(|c| c.config.driver);
            if !keep || cancel_ends_session(driver.as_deref()) {
                // The statement was dropped mid-flight, the server closed the
                // connection, or the driver's interrupter ends the session
                // (Babelfish's KILL): its state is unknown or gone, so the
                // next run opens a new one.
                forget_session(&state, &args.session_id, &entry);
                session_closed = true;
                // A new session has no transaction; the UI tells the user
                // when one held work (`session_closed`).
                out.transaction = None;
            }
            // Finished before the cancel reached it: its results stand.
            if !completed {
                out.error = Some("Ejecución cancelada.".into());
                out.message(Message { level: MessageLevel::Error, text: "Ejecución cancelada.".into(), ..Default::default() });
            }
        }
    }
    out.current_statement = None;
    // Offsets in the outcome become indices of the JS string the UI sent.
    for r in &mut out.results {
        r.offset = r.offset.map(|o| pos.at(o));
    }
    for e in &mut out.errors {
        e.offset = e.offset.map(|o| pos.at(o));
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    if args.record {
        let origin = crate::commands::history::Origin { query_id: args.query_id.clone(), project_id: args.project_id.clone(), file_path: args.file_path.clone() };
        crate::commands::history::record(&state, &args.connection_id, &args.database, &args.sql, started_at, &out, origin);
    }
    Ok(ExecuteResponse { outcome: out, needs_confirmation: Vec::new(), session_closed })
}

/// The units an editor run goes through one by one, or `None` to hand the
/// driver the whole script (`Whole`, or `Auto` on a driver that wants it).
fn script_units(mode: RunMode, driver: &dyn Driver, sql: &str) -> Option<Vec<ScriptStatement>> {
    let per = match mode {
        RunMode::Whole => false,
        RunMode::Auto => driver.script_mode() != ScriptMode::Whole,
        RunMode::PerStatement | RunMode::Batches => true,
    };
    per.then(|| driver.split_script(sql))
}

/// The UPDATE / DELETE without WHERE of `sql`, on engines that check them.
fn unsafe_dml(driver: &dyn Driver, sql: &str) -> Vec<sql::UnsafeStatement> {
    if !driver.script_defaults().confirm_unsafe_dml {
        return Vec::new();
    }
    let dialect = driver.script_dialect();
    if !sqlplus_units(&dialect) {
        return sql::unsafe_statements(sql, &dialect);
    }
    // Only its SQL statements: words of a PROMPT or REM line aren't DML.
    driver
        .split_script(sql)
        .into_iter()
        .filter(|u| u.kind == StatementKind::Sql)
        .flat_map(|u| {
            sql::unsafe_statements(&u.text, &dialect).into_iter().map(move |mut x| {
                x.start += u.start;
                x.end += u.start;
                x.line += u.line - 1;
                x
            })
        })
        .collect()
}

/// SQL*Plus scripts (`/` lines): they have no batches, so the driver's own
/// units are its statements, and only its splitter reads SQL*Plus command
/// lines (`PROMPT`, `EXEC`…) as units of their own.
fn sqlplus_units(dialect: &sql::ScriptDialect) -> bool {
    dialect.batch == sql::BatchLine::Slash
}

/// Byte offsets of a script as JS string (UTF-16) indices. For non-ASCII
/// text it keeps the UTF-16 index of every `STEP`-th byte, so each lookup
/// encodes at most `STEP` bytes, whatever the script's size.
pub(crate) struct Utf16<'a> {
    s: &'a str,
    /// `marks[k]`: UTF-16 length of `s[..floor(k * STEP)]` (floored to a
    /// char boundary). Empty for ASCII text.
    marks: Vec<usize>,
}

impl<'a> Utf16<'a> {
    const STEP: usize = 256;

    pub(crate) fn new(s: &'a str) -> Self {
        let mut marks = Vec::new();
        if !s.is_ascii() {
            let (mut at, mut units) = (0, 0);
            for k in 0..=s.len() / Self::STEP {
                let b = Self::floor(s, k * Self::STEP);
                units += s[at..b].encode_utf16().count();
                at = b;
                marks.push(units);
            }
        }
        Self { s, marks }
    }

    fn floor(s: &str, byte: usize) -> usize {
        let mut b = byte.min(s.len());
        while !s.is_char_boundary(b) {
            b -= 1;
        }
        b
    }

    pub(crate) fn at(&self, byte: usize) -> usize {
        let b = Self::floor(self.s, byte);
        if self.marks.is_empty() {
            return b;
        }
        let k = b / Self::STEP;
        let from = Self::floor(self.s, k * Self::STEP);
        self.marks[k] + self.s[from..b].encode_utf16().count()
    }
}

/// A line relative to a unit's text, made a line of the script.
fn rebase_line(line: &mut Option<u32>, unit: Option<&ScriptStatement>) {
    if let (Some(l), Some(u)) = (line.as_mut(), unit) {
        *l = u.line + l.saturating_sub(1);
    }
}

/// A unit's error as the driver placed it (offset and line relative to the
/// unit's text), with a line always: from its offset, or the unit's first.
fn unit_error(e: &Error, unit: &ScriptStatement) -> ScriptError {
    let mut se = e.to_script_error();
    if se.line.is_none() {
        se.line = Some(se.offset.map_or(1, |o| line_of(&unit.text, o)));
    }
    se
}

/// An editor script run unit by unit.
struct ScriptRun<'a> {
    units: &'a [ScriptStatement],
    continue_on_error: bool,
    max_rows: usize,
    /// `cancel_query` was called: no further statement starts.
    cancelled: &'a AtomicBool,
    progress: &'a (dyn Fn(Progress<'_>) + Send + Sync),
}

/// A unit (or one run of a `GO N` batch) ended.
struct Progress<'a> {
    statement: usize,
    total: usize,
    unit: &'a ScriptStatement,
    iteration: u32,
    elapsed_ms: u64,
    results: &'a [StatementResult],
    log: &'a [Message],
    errors: &'a [ScriptError],
}

/// Run `run.units` one by one on `session`, recording every failure in
/// `out` (with its statement, offset and line in the script). After a
/// failure it goes on only with `continue_on_error`; it always stops on a
/// cancel, a lost connection or an error the driver marks fatal, and then
/// returns that error (already recorded, except `Cancelled`).
async fn run_script(session: &mut dyn Session, run: ScriptRun<'_>, out: &mut QueryOutcome) -> dbine_driver::Result<()> {
    let total = run.units.len();
    // A client-side error (`GO 99999999999`): nothing runs, as in SSMS.
    if let Some((idx, unit, msg)) = run.units.iter().enumerate().find_map(|(i, u)| u.error.as_ref().map(|m| (i, u, m))) {
        let mut e = ScriptError::new(msg.clone()).fatal();
        e.statement = Some(idx);
        e.offset = Some(unit.start);
        e.line = Some(unit.line);
        out.push_error(e.clone());
        return Err(e.into());
    }
    for (idx, unit) in run.units.iter().enumerate() {
        if unit.kind == StatementKind::ClientCommand {
            continue;
        }
        if run.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        out.current_statement = Some(idx);
        let repeat = unit.repeat.max(1);
        if repeat > 1 {
            out.info("Inicio del ciclo de ejecución");
        }
        let mut done = 0u32;
        let mut failed = None;
        for iteration in 1..=repeat {
            if iteration > 1 && run.cancelled.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let (r0, l0, e0) = (out.results.len(), out.log.len(), out.errors.len());
            let started = std::time::Instant::now();
            let r = session.execute(&unit.text, run.max_rows, out).await;
            out.adopt_plain_messages();
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // Errors go in as the driver placed them (the live sink moves
            // their line once); everything is moved to the script below.
            let stop = match r {
                Ok(()) => {
                    done += 1;
                    false
                }
                Err(Error::Cancelled) => true,
                Err(e) => {
                    // A cancel that arrived mid-statement: the server's own
                    // words, and the script stops. A driver that recorded
                    // the statement's errors itself (`out.push_error`, several
                    // per batch on SQL Server) returns one of them to say it
                    // failed: not recorded twice.
                    if !recorded(&out.errors[e0..], &e) {
                        out.push_error(unit_error(&e, unit));
                    }
                    let cancelled = run.cancelled.load(Ordering::SeqCst);
                    if !cancelled {
                        // It ran (and failed): sqlcmd counts it and goes on
                        // with the next repetition. The first error is kept,
                        // unless a later one ends the script.
                        done += 1;
                        if failed.is_none() || e.ends_script() {
                            failed = Some(e);
                        }
                    }
                    cancelled
                }
            };
            for res in &mut out.results[r0..] {
                res.statement = Some(idx);
                res.offset = Some(unit.start);
                res.line = Some(unit.line);
                res.elapsed_ms.get_or_insert(elapsed_ms);
            }
            for m in &mut out.log[l0..] {
                rebase_line(&mut m.line, Some(unit));
            }
            for e in &mut out.errors[e0..] {
                e.statement = Some(idx);
                e.offset = e.offset.map(|o| unit.start + o.min(unit.text.len()));
                rebase_line(&mut e.line, Some(unit));
            }
            if stop {
                return Err(Error::Cancelled);
            }
            (run.progress)(Progress {
                statement: idx,
                total,
                unit,
                iteration,
                elapsed_ms,
                results: &out.results[r0..],
                log: &out.log[l0..],
                errors: &out.errors[e0..],
            });
            // Only an error that ends the script stops `GO N` (as in sqlcmd).
            if failed.as_ref().is_some_and(|e| e.ends_script()) {
                break;
            }
        }
        if repeat > 1 {
            out.info(format!("Lote ejecutado {done} veces."));
        }
        if let Some(e) = failed {
            if e.ends_script() {
                out.current_statement = None;
                return Err(e);
            }
            if !run.continue_on_error {
                break;
            }
        }
    }
    out.current_statement = None;
    Ok(())
}

/// A script run with no editor behind it (scheduled tasks): split as the
/// editor's Auto mode does, on a session of its own under `key` (so
/// `cancel_query` stops it), committing each statement whatever the
/// connection's autocommit option says. Failures end up in `out.errors`;
/// the `Err` is for what kept it from starting (connecting, a password).
pub(crate) async fn run_unattended(
    state: &AppState,
    key: &str,
    connection_id: &str,
    database: &str,
    sql: &str,
    continue_on_error: Option<bool>,
    max_rows: usize,
) -> CommandResult<QueryOutcome> {
    let driver = crate::commands::schema::driver_of(state, connection_id)?;
    let units = script_units(RunMode::Auto, driver.as_ref(), sql);
    let continue_on_error = continue_on_error.unwrap_or_else(|| driver.script_defaults().continue_on_error);
    let entry = state.dedicated_session(key, connection_id, database, false).await?;
    let mut out = QueryOutcome::default();
    let finished = {
        let mut session = entry.session.lock().await;
        let ready = if entry.autocommit.load(Ordering::SeqCst) { Ok(()) } else { session.set_autocommit(true).await };
        let run = async {
            ready?;
            match &units {
                Some(units) => {
                    let script = ScriptRun { units, continue_on_error, max_rows, cancelled: &entry.cancelled, progress: &|_| {} };
                    run_script(&mut **session, script, &mut out).await
                }
                None => {
                    out.continue_on_error = Some(continue_on_error);
                    session.execute(sql, max_rows, &mut out).await
                }
            }
        };
        tokio::select! {
            r = run => Some(r),
            _ = entry.cancel.notified() => None,
        }
    };
    state.sessions.remove(key);
    match finished {
        Some(Ok(())) => {}
        Some(Err(e)) if units.is_some() && !matches!(e, Error::Cancelled) => {}
        Some(Err(e)) if recorded(&out.errors, &e) => {}
        Some(Err(e)) => out.push_error(e.to_script_error()),
        None => out.push_error(ScriptError::new("Ejecución cancelada.")),
    }
    out.current_statement = None;
    Ok(out)
}

/// The driver already put this error in the outcome (`out.push_error`) and
/// returned it only to say the call failed.
fn recorded(errors: &[ScriptError], e: &Error) -> bool {
    !matches!(e, Error::Cancelled) && errors.iter().any(|x| x.message == e.to_string())
}

/// 1-based line of a byte offset.
fn line_of(script: &str, offset: usize) -> u32 {
    let end = offset.min(script.len());
    script.as_bytes()[..end].iter().filter(|&&c| c == b'\n').count() as u32 + 1
}

fn forget_session(state: &AppState, key: &str, entry: &Arc<SessionEntry>) {
    state.sessions.remove_if(key, |_, e| Arc::ptr_eq(e, entry));
}

/// How long a cancelled statement gets to stop once the driver's interrupter
/// fired (TDS attention, PostgreSQL cancel request, KILL QUERY…) before its
/// connection is given up.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum Run {
    /// It ended on its own; nobody cancelled it.
    Finished(dbine_driver::Result<()>),
    /// It was cancelled. `keep`: the session is still usable (the server
    /// stopped the statement and the connection lives on, with its SET
    /// options, temporary tables and open transaction). `completed`: it had
    /// already finished without error when the cancel got there.
    Cancelled { keep: bool, completed: bool },
}

/// Run a statement on `entry`'s session until it ends or is cancelled.
///
/// On `cancel_query` (which fires the interrupter first), the statement is
/// awaited up to `grace` more: a driver that stops it on the server returns,
/// and the connection is kept, as psql or SSMS do. It is dropped instead, and
/// the session with it, when there's no interrupter (dropping is the only
/// way to stop it), when it doesn't return in time, when the driver reports
/// the connection lost, or when the wake-up came from closing the session
/// (`registered` false) rather than from a cancel.
async fn run_cancellable<F>(run: F, entry: &SessionEntry, grace: Duration, registered: impl Fn() -> bool) -> Run
where
    F: Future<Output = dbine_driver::Result<()>>,
{
    let dropped = Run::Cancelled { keep: false, completed: false };
    tokio::pin!(run);
    tokio::select! {
        // The interrupter may stop it before the wake-up arrives.
        r = &mut run => return if entry.cancelled.load(Ordering::SeqCst) { after_cancel(r) } else { Run::Finished(r) },
        _ = entry.cancel.notified() => {}
    }
    if !entry.cancelled.load(Ordering::SeqCst) || entry.interrupter.is_none() || !registered() {
        return dropped;
    }
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            r = &mut run => return after_cancel(r),
            _ = &mut deadline => {
                tracing::warn!("the cancelled statement did not stop in {} s; dropping its connection", grace.as_secs());
                return dropped;
            }
            // Another cancel is ignored; a close ends the wait.
            _ = entry.cancel.notified() => {
                if !registered() {
                    return dropped;
                }
            }
        }
    }
}

/// A cancelled statement returned: its session is kept unless the driver
/// reports the connection gone with it.
fn after_cancel(r: dbine_driver::Result<()>) -> Run {
    match r {
        Ok(()) => Run::Cancelled { keep: true, completed: true },
        Err(dbine_driver::Error::Connect(_) | dbine_driver::Error::Io(_)) => Run::Cancelled { keep: false, completed: false },
        Err(_) => Run::Cancelled { keep: true, completed: false },
    }
}

/// Drivers whose interrupter stops the statement by ending the server session:
/// Babelfish's is a `KILL` of the session (verified as this one) from another
/// connection, since Babelfish reads the TDS attention only once the batch is
/// over. The driver still returns `Error::Cancelled`, but every later
/// statement fails on the dead connection with a plain query error, and the
/// #temp tables and SET options are gone anyway: the session is always
/// dropped after a cancel. Oracle's is an `ALTER SYSTEM KILL SESSION` (its
/// thin client has no break): the driver reconnects, but the transaction
/// and session state are gone, so the session is dropped and the UI told.
/// SQL Server, Azure SQL and Fabric send the attention, which keeps the
/// session. An unknown driver (the connection couldn't be read) is dropped
/// too, as before.
const CANCEL_ENDS_SESSION: &[&str] = &["babelfish", "oracle", "oracle_adb"];

fn cancel_ends_session(driver: Option<&str>) -> bool {
    driver.is_none_or(|d| CANCEL_ENDS_SESSION.contains(&d))
}

/// Stop what runs on `entry`: the driver's interrupter first (it stops the
/// statement on the server), then the flag long jobs check between steps and
/// the wake-up for the statement's wait.
fn request_cancel(entry: &SessionEntry) {
    // Set first: a statement the interrupter stops at once reads as cancelled.
    entry.cancelled.store(true, Ordering::SeqCst);
    if let Some(interrupt) = &entry.interrupter {
        interrupt();
    }
    entry.cancel.notify_waiters();
}

#[derive(Deserialize)]
pub struct SessionArgs {
    pub session_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn cancel_query(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<()> {
    if let Some(entry) = state.sessions.get(&args.session_id).map(|e| e.clone()) {
        request_cancel(&entry);
    }
    Ok(())
}

/// The tab closed: release its connection.
#[tauri::command(rename_all = "camelCase")]
pub async fn close_session(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<()> {
    if let Some((_, entry)) = state.sessions.remove(&args.session_id) {
        entry.cancel.notify_waiters();
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct AutocommitArgs {
    pub session_id: String,
    pub connection_id: String,
    pub database: String,
    pub autocommit: bool,
}

/// The tab's Auto/Manual toggle: opens the tab's session if needed and
/// switches it. Returns the transaction state after the switch (a driver
/// may commit the open transaction when autocommit comes back on).
#[tauri::command(rename_all = "camelCase")]
pub async fn set_tab_autocommit(state: State<'_, AppState>, args: AutocommitArgs) -> CommandResult<Option<TxState>> {
    let entry = state.session(&args.session_id, &args.connection_id, &args.database).await?;
    let mut s = entry.session.lock().await;
    s.set_autocommit(args.autocommit).await?;
    entry.autocommit.store(args.autocommit, Ordering::SeqCst);
    Ok(s.transaction_state().await.unwrap_or(None))
}

/// Commit the tab's open transaction. `None` when the tab has no session.
#[tauri::command(rename_all = "camelCase")]
pub async fn commit_tab(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<Option<TxState>> {
    let Some(entry) = state.sessions.get(&args.session_id).map(|e| e.clone()) else { return Ok(None) };
    let mut s = entry.session.lock().await;
    s.commit().await?;
    Ok(s.transaction_state().await.unwrap_or(None))
}

/// Roll back the tab's open transaction. `None` when the tab has no session.
#[tauri::command(rename_all = "camelCase")]
pub async fn rollback_tab(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<Option<TxState>> {
    let Some(entry) = state.sessions.get(&args.session_id).map(|e| e.clone()) else { return Ok(None) };
    let mut s = entry.session.lock().await;
    s.rollback().await?;
    Ok(s.transaction_state().await.unwrap_or(None))
}

/// Whether the tab's session has a transaction open (before closing the tab
/// or switching database). `None`: no session, or the driver doesn't track
/// it.
#[tauri::command(rename_all = "camelCase")]
pub async fn tab_transaction_state(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<Option<TxState>> {
    let Some(entry) = state.sessions.get(&args.session_id).map(|e| e.clone()) else { return Ok(None) };
    let mut s = entry.session.lock().await;
    Ok(s.transaction_state().await?)
}

#[derive(Deserialize)]
pub struct SplitArgs {
    /// The engine: a saved connection's, or a driver id.
    pub connection_id: Option<String>,
    pub driver: Option<String>,
    pub sql: String,
    /// Statement by statement even inside batches (T-SQL): for "run the
    /// statement at the cursor". `false`: the units a run goes through.
    #[serde(default)]
    pub statements: bool,
}

/// A unit of a script for the UI: offsets are JS string (UTF-16) indices.
#[derive(Serialize, Debug, PartialEq)]
pub struct SplitStatement {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub kind: StatementKind,
    pub repeat: u32,
    /// A client-side error found while splitting (`GO 99999999999`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The script cut as the engine's tool would (see [`Driver::split_script`]).
#[tauri::command(rename_all = "camelCase")]
pub async fn split_script(state: State<'_, AppState>, args: SplitArgs) -> CommandResult<Vec<SplitStatement>> {
    let id = match (&args.driver, &args.connection_id) {
        (Some(d), _) => Some(d.clone()),
        (None, Some(c)) => state.store.get_connection(c)?.map(|c| c.config.driver),
        (None, None) => None,
    };
    let driver = id.as_deref().and_then(dbine_drivers::find);
    Ok(split_for_ui(driver.map(|d| d.as_ref()), &args.sql, args.statements))
}

fn split_for_ui(driver: Option<&dyn Driver>, sql: &str, statements: bool) -> Vec<SplitStatement> {
    let dialect = driver.map_or_else(sql::ScriptDialect::generic, |d| d.script_dialect());
    let units = match (driver, statements) {
        (Some(d), true) if sqlplus_units(&dialect) => d.split_script(sql),
        (_, true) => sql::split_script(sql, &dialect.statements()),
        (Some(d), false) => d.split_script(sql),
        (None, false) => sql::split_script(sql, &dialect),
    };
    let pos = Utf16::new(sql);
    units
        .into_iter()
        .map(|u| SplitStatement { start: pos.at(u.start), end: pos.at(u.end), line: u.line, kind: u.kind, repeat: u.repeat, text: u.text, error: u.error })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{async_trait, ColumnInfo, DbObject, ObjectRef};
    use tokio::sync::{Mutex, Notify};

    /// A session nothing is called on: the runs below are plain futures.
    struct Idle;

    #[async_trait]
    impl Session for Idle {
        async fn server_version(&mut self) -> dbine_driver::Result<String> {
            unimplemented!()
        }
        async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
            unimplemented!()
        }
        async fn list_objects(&mut self) -> dbine_driver::Result<Vec<DbObject>> {
            unimplemented!()
        }
        async fn columns(&mut self, _: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
            unimplemented!()
        }
        async fn definition(&mut self, _: &ObjectRef) -> dbine_driver::Result<Option<String>> {
            unimplemented!()
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            unimplemented!()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut QueryOutcome) -> dbine_driver::Result<()> {
            unimplemented!()
        }
    }

    fn entry(interrupter: Option<Arc<dyn Fn() + Send + Sync>>) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            connection_id: "c".into(),
            database: "d".into(),
            session: Mutex::new(Box::new(Idle)),
            cancel: Notify::new(),
            cancelled: Default::default(),
            interrupter,
            autocommit: AtomicBool::new(true),
            switched_to: Default::default(),
        })
    }

    /// An interrupter that wakes `stop`, as a server cancel does.
    fn signalling(stop: &Arc<Notify>) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let stop = stop.clone();
        Some(Arc::new(move || stop.notify_one()))
    }

    /// `cancel_query` once the run is waiting.
    fn cancel_soon(entry: &Arc<SessionEntry>) {
        let entry = entry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            request_cancel(&entry);
        });
    }

    const GRACE: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn uncancelled_run_finishes() {
        let e = entry(None);
        let r = run_cancellable(async { Ok(()) }, &e, GRACE, || true).await;
        assert!(matches!(r, Run::Finished(Ok(()))), "{r:?}");
    }

    #[tokio::test]
    async fn interrupted_statement_keeps_the_session() {
        let stop = Arc::new(Notify::new());
        let e = entry(signalling(&stop));
        cancel_soon(&e);
        let run = async {
            stop.notified().await;
            Err(Error::Query("canceling statement due to user request".into()))
        };
        let r = run_cancellable(run, &e, GRACE, || true).await;
        assert!(matches!(r, Run::Cancelled { keep: true, completed: false }), "{r:?}");
    }

    #[tokio::test]
    async fn statement_done_before_the_cancel_keeps_its_results() {
        let stop = Arc::new(Notify::new());
        let e = entry(signalling(&stop));
        cancel_soon(&e);
        let run = async {
            stop.notified().await;
            Ok(())
        };
        let r = run_cancellable(run, &e, GRACE, || true).await;
        assert!(matches!(r, Run::Cancelled { keep: true, completed: true }), "{r:?}");
    }

    #[tokio::test]
    async fn without_interrupter_the_session_is_dropped() {
        let e = entry(None);
        cancel_soon(&e);
        let r = run_cancellable(std::future::pending(), &e, GRACE, || true).await;
        assert!(matches!(r, Run::Cancelled { keep: false, completed: false }), "{r:?}");
    }

    #[tokio::test]
    async fn statement_that_ignores_the_interrupter_is_dropped_after_the_grace() {
        let e = entry(Some(Arc::new(|| {})));
        cancel_soon(&e);
        let started = std::time::Instant::now();
        let r = run_cancellable(std::future::pending(), &e, GRACE, || true).await;
        assert!(matches!(r, Run::Cancelled { keep: false, completed: false }), "{r:?}");
        assert!(started.elapsed() >= GRACE);
    }

    #[tokio::test]
    async fn lost_connection_drops_the_session() {
        // Oracle-like: the interrupter ends the server session.
        let stop = Arc::new(Notify::new());
        let e = entry(signalling(&stop));
        cancel_soon(&e);
        let run = async {
            stop.notified().await;
            Err(Error::Connect("connection closed".into()))
        };
        let r = run_cancellable(run, &e, GRACE, || true).await;
        assert!(matches!(r, Run::Cancelled { keep: false, completed: false }), "{r:?}");
    }

    #[tokio::test]
    async fn babelfish_kill_drops_the_session_despite_cancelled() {
        // Babelfish-like: the interrupter KILLs the session; the driver
        // reports Error::Cancelled, yet the connection is dead.
        let stop = Arc::new(Notify::new());
        let e = entry(signalling(&stop));
        cancel_soon(&e);
        let run = async {
            stop.notified().await;
            Err(Error::Cancelled)
        };
        let r = run_cancellable(run, &e, GRACE, || true).await;
        let Run::Cancelled { keep, completed: false } = r else { panic!("{r:?}") };
        assert!(keep, "the run itself looks recoverable");
        for id in CANCEL_ENDS_SESSION {
            assert!(cancel_ends_session(Some(id)), "{id}");
        }
    }

    #[test]
    fn engines_with_a_server_cancel_keep_the_session() {
        // SQL Server and its cloud variants: TDS attention, the session lives on.
        for id in ["postgres", "mysql", "clickhouse", "mongodb", "sqlserver", "azuresql", "fabric"] {
            assert!(!cancel_ends_session(Some(id)), "{id}");
        }
        assert!(cancel_ends_session(None));
    }

    #[test]
    fn session_ending_ids_are_real_drivers() {
        for id in CANCEL_ENDS_SESSION {
            assert!(dbine_drivers::find(id).is_some(), "{id}");
        }
        for id in ["postgres", "mysql", "oracle", "sqlserver", "azuresql", "fabric"] {
            assert!(dbine_drivers::find(id).is_some(), "{id}");
        }
    }

    /// A session that records what it runs: "boom" fails with details,
    /// "lost" loses the connection, "stop" presses Cancel while it runs.
    struct Scripted {
        ran: Vec<String>,
        cancel: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Session for Scripted {
        async fn server_version(&mut self) -> dbine_driver::Result<String> {
            unimplemented!()
        }
        async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
            unimplemented!()
        }
        async fn list_objects(&mut self) -> dbine_driver::Result<Vec<DbObject>> {
            unimplemented!()
        }
        async fn columns(&mut self, _: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
            unimplemented!()
        }
        async fn definition(&mut self, _: &ObjectRef) -> dbine_driver::Result<Option<String>> {
            unimplemented!()
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            unimplemented!()
        }
        async fn execute(&mut self, text: &str, _: usize, out: &mut QueryOutcome) -> dbine_driver::Result<()> {
            self.ran.push(text.to_string());
            out.messages.push(format!("ran {text}"));
            if let Some(at) = text.find("boom") {
                return Err(ScriptError::new("Incorrect syntax near 'boom'.").with_code("102").at_offset(at).into());
            }
            if text.contains("lost") {
                return Err(Error::Connect("connection reset".into()));
            }
            if let Some(at) = text.find("stop") {
                self.cancel.store(true, Ordering::SeqCst);
                return Err(ScriptError::new("canceling statement due to user request").at_offset(at).into());
            }
            out.push_affected(1);
            Ok(())
        }
    }

    struct Ran {
        result: dbine_driver::Result<()>,
        ran: Vec<String>,
        out: QueryOutcome,
        progress: Vec<(usize, u32, usize)>,
    }

    async fn run_units(sql: &str, dialect: sql::ScriptDialect, continue_on_error: bool) -> Ran {
        let units = sql::split_script(sql, &dialect);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = Scripted { ran: Vec::new(), cancel: cancel.clone() };
        let mut out = QueryOutcome::default();
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: Progress<'_>| seen.lock().unwrap().push((p.statement, p.iteration, p.errors.len()));
        let run = ScriptRun { units: &units, continue_on_error, max_rows: 10, cancelled: &cancel, progress: &progress };
        let result = run_script(&mut s, run, &mut out).await;
        Ran { result, ran: s.ran, out, progress: seen.into_inner().unwrap() }
    }

    #[tokio::test]
    async fn script_continues_after_an_error_when_asked() {
        let sql = "select 1;\nselect boom;\nselect 3";
        let r = run_units(sql, sql::ScriptDialect::generic(), true).await;
        assert!(r.result.is_ok());
        assert_eq!(r.ran, vec!["select 1", "select boom", "select 3"]);
        assert_eq!(r.out.results.iter().map(|x| x.statement).collect::<Vec<_>>(), vec![Some(0), Some(2)]);
        assert_eq!(r.out.results[1].line, Some(3));
        let e = &r.out.errors[0];
        assert_eq!((e.statement, e.code.as_deref(), e.line), (Some(1), Some("102"), Some(2)));
        assert_eq!(&sql[e.offset.unwrap()..e.offset.unwrap() + 4], "boom");
        assert_eq!(r.out.error.as_deref(), Some("Incorrect syntax near 'boom'."));
        // Messages and the error, in order, each with its statement.
        let log: Vec<_> = r.out.log.iter().map(|m| (m.level, m.statement)).collect();
        assert_eq!(
            log,
            vec![
                (MessageLevel::Info, Some(0)),
                (MessageLevel::Info, Some(1)),
                (MessageLevel::Error, Some(1)),
                (MessageLevel::Info, Some(2)),
            ]
        );
        assert_eq!(r.out.log[2].line, Some(2));
        assert_eq!(r.progress, vec![(0, 1, 0), (1, 1, 1), (2, 1, 0)]);
    }

    #[tokio::test]
    async fn script_stops_at_the_first_error_by_default() {
        let r = run_units("select 1; select boom; select 3", sql::ScriptDialect::generic(), false).await;
        assert!(r.result.is_ok());
        assert_eq!(r.ran, vec!["select 1", "select boom"]);
        assert_eq!(r.out.errors.len(), 1);
    }

    #[tokio::test]
    async fn a_lost_connection_ends_the_script_even_when_continuing() {
        let r = run_units("select 1; select lost; select 3", sql::ScriptDialect::generic(), true).await;
        assert!(matches!(r.result, Err(Error::Connect(_))));
        assert_eq!(r.ran, vec!["select 1", "select lost"]);
        assert!(r.out.errors[0].fatal);
    }

    #[tokio::test]
    async fn go_n_repeats_its_batch() {
        let r = run_units("insert x\nGO 3\nselect 1", sql::ScriptDialect::tsql(), false).await;
        assert_eq!(r.ran, vec!["insert x", "insert x", "insert x", "select 1"]);
        assert_eq!(r.out.results.len(), 4);
        assert!(r.out.messages.contains(&"Lote ejecutado 3 veces.".to_string()));
        assert_eq!(r.progress.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>(), vec![(0, 1), (0, 2), (0, 3), (1, 1)]);
        // A failing repetition doesn't end the repeats (sqlcmd runs all of
        // them and counts them); the script goes on after them.
        let r = run_units("select boom\nGO 5\nselect 2", sql::ScriptDialect::tsql(), true).await;
        assert_eq!(r.ran, vec!["select boom", "select boom", "select boom", "select boom", "select boom", "select 2"]);
        assert!(r.out.messages.contains(&"Lote ejecutado 5 veces.".to_string()), "{:?}", r.out.messages);
    }

    #[tokio::test]
    async fn an_invalid_go_count_runs_nothing() {
        let sql = "insert x\nGO 99999999999\nselect 1";
        let r = run_units(sql, sql::ScriptDialect::tsql(), true).await;
        assert!(matches!(r.result, Err(Error::Statement(_))));
        assert!(r.ran.is_empty(), "{:?}", r.ran);
        let e = &r.out.errors[0];
        assert_eq!((e.message.as_str(), e.statement, e.line, e.fatal), (sql::GO_COUNT_ERROR, Some(1), Some(2), true));
        assert_eq!(&sql[e.offset.unwrap()..e.offset.unwrap() + 2], "GO");
        assert_eq!(r.out.error.as_deref(), Some(sql::GO_COUNT_ERROR));
    }

    #[tokio::test]
    async fn cancel_stops_the_script() {
        let r = run_units("select 1; select stop; select 3", sql::ScriptDialect::generic(), true).await;
        assert!(matches!(r.result, Err(Error::Cancelled)));
        assert_eq!(r.ran, vec!["select 1", "select stop"]);
        assert_eq!(r.out.errors.len(), 1, "the server's cancel message is kept");
    }

    #[tokio::test]
    async fn a_cancel_mid_statement_is_placed_in_the_script() {
        let sql = "select 1;

select stop;";
        let r = run_units(sql, sql::ScriptDialect::generic(), false).await;
        let e = &r.out.errors[0];
        assert_eq!((e.statement, e.line), (Some(1), Some(3)));
        assert_eq!(&sql[e.offset.unwrap()..e.offset.unwrap() + 4], "stop");
        assert_eq!(r.out.log.last().map(|m| m.line), Some(Some(3)));
    }

    #[tokio::test]
    async fn live_messages_carry_script_lines_once() {
        let sql = "select 1;

select
 boom;";
        let units = Arc::new(sql::split_script(sql, &sql::ScriptDialect::generic()));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = QueryOutcome::default();
        let (u2, s2) = (units.clone(), seen.clone());
        out.message_sink = Some(MessageSinkRef(Arc::new(move |m: &Message| {
            let mut line = m.line;
            rebase_line(&mut line, m.statement.and_then(|i| u2.get(i)));
            s2.lock().unwrap().push((m.level, line));
        })));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = Scripted { ran: Vec::new(), cancel: cancel.clone() };
        let progress = |_: Progress<'_>| {};
        let run = ScriptRun { units: &units, continue_on_error: true, max_rows: 10, cancelled: &cancel, progress: &progress };
        run_script(&mut s, run, &mut out).await.unwrap();
        // The error is on line 4 of the script, live and in the outcome.
        let live: Vec<_> = seen.lock().unwrap().iter().filter(|(l, _)| *l == MessageLevel::Error).map(|x| x.1).collect();
        assert_eq!(live, vec![Some(4)]);
        assert_eq!(out.errors[0].line, Some(4));
        assert_eq!(out.log.iter().find(|m| m.level == MessageLevel::Error).unwrap().line, Some(4));
    }

    #[tokio::test]
    async fn client_commands_are_not_sent() {
        let r = run_units("DELIMITER //\nselect 1//\nDELIMITER ;\nselect 2;", sql::ScriptDialect::mysql(), false).await;
        assert_eq!(r.ran, vec!["select 1", "select 2"]);
        // Statement numbers follow split_script's, client commands included.
        assert_eq!(r.out.results.iter().map(|x| x.statement).collect::<Vec<_>>(), vec![Some(1), Some(3)]);
    }

    #[test]
    fn whole_mode_drivers_get_the_whole_script() {
        // Snowflake runs a script as one request (its own scripting).
        let sf = dbine_drivers::find("snowflake").unwrap().as_ref();
        assert_eq!(sf.script_mode(), ScriptMode::Whole);
        assert!(script_units(RunMode::Whole, sf, "select 1; select 2").is_none());
        assert!(script_units(RunMode::Auto, sf, "select 1; select 2").is_none());
        assert_eq!(script_units(RunMode::PerStatement, sf, "select 1; select 2").unwrap().len(), 2);
    }

    #[test]
    fn unsafe_dml_is_checked_on_sql_engines_only() {
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        let found = unsafe_dml(pg, "select 1;\ndelete from t;\nupdate t set a = 1 where b = 2");
        assert_eq!(found.iter().map(|u| (u.keyword.as_str(), u.line)).collect::<Vec<_>>(), vec![("DELETE", 2)]);
        let redis = dbine_drivers::find("redis").unwrap().as_ref();
        assert!(unsafe_dml(redis, "delete from t").is_empty());
    }

    #[test]
    fn engines_get_their_dialect_before_adapting() {
        // PostgreSQL: a dollar-quoted body is one statement, asked nothing.
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        let f = "create function f() returns void as $$ begin delete from log; end $$ language plpgsql;\nselect 1";
        assert!(unsafe_dml(pg, f).is_empty());
        let st = split_for_ui(Some(pg), f, true);
        assert_eq!(st.len(), 2);
        assert!(st[0].text.ends_with("language plpgsql"));
        // SQL Server: GO lines split, a procedure body is kept whole.
        let ms = dbine_drivers::find("sqlserver").unwrap().as_ref();
        let p = "CREATE PROCEDURE p AS SET NOCOUNT ON; UPDATE t SET a = 1;\nGO\nUPDATE t SET a = 2\nSELECT * FROM t WHERE id = 1";
        let found = unsafe_dml(ms, p);
        assert_eq!(found.iter().map(|u| (&p[u.start..u.end], u.line)).collect::<Vec<_>>(), vec![("UPDATE t SET a = 2", 3)]);
        assert_eq!(split_for_ui(Some(ms), p, true)[0].text, "CREATE PROCEDURE p AS SET NOCOUNT ON; UPDATE t SET a = 1;");
    }

    #[test]
    fn sqlplus_scripts_use_the_drivers_units() {
        // Oracle: SQL*Plus lines are units of their own, for the statement
        // at the cursor and the UPDATE/DELETE check too.
        let ora = dbine_drivers::find("oracle").unwrap().as_ref();
        let sql = "PROMPT Delete old rows\nEXEC p(1)\nSELECT 1 FROM dual;\nUPDATE t SET a = 1;";
        let st = split_for_ui(Some(ora), sql, true);
        assert_eq!(st.iter().map(|u| u.text.as_str()).collect::<Vec<_>>(), ["PROMPT Delete old rows", "EXEC p(1)", "SELECT 1 FROM dual", "UPDATE t SET a = 1"]);
        let found = unsafe_dml(ora, sql);
        assert_eq!(found.iter().map(|u| (&sql[u.start..u.end], u.line)).collect::<Vec<_>>(), vec![("UPDATE t SET a = 1", 4)]);
    }

    #[test]
    fn split_for_the_ui_uses_js_indices() {
        let sql = "select 'ñandú';\nselect 2";
        let st = split_for_ui(None, sql, false);
        assert_eq!(st.len(), 2);
        let js: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(String::from_utf16(&js[st[1].start..st[1].end]).unwrap(), "select 2");
        assert_eq!(st[1].line, 2);
        // A driver's own units.
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        assert_eq!(split_for_ui(Some(pg), "select 1; select 2", false).len(), 2);
    }

    #[test]
    fn utf16_positions_match_a_full_encode() {
        let mut s = String::new();
        for i in 0..2000 {
            s.push_str(["a", "é", "😀", "ñandú ", "\n", "中"][i % 6]);
        }
        let p = Utf16::new(&s);
        for b in 0..=s.len() + 3 {
            let mut f = b.min(s.len());
            while !s.is_char_boundary(f) {
                f -= 1;
            }
            assert_eq!(p.at(b), s[..f].encode_utf16().count(), "{b}");
        }
    }

    #[test]
    fn utf16_positions() {
        let p = Utf16::new("aé😀b");
        assert_eq!((p.at(0), p.at(1), p.at(3), p.at(7), p.at(8)), (0, 1, 2, 4, 5));
        assert_eq!(line_of("a\nb\nc", 4), 3);
    }

    #[tokio::test]
    async fn closing_the_tab_drops_without_waiting() {
        let e = entry(Some(Arc::new(|| {})));
        let closer = e.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            // close_session: no cancel flag, just the wake-up.
            closer.cancel.notify_waiters();
        });
        let started = std::time::Instant::now();
        let r = run_cancellable(std::future::pending(), &e, Duration::from_secs(30), || false).await;
        assert!(matches!(r, Run::Cancelled { keep: false, completed: false }), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn closing_the_tab_during_the_grace_ends_the_wait() {
        let e = entry(Some(Arc::new(|| {})));
        let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
        cancel_soon(&e);
        let (closer, flag) = (e.clone(), open.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            flag.store(false, Ordering::SeqCst);
            closer.cancel.notify_waiters();
        });
        let started = std::time::Instant::now();
        let r = run_cancellable(std::future::pending(), &e, Duration::from_secs(30), || open.load(Ordering::SeqCst)).await;
        assert!(matches!(r, Run::Cancelled { keep: false, completed: false }), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

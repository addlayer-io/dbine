//! "Ejecutar en varias bases…": the editor's script on several databases of
//! one connection, each on a session of its own (never the explorer's
//! `meta:` session), at most [`MAX_PARALLEL`] at a time. The script runs the
//! way the editor runs it on that driver (statement by statement, batch by
//! batch, or whole), and the connection's read-only flag holds: the sessions
//! come from `dbine_drivers::open_session`, which wraps them.
//!
//! Results come back together: when the first result set of every database
//! that ran without errors has the same columns, they're merged into one
//! grid with a leading `base` column; otherwise each database keeps its own.
//! `multi-db-progress` reports each database as it ends; `cancel_multi_db`
//! stops starting new ones and interrupts the ones running.

use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dashmap::DashMap;
use dbine_driver::sql::{ScriptMode, StatementKind};
use dbine_driver::{Driver, Error, Language, MessageLevel, QueryOutcome, ResultColumn, ScriptError, Session, StatementResult};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Databases running at the same time.
pub const MAX_PARALLEL: usize = 4;
/// Rows kept per result set and database when the UI doesn't say.
const DEFAULT_MAX_ROWS: usize = 5_000;
/// How long an interrupted statement gets to stop before its connection is
/// dropped (as the editor's cancel does).
const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// The merged grid's first column: the database each row came from.
pub const BASE_COLUMN: &str = "base";

/// Runs in progress: their cancel flag.
static RUNS: LazyLock<DashMap<String, Arc<AtomicBool>>> = LazyLock::new(DashMap::new);

fn session_prefix(run_id: &str) -> String {
    format!("multidb:{run_id}:")
}

#[derive(Deserialize)]
pub struct MultiDbArgs {
    /// Chosen by the UI: `cancel_multi_db` and the progress events use it.
    pub run_id: String,
    pub connection_id: String,
    pub databases: Vec<String>,
    pub sql: String,
    /// Per result set and database, as a normal run.
    pub max_rows: Option<usize>,
    /// Go on after a failed statement; `None`: the engine's default.
    #[serde(default)]
    pub continue_on_error: Option<bool>,
    /// The user agreed to run a script that isn't read-only.
    #[serde(default)]
    pub confirmed_write: bool,
    /// Only say whether it needs confirming (`needs_confirmation`); nothing
    /// runs. The UI asks first, then starts the run as a task.
    #[serde(default)]
    pub check_only: bool,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DbStatus {
    Ok,
    Error,
    /// Interrupted by a cancel while it ran.
    Cancelled,
    /// Not started: the run was cancelled first.
    Skipped,
}

/// One database's outcome.
#[derive(Serialize, Debug, Clone)]
pub struct DbRun {
    pub database: String,
    pub status: DbStatus,
    pub error: Option<String>,
    /// Rows of its result sets (before the `max_rows` cut).
    pub rows: u64,
    /// Rows its statements changed, when the driver says.
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u64,
    /// Its result sets, minus the one merged into the shared grid.
    pub results: Vec<StatementResult>,
    /// Server messages (PRINT, notices…).
    pub messages: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct MultiDbResponse {
    /// Nothing ran: the script isn't read-only (the first statement that
    /// writes, upper case; empty when the engine's language can't be
    /// checked). Run again with `confirmed_write` once the user agrees.
    pub needs_confirmation: Option<String>,
    /// Every database's first result set in one grid (`base` first).
    pub merged: Option<StatementResult>,
    /// In the order asked.
    pub databases: Vec<DbRun>,
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

/// `multi-db-progress`: a database ended.
#[derive(Serialize, Clone, Debug)]
pub struct MultiDbProgress {
    pub run_id: String,
    pub database: String,
    pub status: DbStatus,
    pub rows: u64,
    pub elapsed_ms: u64,
    pub error: Option<String>,
    pub done: usize,
    pub total: usize,
}

/// A database's run, before the merge.
#[derive(Debug, Default)]
pub struct DbOutcome {
    pub database: String,
    pub outcome: QueryOutcome,
    /// Connecting failed (nothing ran).
    pub connect_error: Option<String>,
    pub cancelled: bool,
    pub skipped: bool,
    pub elapsed_ms: u64,
}

impl DbOutcome {
    fn status(&self) -> DbStatus {
        if self.skipped {
            DbStatus::Skipped
        } else if self.cancelled {
            DbStatus::Cancelled
        } else if self.connect_error.is_some() || self.outcome.error.is_some() || !self.outcome.errors.is_empty() {
            DbStatus::Error
        } else {
            DbStatus::Ok
        }
    }

    fn error(&self) -> Option<String> {
        self.connect_error.clone().or_else(|| self.outcome.error.clone()).or_else(|| self.outcome.errors.first().map(|e| e.message.clone()))
    }
}

/// The first statement of `sql` that isn't a read, as the read-only guard
/// classifies them; `Some("")` when the engine's language can't be checked.
fn write_keyword(driver: &dyn Driver, sql: &str) -> Option<String> {
    if driver.info().language != Language::Sql {
        return Some(String::new());
    }
    dbine_driver::read_only::first_write_in(sql, &driver.script_dialect())
}

/// Merge the databases' outcomes: one grid when the first result set of
/// every database that ran without errors has the same column names (case
/// aside), each database's own result sets otherwise. Failed, cancelled and
/// skipped databases keep their results and error, and never join the grid.
pub fn merge(outcomes: Vec<DbOutcome>) -> (Option<StatementResult>, Vec<DbRun>) {
    let first_set = |o: &DbOutcome| o.outcome.results.iter().position(|r| !r.columns.is_empty());
    let names = |cols: &[ResultColumn]| cols.iter().map(|c| c.name.to_lowercase()).collect::<Vec<_>>();
    let ok: Vec<&DbOutcome> = outcomes.iter().filter(|o| o.status() == DbStatus::Ok).collect();
    let mergeable = !ok.is_empty() && {
        let shape = |o: &DbOutcome| first_set(o).map(|i| names(&o.outcome.results[i].columns));
        let first = shape(ok[0]);
        first.is_some() && ok.iter().all(|o| shape(o) == first)
    };
    let mut merged: Option<StatementResult> = None;
    let mut runs = Vec::with_capacity(outcomes.len());
    for o in outcomes {
        let status = o.status();
        let error = o.error();
        let DbOutcome { database, outcome, elapsed_ms, .. } = o;
        let rows = outcome.results.iter().filter(|r| !r.columns.is_empty()).map(|r| r.total_rows).sum();
        let affected: Vec<u64> = outcome.results.iter().filter_map(|r| r.rows_affected).collect();
        let rows_affected = (!affected.is_empty()).then(|| affected.iter().sum());
        let messages = outcome.log.iter().filter(|m| m.level != MessageLevel::Error).map(|m| m.text.clone()).collect();
        let mut results = outcome.results;
        if mergeable && status == DbStatus::Ok {
            if let Some(i) = results.iter().position(|r| !r.columns.is_empty()) {
                let set = results.remove(i);
                let m = merged.get_or_insert_with(|| StatementResult {
                    columns: std::iter::once(ResultColumn { name: BASE_COLUMN.into(), type_name: String::new() })
                        .chain(set.columns.iter().cloned())
                        .collect(),
                    ..Default::default()
                });
                m.total_rows += set.total_rows;
                m.truncated |= set.truncated;
                let db = serde_json::Value::String(database.clone());
                m.rows.extend(set.rows.into_iter().map(|row| std::iter::once(db.clone()).chain(row).collect()));
            }
        }
        runs.push(DbRun { database, status, error, rows, rows_affected, elapsed_ms, results, messages });
    }
    (merged, runs)
}

/// Run `sql` on `session` as the editor runs it on `driver`: unit by unit
/// (its statements or batches, `GO N` repeated) or whole, recording every
/// failure in `out`. After a failure it goes on only with
/// `continue_on_error`; it always stops on `cancelled`, a lost connection or
/// a fatal error, and returns that error (already recorded, except
/// `Cancelled`).
pub async fn run_script_on(
    session: &mut dyn Session,
    driver: &dyn Driver,
    sql: &str,
    max_rows: usize,
    continue_on_error: bool,
    cancelled: &AtomicBool,
    out: &mut QueryOutcome,
) -> dbine_driver::Result<()> {
    if driver.script_mode() == ScriptMode::Whole {
        out.continue_on_error = Some(continue_on_error);
        let r = session.execute(sql, max_rows, out).await;
        out.adopt_plain_messages();
        return r;
    }
    let units = driver.split_script(sql);
    // A client-side error (`GO 99999999999`): nothing runs.
    if let Some((i, msg)) = units.iter().enumerate().find_map(|(i, u)| u.error.as_ref().map(|m| (i, m))) {
        let mut e = ScriptError::new(msg.clone()).fatal();
        e.statement = Some(i);
        out.push_error(e.clone());
        return Err(e.into());
    }
    for (idx, unit) in units.iter().enumerate() {
        if unit.kind == StatementKind::ClientCommand {
            continue;
        }
        out.current_statement = Some(idx);
        let mut failed = None;
        for _ in 0..unit.repeat.max(1) {
            if cancelled.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let e0 = out.errors.len();
            let r = session.execute(&unit.text, max_rows, out).await;
            out.adopt_plain_messages();
            match r {
                Ok(()) => {}
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    if !recorded(&out.errors[e0..], &e) {
                        out.push_error(e.to_script_error());
                    }
                    let ends = e.ends_script();
                    failed = Some(e);
                    if ends {
                        break;
                    }
                }
            }
        }
        if let Some(e) = failed {
            if e.ends_script() || !continue_on_error {
                out.current_statement = None;
                return Err(e);
            }
        }
    }
    out.current_statement = None;
    Ok(())
}

/// The driver already put this error in the outcome.
fn recorded(errors: &[ScriptError], e: &Error) -> bool {
    !matches!(e, Error::Cancelled) && errors.iter().any(|x| x.message == e.to_string())
}

/// Await `run` until it ends or `entry` is cancelled (or its connection
/// closed). A cancel fired the driver's interrupter: the statement gets
/// [`CANCEL_GRACE`] to return. `None`: it was dropped mid-flight.
async fn until_cancelled<F, R>(run: F, entry: &SessionEntry) -> Option<R>
where
    F: Future<Output = R>,
{
    let woken = entry.cancel.notified();
    tokio::pin!(run, woken);
    // Registered before the check: a cancel in between still wakes it.
    woken.as_mut().enable();
    if !entry.cancelled.load(Ordering::SeqCst) {
        tokio::select! {
            r = &mut run => return Some(r),
            _ = &mut woken => {}
        }
    }
    // Without an interrupter, dropping the statement is the only way to stop it.
    entry.interrupter.as_ref()?;
    tokio::select! {
        r = &mut run => Some(r),
        _ = tokio::time::sleep(CANCEL_GRACE) => None,
    }
}

/// Stop what runs on `entry` (as the editor's cancel does).
fn interrupt(entry: &SessionEntry) {
    entry.cancelled.store(true, Ordering::SeqCst);
    if let Some(i) = &entry.interrupter {
        i();
    }
    entry.cancel.notify_waiters();
}

struct DbJob {
    state: AppState,
    driver: &'static Arc<dyn Driver>,
    run_id: String,
    connection_id: String,
    database: String,
    sql: Arc<String>,
    max_rows: usize,
    continue_on_error: bool,
    flag: Arc<AtomicBool>,
}

async fn run_database(job: DbJob) -> DbOutcome {
    let mut o = DbOutcome { database: job.database.clone(), ..Default::default() };
    if job.flag.load(Ordering::SeqCst) {
        o.skipped = true;
        return o;
    }
    let started = Instant::now();
    let key = format!("{}{}", session_prefix(&job.run_id), job.database);
    let entry = match job.state.dedicated_session(&key, &job.connection_id, &job.database, false).await {
        Ok(e) => e,
        Err(e) => {
            o.connect_error = Some(e.to_string());
            o.elapsed_ms = started.elapsed().as_millis() as u64;
            return o;
        }
    };
    // A cancel that came while it connected.
    if job.flag.load(Ordering::SeqCst) {
        entry.cancelled.store(true, Ordering::SeqCst);
    }
    let mut out = QueryOutcome::default();
    let finished = {
        let mut session = entry.session.lock().await;
        let work = run_script_on(&mut **session, job.driver.as_ref(), &job.sql, job.max_rows, job.continue_on_error, &entry.cancelled, &mut out);
        until_cancelled(work, &entry).await
    };
    job.state.sessions.remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
    out.current_statement = None;
    let was_cancelled = entry.cancelled.load(Ordering::SeqCst) || job.flag.load(Ordering::SeqCst);
    match finished {
        Some(Ok(())) => {}
        Some(Err(Error::Cancelled)) => o.cancelled = true,
        Some(Err(e)) => {
            if was_cancelled {
                o.cancelled = true;
            } else if !recorded(&out.errors, &e) {
                out.push_error(e.to_script_error());
            }
        }
        None => o.cancelled = true,
    }
    o.outcome = out;
    o.elapsed_ms = started.elapsed().as_millis() as u64;
    o
}

/// Run the script on each of `databases`, at most [`MAX_PARALLEL`] at once.
#[tauri::command(rename_all = "camelCase")]
pub async fn run_multi_db(app: AppHandle, state: State<'_, AppState>, args: MultiDbArgs) -> CommandResult<MultiDbResponse> {
    let saved = state
        .store
        .get_connection(&args.connection_id)?
        .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{}'", args.connection_id)))?;
    let driver = dbine_drivers::find(&saved.config.driver)
        .ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{}'", saved.config.driver)))?;
    if args.databases.is_empty() {
        return Err(CommandError::BadRequest("no se eligió ninguna base".into()));
    }
    let needs_confirmation = (!args.confirmed_write && !saved.config.read_only).then(|| write_keyword(driver.as_ref(), &args.sql)).flatten();
    if needs_confirmation.is_some() || args.check_only {
        return Ok(MultiDbResponse { needs_confirmation, merged: None, databases: Vec::new(), cancelled: false, elapsed_ms: 0 });
    }
    let started = Instant::now();
    let flag = Arc::new(AtomicBool::new(false));
    RUNS.insert(args.run_id.clone(), flag.clone());
    let total = args.databases.len();
    let sql = Arc::new(args.sql);
    let max_rows = args.max_rows.unwrap_or(DEFAULT_MAX_ROWS).max(1);
    let continue_on_error = args.continue_on_error.unwrap_or_else(|| driver.script_defaults().continue_on_error);
    let permits = Arc::new(Semaphore::new(MAX_PARALLEL));
    let mut jobs = JoinSet::new();
    for (i, database) in args.databases.iter().enumerate() {
        let job = DbJob {
            state: state.inner().clone(),
            driver,
            run_id: args.run_id.clone(),
            connection_id: args.connection_id.clone(),
            database: database.clone(),
            sql: sql.clone(),
            max_rows,
            continue_on_error,
            flag: flag.clone(),
        };
        let permits = permits.clone();
        jobs.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (i, run_database(job).await)
        });
    }
    let mut outcomes: Vec<Option<DbOutcome>> = (0..total).map(|_| None).collect();
    let mut done = 0;
    while let Some(joined) = jobs.join_next().await {
        let (i, o) = match joined {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(%e, "a multi-database job failed");
                continue;
            }
        };
        done += 1;
        let _ = app.emit(
            "multi-db-progress",
            MultiDbProgress {
                run_id: args.run_id.clone(),
                database: o.database.clone(),
                status: o.status(),
                rows: o.outcome.results.iter().filter(|r| !r.columns.is_empty()).map(|r| r.total_rows).sum(),
                elapsed_ms: o.elapsed_ms,
                error: o.error(),
                done,
                total,
            },
        );
        outcomes[i] = Some(o);
    }
    RUNS.remove(&args.run_id);
    let cancelled = flag.load(Ordering::SeqCst);
    let outcomes = outcomes
        .into_iter()
        .zip(&args.databases)
        .map(|(o, db)| o.unwrap_or_else(|| DbOutcome { database: db.clone(), connect_error: Some("la ejecución terminó sin respuesta".into()), ..Default::default() }))
        .collect();
    let (merged, databases) = merge(outcomes);
    Ok(MultiDbResponse { needs_confirmation: None, merged, databases, cancelled, elapsed_ms: started.elapsed().as_millis() as u64 })
}

#[derive(Deserialize)]
pub struct CancelMultiDbArgs {
    pub run_id: String,
}

/// No new database starts; the running ones are interrupted.
#[tauri::command(rename_all = "camelCase")]
pub async fn cancel_multi_db(state: State<'_, AppState>, args: CancelMultiDbArgs) -> CommandResult<()> {
    if let Some(flag) = RUNS.get(&args.run_id) {
        flag.store(true, Ordering::SeqCst);
    }
    let prefix = session_prefix(&args.run_id);
    let running: Vec<Arc<SessionEntry>> = state.sessions.iter().filter(|e| e.key().starts_with(&prefix)).map(|e| e.value().clone()).collect();
    for entry in running {
        interrupt(&entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(cols: &[&str], rows: Vec<Vec<serde_json::Value>>) -> StatementResult {
        StatementResult {
            columns: cols.iter().map(|c| ResultColumn { name: c.to_string(), type_name: String::new() }).collect(),
            total_rows: rows.len() as u64,
            rows,
            ..Default::default()
        }
    }

    fn ok(db: &str, results: Vec<StatementResult>) -> DbOutcome {
        DbOutcome { database: db.into(), outcome: QueryOutcome { results, ..Default::default() }, elapsed_ms: 5, ..Default::default() }
    }

    fn failed(db: &str, msg: &str) -> DbOutcome {
        let mut o = ok(db, Vec::new());
        o.outcome.push_error(ScriptError::new(msg));
        o
    }

    #[test]
    fn same_columns_merge_with_the_base_column() {
        let (merged, runs) = merge(vec![
            ok("t1", vec![set(&["id", "name"], vec![vec![json!(1), json!("a")]])]),
            ok("t2", vec![set(&["ID", "Name"], vec![vec![json!(2), json!("b")], vec![json!(3), json!("c")]])]),
        ]);
        let m = merged.expect("merged");
        assert_eq!(m.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["base", "id", "name"]);
        assert_eq!(m.rows, vec![vec![json!("t1"), json!(1), json!("a")], vec![json!("t2"), json!(2), json!("b")], vec![json!("t2"), json!(3), json!("c")]]);
        assert_eq!(m.total_rows, 3);
        assert!(runs.iter().all(|r| r.status == DbStatus::Ok && r.results.is_empty()));
        assert_eq!(runs[1].rows, 2);
    }

    #[test]
    fn different_columns_stay_per_database() {
        let (merged, runs) = merge(vec![ok("t1", vec![set(&["id"], vec![vec![json!(1)]])]), ok("t2", vec![set(&["id", "extra"], vec![])])]);
        assert!(merged.is_none());
        assert_eq!(runs[0].results.len(), 1);
        assert_eq!(runs[1].results[0].columns.len(), 2);
    }

    #[test]
    fn errors_are_kept_and_left_out_of_the_merge() {
        let skipped = DbOutcome { database: "t4".into(), skipped: true, ..Default::default() };
        let (merged, runs) = merge(vec![
            ok("t1", vec![set(&["id"], vec![vec![json!(1)]])]),
            failed("t2", "Invalid object name 'people.customer'."),
            DbOutcome { database: "t3".into(), connect_error: Some("login failed".into()), ..Default::default() },
            skipped,
        ]);
        assert_eq!(merged.expect("merged").rows, vec![vec![json!("t1"), json!(1)]]);
        assert_eq!(runs[1].status, DbStatus::Error);
        assert_eq!(runs[1].error.as_deref(), Some("Invalid object name 'people.customer'."));
        assert_eq!(runs[2].error.as_deref(), Some("login failed"));
        assert_eq!(runs[3].status, DbStatus::Skipped);
    }

    #[test]
    fn statements_without_result_sets_are_not_merged() {
        let affected = StatementResult { rows_affected: Some(3), ..Default::default() };
        let (merged, runs) = merge(vec![ok("t1", vec![affected.clone()]), ok("t2", vec![affected])]);
        assert!(merged.is_none());
        assert_eq!(runs[0].rows_affected, Some(3));
    }

    #[test]
    fn later_result_sets_stay_with_their_database() {
        let (merged, runs) =
            merge(vec![ok("t1", vec![set(&["a"], vec![vec![json!(1)]]), set(&["b"], vec![])]), ok("t2", vec![set(&["a"], vec![vec![json!(2)]])])]);
        assert_eq!(merged.expect("merged").rows.len(), 2);
        assert_eq!(runs[0].results.len(), 1);
        assert_eq!(runs[0].results[0].columns[0].name, "b");
    }
}

/// Live: `cargo test -p dbine multi_db::live -- --ignored --nocapture`
/// against `dbine-test-sqlserver` (port 25013) and `dbine-test-postgres`.
#[cfg(test)]
mod live {
    use super::*;
    use dbine_driver::ConnectionConfig;

    async fn run_on(cfg: &ConnectionConfig, dbs: &[&str], sql: &str) -> (Option<StatementResult>, Vec<DbRun>) {
        let driver = dbine_drivers::find(&cfg.driver).unwrap();
        let mut outcomes = Vec::new();
        for db in dbs {
            let started = Instant::now();
            let mut o = DbOutcome { database: db.to_string(), ..Default::default() };
            match dbine_drivers::open_session(cfg, Some(db)).await {
                Ok(mut s) => {
                    let flag = AtomicBool::new(false);
                    let r = run_script_on(s.as_mut(), driver.as_ref(), sql, 100, false, &flag, &mut o.outcome).await;
                    if let Err(e) = r {
                        if !recorded(&o.outcome.errors, &e) {
                            o.outcome.push_error(e.to_script_error());
                        }
                    }
                }
                Err(e) => o.connect_error = Some(e.to_string()),
            }
            o.elapsed_ms = started.elapsed().as_millis() as u64;
            outcomes.push(o);
        }
        merge(outcomes)
    }

    fn print(label: &str, merged: &Option<StatementResult>, runs: &[DbRun]) {
        match merged {
            Some(m) => println!(
                "[{label}] merged: columns={:?} rows={}",
                m.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                m.rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|")).collect::<Vec<_>>().join(", ")
            ),
            None => println!("[{label}] not merged"),
        }
        for r in runs {
            println!("[{label}]   {} {:?} rows={} sets={} error={:?}", r.database, r.status, r.rows, r.results.len(), r.error);
        }
    }

    async fn setup(cfg: &ConnectionConfig, admin_db: &str, stmts: &[String]) {
        let mut s = dbine_drivers::open_session(cfg, Some(admin_db)).await.expect("connect");
        for q in stmts {
            let mut o = QueryOutcome::default();
            s.execute(q, 10, &mut o).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }

    #[tokio::test]
    #[ignore]
    async fn sqlserver_tenants() {
        let cfg = ConnectionConfig {
            driver: "sqlserver".into(),
            host: "localhost".into(),
            port: 25013,
            username: Some("sa".into()),
            password: Some("Pw_12345!".into()),
            trust_server_certificate: true,
            ..Default::default()
        };
        let dbs = ["mdb_t1", "mdb_t2", "mdb_t3", "mdb_odd"];
        let mut prep = Vec::new();
        for db in dbs {
            prep.push(format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"));
            prep.push(format!("CREATE DATABASE [{db}]"));
        }
        setup(&cfg, "master", &prep).await;
        for (i, db) in dbs.iter().enumerate() {
            let cols = if *db == "mdb_odd" { "id INT, nickname NVARCHAR(50)" } else { "id INT, name NVARCHAR(50)" };
            setup(&cfg, db, &[
                "CREATE SCHEMA people".into(),
                format!("CREATE TABLE people.customer ({cols})"),
                format!("INSERT INTO people.customer VALUES ({i}, N'c{i}'), ({}, N'd{i}')", i + 10),
            ])
            .await;
        }
        let sql = "SELECT TOP 10 * FROM people.customer ORDER BY id";
        let (m, runs) = run_on(&cfg, &dbs[..3], sql).await;
        print("mssql same", &m, &runs);
        assert_eq!(m.as_ref().unwrap().rows.len(), 6);
        assert_eq!(m.as_ref().unwrap().columns[0].name, "base");

        let (m, runs) = run_on(&cfg, &dbs, sql).await;
        print("mssql odd", &m, &runs);
        assert!(m.is_none());
        assert_eq!(runs.len(), 4);

        // A database without the table: its error is reported, the rest merge.
        setup(&cfg, "mdb_t3", &["DROP TABLE people.customer".into()]).await;
        let (m, runs) = run_on(&cfg, &dbs[..3], "SELECT TOP 10 id, name FROM people.customer\nGO\nSELECT 1 AS later").await;
        print("mssql error", &m, &runs);
        assert_eq!(runs[2].status, DbStatus::Error);
        assert!(m.is_some());

        let mut ro = cfg.clone();
        ro.read_only = true;
        let (_, runs) = run_on(&ro, &dbs[..1], "DELETE FROM people.customer").await;
        print("mssql read-only", &None, &runs);
        assert_eq!(runs[0].status, DbStatus::Error);
        assert_eq!(write_keyword(dbine_drivers::find("sqlserver").unwrap().as_ref(), "SELECT 1; UPDATE x SET a = 1").as_deref(), Some("UPDATE"));

        let drop: Vec<String> = dbs.iter().map(|db| format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).collect();
        setup(&cfg, "master", &drop).await;
    }

    #[tokio::test]
    #[ignore]
    async fn postgres_tenants() {
        let port: u16 = std::env::var("DBINE_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432);
        let cfg = ConnectionConfig {
            driver: "postgres".into(),
            host: "localhost".into(),
            port,
            username: Some(std::env::var("DBINE_PG_USER").unwrap_or_else(|_| "postgres".into())),
            password: Some(std::env::var("DBINE_PG_PASSWORD").unwrap_or_else(|_| "postgres".into())),
            ..Default::default()
        };
        let dbs = ["mdb_t1", "mdb_t2", "mdb_t3", "mdb_odd"];
        let mut prep = Vec::new();
        for db in dbs {
            prep.push(format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"));
            prep.push(format!("CREATE DATABASE {db}"));
        }
        setup(&cfg, "postgres", &prep).await;
        for (i, db) in dbs.iter().enumerate() {
            let cols = if *db == "mdb_odd" { "id int, nickname text" } else { "id int, name text" };
            setup(&cfg, db, &[
                "CREATE SCHEMA people".into(),
                format!("CREATE TABLE people.customer ({cols})"),
                format!("INSERT INTO people.customer VALUES ({i}, 'c{i}'), ({}, 'd{i}')", i + 10),
            ])
            .await;
        }
        let sql = "SELECT * FROM people.customer ORDER BY id LIMIT 10;";
        let (m, runs) = run_on(&cfg, &dbs[..3], sql).await;
        print("pg same", &m, &runs);
        assert_eq!(m.as_ref().unwrap().rows.len(), 6);
        let (m, runs) = run_on(&cfg, &dbs, sql).await;
        print("pg odd", &m, &runs);
        assert!(m.is_none());
        setup(&cfg, "mdb_t3", &["DROP TABLE people.customer".into()]).await;
        let (m, runs) = run_on(&cfg, &dbs[..3], sql).await;
        print("pg error", &m, &runs);
        assert_eq!(runs[2].status, DbStatus::Error);
        assert!(m.is_some());
        let drop: Vec<String> = dbs.iter().map(|db| format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).collect();
        setup(&cfg, "postgres", &drop).await;
    }
}

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::export::{export_rows, ExportOptions, Exporter, SourceStrings};
use dbine_driver::sql::leading_keyword;
use dbine_driver::{
    ConnectionConfig, Driver, Language, QueryOutcome, ResultColumn, RowSink, RowSinkRef,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::multi_db::{run_script_on, BASE_COLUMN};
use crate::state::SessionEntry;

#[derive(Deserialize)]
pub struct ExportRowsArgs {
    pub path: String,
    pub options: ExportOptions,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
    /// The connection the rows came from: SQL string literals follow its
    /// engine's escaping. Without it, only the backtick quoting turns the
    /// backslash escaping on.
    #[serde(default)]
    pub connection_id: Option<String>,
}

#[derive(Serialize)]
pub struct ExportResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

/// Export the rows the grid already has.
#[tauri::command(rename_all = "camelCase")]
pub async fn export_rows_to_file(
    state: State<'_, AppState>,
    args: ExportRowsArgs,
) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(&args.path);
    let mut options = args.options;
    // String literals of an SQL export follow the source engine; an unknown
    // source (a multi-database grid, a connection gone) gets the form that
    // ends in the same place on every engine.
    options.source = match args
        .connection_id
        .as_deref()
        .map(|id| driver_of(&state, id))
    {
        Some(Ok(d)) => SourceStrings::of(d.as_ref()),
        _ => SourceStrings::Unknown,
    };
    let rows =
        tokio::task::spawn_blocking(move || export_rows(&path, options, &args.columns, &args.rows))
            .await
            .map_err(|e| CommandError::Internal(e.to_string()))?
            .map_err(|e| CommandError::Internal(format!("no se pudo escribir el archivo: {e}")))?;
    Ok(ExportResult {
        rows,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[derive(Deserialize)]
pub struct ExportQueryArgs {
    /// Chosen by the UI; progress events carry it and `cancel_query` takes
    /// `export:<id>` to stop it.
    pub export_id: String,
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    /// A DataFrame SQL cell's result: the notebook tab whose DuckDB session
    /// (`nbx:<tab>`) runs `sql` again over the DataFrames its last run read
    /// ([`crate::notebook::local::export_local`]), instead of a connection.
    #[serde(default)]
    pub notebook_tab: Option<String>,
    pub sql: String,
    /// Which result set of the script to write.
    pub result_index: usize,
    pub path: String,
    pub options: ExportOptions,
    /// A result of "Ejecutar en varias bases…": `sql` runs again on each of
    /// these databases of the connection instead of `database`.
    #[serde(default)]
    pub multi: Option<DatabasesExport>,
}

/// What a multi-database result's grid showed ([`crate::commands::multi_db`]):
/// the script runs again on each database, one after the other and in the
/// grid's order, and one file gets every row of each one's result set.
#[derive(Deserialize, Clone, Debug)]
pub struct DatabasesExport {
    pub databases: Vec<String>,
    /// Which of each run's result sets with columns: 0 is the first, the
    /// one the merged grid holds.
    pub set: usize,
    /// Each row starts with its database's name, in the `base` column (the
    /// merged grid).
    pub base: bool,
}

/// `export-progress`: rows written so far and, once the engine's estimated
/// plan gave one, the rows the result is expected to have (an estimate: the
/// export can end above or below it).
#[derive(Clone, Serialize)]
struct Progress {
    id: String,
    rows: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
}

/// Minimum time between two progress events of one export.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
/// How long the row estimate may take, login included, before the export
/// goes on without it: it must never be noticed.
const ESTIMATE_TIMEOUT: Duration = Duration::from_secs(3);
/// `total` while no estimate is known.
const UNKNOWN: u64 = u64::MAX;

/// Option keys that pick how a connection signs in.
const AUTH_KEYS: &[&str] = &[
    "auth",
    "auth_mode",
    "authentication",
    "authenticator",
    "auth_method",
];
/// Words in a sign-in method that mean a person takes part (a browser, a
/// device code, MFA, a prompt…): a second login for an estimate would ask
/// them again.
const INTERACTIVE: &[&str] = &[
    "interactive",
    "browser",
    "device",
    "mfa",
    "sso",
    "prompt",
    "okta",
];

/// Whether the export may open a second, read-only login to ask the
/// engine for the row estimate. Not when that login could involve the
/// person: an interactive sign-in, or a tunnel through an SSH agent (it
/// may ask to approve or touch a key if the tunnel has to reopen). Only
/// for a plain query: one statement, and for SQL a read (`SELECT`,
/// `WITH`, `VALUES`, `TABLE`, `FROM`), so nothing else is planned.
fn estimate_allowed(cfg: &ConnectionConfig, driver: &dyn Driver, sql: &str) -> bool {
    let interactive = AUTH_KEYS.iter().filter_map(|k| cfg.option(k)).any(|v| {
        let v = v.to_ascii_lowercase();
        INTERACTIVE.iter().any(|w| v.contains(w))
    });
    // An alias of `~/.ssh/config` may sign in through the agent too.
    let agent_tunnel = crate::tunnels::enabled(cfg)
        && (cfg.option("ssh.auth") == Some("agent") || crate::ssh_config::alias(cfg).is_some());
    if interactive || agent_tunnel || !driver.supports_explain() {
        return false;
    }
    if driver.info().language != Language::Sql {
        return !sql.trim().is_empty();
    }
    // A line that only sets the context (Cosmos DB's `-- container: x`)
    // isn't a second statement.
    match crate::commands::chart::query_units(driver, sql).as_slice() {
        [one] if one.error.is_none() && one.repeat <= 1 => {
            matches!(
                leading_keyword(&one.text, &driver.script_dialect()).as_deref(),
                Some("select" | "with" | "values" | "table" | "from")
            )
        }
        _ => false,
    }
}

/// Rows the engine expects the script to return: the root of its estimated
/// plan (nothing runs), on a read-only session of its own so the export
/// doesn't wait for it. Login and plan share [`ESTIMATE_TIMEOUT`]; past it
/// the plan is interrupted and the session closed (dropped) right away.
/// Only for a script with one plan, the usual case for an export; engines
/// without plans or estimates give `None`.
async fn estimate_rows(
    state: &AppState,
    key: &str,
    connection_id: &str,
    database: &str,
    sql: &str,
) -> Option<u64> {
    let deadline = tokio::time::Instant::now() + ESTIMATE_TIMEOUT;
    let opened = tokio::time::timeout_at(
        deadline,
        state.dedicated_session(key, connection_id, database, true),
    )
    .await;
    // Nothing cancels it by key: only this task holds it, so it closes when
    // the task ends or is aborted.
    state.sessions.remove(key);
    let entry = opened.ok()?.ok()?;
    let work = async {
        let mut s = entry.session.lock().await;
        explain_rows(&mut **s, sql).await
    };
    match tokio::time::timeout_at(deadline, work).await {
        Ok(n) => n,
        Err(_) => {
            if let Some(stop) = &entry.interrupter {
                stop();
            }
            None
        }
    }
}

async fn explain_rows(s: &mut dyn dbine_driver::Session, sql: &str) -> Option<u64> {
    let mut out = QueryOutcome::default();
    s.explain(sql, false, 1, &mut out).await.ok()?;
    match out.plans.as_slice() {
        [p] => p
            .root
            .est_rows
            .filter(|n| n.is_finite() && *n >= 1.0)
            .map(|n| n.round() as u64),
        _ => None,
    }
}

/// Run the script again and stream the chosen result set to the file, all
/// rows, without holding them in memory. The session is read-only whatever
/// the connection says: an export never writes to the database (SQL
/// engines refuse writing statements; the others enforce read-only in
/// their drivers). A DataFrame SQL cell's (`notebook_tab`) runs on its
/// tab's locked-down DuckDB session instead, over the DataFrames it read.
#[tauri::command(rename_all = "camelCase")]
pub async fn export_query_to_file(
    app: AppHandle,
    state: State<'_, AppState>,
    mut args: ExportQueryArgs,
) -> CommandResult<ExportResult> {
    if let Some(multi) = args.multi.take() {
        return export_databases(app, state, args, multi).await;
    }
    let started = std::time::Instant::now();
    let key = format!("export:{}", args.export_id);
    let tab = args.notebook_tab.clone().filter(|t| !t.is_empty());
    let entry = match &tab {
        Some(_) => None,
        None => Some(
            state
                .dedicated_session(&key, &args.connection_id, &args.database, true)
                .await?,
        ),
    };

    let path = PathBuf::from(&args.path);
    let written = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(UNKNOWN));
    let known = |total: &AtomicU64| Some(total.load(Ordering::Relaxed)).filter(|t| *t != UNKNOWN);

    // The estimate comes in while the export runs; its event carries the
    // rows written so far so the count never goes back.
    let estimate_key = format!("export-estimate:{}", args.export_id);
    let allowed = tab.is_none()
        && match (
            state.store.get_connection(&args.connection_id),
            driver_of(&state, &args.connection_id),
        ) {
            (Ok(Some(saved)), Ok(driver)) => {
                // Its tunnel's auth may be its SSH profile's.
                let mut cfg = saved.config;
                state.store.resolve_ssh_profile(&mut cfg).is_ok()
                    && estimate_allowed(&cfg, driver.as_ref(), &args.sql)
            }
            _ => false,
        };
    let estimator = allowed.then(|| {
        let (app, key, id) = (app.clone(), estimate_key.clone(), args.export_id.clone());
        let (conn, db, sql) = (
            args.connection_id.clone(),
            args.database.clone(),
            args.sql.clone(),
        );
        let (written, total) = (written.clone(), total.clone());
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let est = estimate_rows(&state, &key, &conn, &db, &sql).await;
            if let Some(n) = est {
                total.store(n, Ordering::Relaxed);
                let rows = written.load(Ordering::Relaxed);
                let _ = app.emit(
                    "export-progress",
                    Progress {
                        id,
                        rows,
                        total: known(&total),
                    },
                );
            }
        })
    });

    let id = args.export_id.clone();
    let emitter = app.clone();
    let (written_cb, total_cb) = (written.clone(), total.clone());
    let mut last_emit: Option<Instant> = None;
    // String literals of an SQL export follow the source engine.
    let mut options = args.options;
    options.source = match &tab {
        Some(_) => dbine_drivers::find("duckdb")
            .map_or(SourceStrings::Unknown, |d| SourceStrings::of(d.as_ref())),
        None => driver_of(&state, &args.connection_id)
            .map_or(SourceStrings::Unknown, |d| SourceStrings::of(d.as_ref())),
    };
    let exporter = Arc::new(Mutex::new(
        Exporter::new(&path, args.result_index, options).on_progress(move |rows| {
            written_cb.store(rows, Ordering::Relaxed);
            if last_emit.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
                return;
            }
            last_emit = Some(Instant::now());
            let _ = emitter.emit(
                "export-progress",
                Progress {
                    id: id.clone(),
                    rows,
                    total: known(&total_cb),
                },
            );
        }),
    ));
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(exporter.clone())),
        ..Default::default()
    };

    let finished = match (&tab, &entry) {
        (Some(tab), _) => {
            match crate::notebook::local::export_local(&state, tab, &args.sql, &key, &mut out).await
            {
                Ok(()) => Some(Ok(())),
                Err(CommandError::Cancelled) => None,
                Err(e) => {
                    out.sink = None;
                    drop(exporter);
                    // Refused before anything ran: the file was never opened
                    // (one already there stays as it was). Otherwise don't leave
                    // half a file behind.
                    if !matches!(e, CommandError::BadRequest(_)) {
                        let _ = std::fs::remove_file(&path);
                    }
                    return Err(e);
                }
            }
        }
        (_, Some(entry)) => {
            let mut s = entry.session.lock().await;
            tokio::select! {
                r = s.execute(&args.sql, usize::MAX, &mut out) => Some(r),
                _ = entry.cancel.notified() => None,
            }
        }
        (None, None) => return Err(CommandError::Internal("exportador".into())),
    };
    if tab.is_none() {
        state.sessions.remove(&key);
    }
    if let Some(task) = estimator {
        // Wait for it to stop so its session can't register after the removal.
        task.abort();
        let _ = task.await;
    }
    state.sessions.remove(&estimate_key);
    out.sink = None;

    let failure = match finished {
        None => Some("Exportación cancelada.".to_string()),
        Some(Err(e)) => Some(e.to_string()),
        Some(Ok(())) => out
            .sink_error
            .clone()
            .map(|e| format!("no se pudo escribir el archivo: {e}"))
            .or_else(|| {
                (out.results.len() <= args.result_index)
                    .then(|| "la consulta no devolvió ese resultado".to_string())
            })
            .or_else(|| {
                out.results[args.result_index]
                    .columns
                    .is_empty()
                    .then(|| "ese resultado no tiene filas para exportar".to_string())
            }),
    };
    let rows = exporter
        .lock()
        .map_err(|_| CommandError::Internal("exportador".into()))?
        .finish();
    if let Some(msg) = failure {
        // Don't leave half a file behind.
        let _ = std::fs::remove_file(&path);
        return Err(CommandError::Sql(msg));
    }
    let rows =
        rows.map_err(|e| CommandError::Internal(format!("no se pudo cerrar el archivo: {e}")))?;
    // The final count from `finish` may have fallen inside the throttle window.
    let _ = app.emit(
        "export-progress",
        Progress {
            id: args.export_id.clone(),
            rows,
            total: known(&total),
        },
    );
    Ok(ExportResult {
        rows,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Why a database's result can't join the file: its columns aren't the
/// first database's any more.
const COLUMNS_CHANGED: &str = "el resultado no tiene las mismas columnas que en la primera base";
const CANCELLED: &str = "Exportación cancelada.";
const NO_RESULT: &str = "la consulta no devolvió ese resultado";

/// The rows of a multi-database export. From each database's run, the
/// [`DatabasesExport::set`]-th result set with columns goes to the file,
/// whose columns are the first database's (after `base` when the grid had
/// it).
pub(crate) struct DatabasesSink {
    exporter: Exporter,
    set: usize,
    base: bool,
    database: Value,
    /// Result sets with columns this database's run has begun.
    seen: usize,
    /// This run's result set being written.
    target: Option<usize>,
    /// The file's column names, case aside (`base` aside).
    names: Option<Vec<String>>,
    /// Why the export can't go on.
    problem: Option<String>,
    row: Vec<Value>,
}

impl DatabasesSink {
    pub(crate) fn new(exporter: Exporter, set: usize, base: bool) -> Self {
        Self {
            exporter,
            set,
            base,
            database: Value::Null,
            seen: 0,
            target: None,
            names: None,
            problem: None,
            row: Vec::new(),
        }
    }

    /// The next database's run starts.
    pub(crate) fn next_database(&mut self, name: &str) {
        self.database = Value::String(name.to_string());
        self.seen = 0;
        self.target = None;
    }

    /// This database's run had the result set.
    pub(crate) fn found(&self) -> bool {
        self.target.is_some()
    }

    pub(crate) fn finish(&mut self) -> std::io::Result<u64> {
        self.exporter.finish()
    }
}

impl RowSink for DatabasesSink {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if columns.is_empty() || self.target.is_some() {
            return Ok(());
        }
        self.seen += 1;
        if self.seen <= self.set {
            return Ok(());
        }
        let names: Vec<String> = columns.iter().map(|c| c.name.to_lowercase()).collect();
        match &self.names {
            None => {
                let base = self.base.then(|| ResultColumn {
                    name: BASE_COLUMN.into(),
                    type_name: String::new(),
                });
                let all: Vec<ResultColumn> =
                    base.into_iter().chain(columns.iter().cloned()).collect();
                self.exporter.begin(0, &all)?;
                self.names = Some(names);
            }
            Some(first) if *first == names => {}
            Some(_) => {
                let db = self.database.as_str().unwrap_or_default();
                self.problem = Some(format!("en la base {db}: {COLUMNS_CHANGED}"));
                return Err(std::io::Error::other(COLUMNS_CHANGED));
            }
        }
        self.target = Some(index);
        Ok(())
    }

    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if self.target != Some(index) {
            return Ok(());
        }
        if !self.base {
            return self.exporter.row(0, row);
        }
        self.row.clear();
        self.row.push(self.database.clone());
        self.row.extend_from_slice(row);
        self.exporter.row(0, &self.row)
    }
}

/// How one database's run of a multi-database export ended.
fn database_failure(
    db: &str,
    entry: &SessionEntry,
    finished: Option<dbine_driver::Result<()>>,
    out: &QueryOutcome,
    sink: &DatabasesSink,
) -> Option<String> {
    let cancelled = entry.cancelled.load(Ordering::SeqCst);
    match finished {
        None | Some(Err(dbine_driver::Error::Cancelled)) => Some(CANCELLED.into()),
        Some(Err(_)) if cancelled => Some(CANCELLED.into()),
        Some(Err(e)) => Some(format!("en la base {db}: {e}")),
        Some(Ok(())) => sink
            .problem
            .clone()
            .or_else(|| {
                out.sink_error
                    .as_ref()
                    .map(|e| format!("no se pudo escribir el archivo: {e}"))
            })
            .or_else(|| {
                out.errors
                    .first()
                    .map(|e| format!("en la base {db}: {}", e.message))
            })
            .or_else(|| (!sink.found()).then(|| format!("en la base {db}: {NO_RESULT}"))),
    }
}

/// Run the script of a multi-database result again on each of its
/// databases and stream what the grid showed, every row, to one file. Each
/// database gets a read-only session of its own, one after the other, and
/// runs the script as the multi-database run did (statement by statement or
/// batch by batch, see [`run_script_on`]), so the result set is the same
/// one. A database that fails stops the export: no half file stays.
async fn export_databases(
    app: AppHandle,
    state: State<'_, AppState>,
    args: ExportQueryArgs,
    multi: DatabasesExport,
) -> CommandResult<ExportResult> {
    let started = Instant::now();
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("export:{}", args.export_id);
    let path = PathBuf::from(&args.path);
    let mut options = args.options;
    options.source = SourceStrings::of(driver.as_ref());
    let id = args.export_id.clone();
    let emitter = app.clone();
    let mut last_emit: Option<Instant> = None;
    let exporter = Exporter::new(&path, 0, options).on_progress(move |rows| {
        if last_emit.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return;
        }
        last_emit = Some(Instant::now());
        let _ = emitter.emit(
            "export-progress",
            Progress {
                id: id.clone(),
                rows,
                total: None,
            },
        );
    });
    let sink = Arc::new(Mutex::new(DatabasesSink::new(
        exporter, multi.set, multi.base,
    )));
    fn lock(s: &Mutex<DatabasesSink>) -> CommandResult<std::sync::MutexGuard<'_, DatabasesSink>> {
        s.lock()
            .map_err(|_| CommandError::Internal("exportador".into()))
    }

    let mut failure = multi.databases.is_empty().then(|| NO_RESULT.to_string());
    // The session that ran last stays registered while the next one
    // connects: a cancel then reaches it, and the loop sees it.
    let mut prev: Option<Arc<SessionEntry>> = None;
    let was_cancelled = |e: &Option<Arc<SessionEntry>>| {
        e.as_ref()
            .is_some_and(|e| e.cancelled.load(Ordering::SeqCst))
    };
    for db in &multi.databases {
        if failure.is_some() {
            break;
        }
        if was_cancelled(&prev) {
            failure = Some(CANCELLED.into());
            break;
        }
        let entry = match state
            .dedicated_session(&key, &args.connection_id, db, true)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                failure = Some(format!("en la base {db}: {e}"));
                break;
            }
        };
        if was_cancelled(&prev) {
            entry.cancelled.store(true, Ordering::SeqCst);
        }
        lock(&sink)?.next_database(db);
        let mut out = QueryOutcome {
            sink: Some(RowSinkRef(sink.clone())),
            ..Default::default()
        };
        let finished = if entry.cancelled.load(Ordering::SeqCst) {
            None
        } else {
            let mut s = entry.session.lock().await;
            tokio::select! {
                r = run_script_on(&mut **s, driver.as_ref(), &args.sql, usize::MAX, false, &entry.cancelled, &mut out) => Some(r),
                _ = entry.cancel.notified() => None,
            }
        };
        out.sink = None;
        failure = database_failure(db, &entry, finished, &out, &*lock(&sink)?);
        prev = Some(entry);
    }
    state.sessions.remove(&key);

    let rows = lock(&sink)?.finish();
    if let Some(msg) = failure {
        // Don't leave half a file behind.
        let _ = std::fs::remove_file(&path);
        return Err(CommandError::Sql(msg));
    }
    let rows =
        rows.map_err(|e| CommandError::Internal(format!("no se pudo cerrar el archivo: {e}")))?;
    let _ = app.emit(
        "export-progress",
        Progress {
            id: args.export_id.clone(),
            rows,
            total: None,
        },
    );
    Ok(ExportResult {
        rows,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ConnectionConfig;

    fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
        let url = std::env::var(env).ok()?;
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.rsplit_once('@')?;
        let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
        let hostport = hostport.split('/').next()?;
        let (host, port) = hostport.rsplit_once(':')?;
        Some(ConnectionConfig {
            driver: driver.into(),
            host: host.into(),
            port: port.parse().ok()?,
            username: Some(user.into()),
            password: (!pass.is_empty()).then(|| pass.into()),
            trust_server_certificate: true,
            read_only: true,
            ..Default::default()
        })
    }

    fn driver(id: &str) -> &'static dyn Driver {
        dbine_drivers::find(id)
            .unwrap_or_else(|| panic!("driver {id} not built in"))
            .as_ref()
    }

    fn with(driver: &str, opts: &[(&str, &str)]) -> ConnectionConfig {
        let mut c = ConnectionConfig {
            driver: driver.into(),
            ..Default::default()
        };
        for (k, v) in opts {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn estimates_only_plain_queries() {
        let (pg, cfg) = (driver("postgres"), with("postgres", &[]));
        assert!(estimate_allowed(&cfg, pg, "SELECT * FROM t"));
        assert!(estimate_allowed(
            &cfg,
            pg,
            "-- rows\nWITH x AS (SELECT 1) SELECT * FROM x;"
        ));
        assert!(!estimate_allowed(&cfg, pg, "SELECT 1; SELECT 2"));
        assert!(!estimate_allowed(&cfg, pg, "CALL p()"));
        assert!(!estimate_allowed(&cfg, pg, "DO $$ BEGIN END $$"));
        assert!(!estimate_allowed(&cfg, pg, "   "));
        let (ms, cfg) = (driver("sqlserver"), with("sqlserver", &[("auth", "sql")]));
        assert!(estimate_allowed(&cfg, ms, "SELECT TOP 10 * FROM t"));
        assert!(!estimate_allowed(&cfg, ms, "EXEC dbo.p"));
        assert!(!estimate_allowed(&cfg, ms, "SELECT 1\nGO\nSELECT 2"));
        assert!(!estimate_allowed(&cfg, ms, "SELECT 1\nGO 3"));
        // Cosmos DB: the container line is part of the query it names.
        let (cosmos, cfg) = (driver("cosmosdb"), with("cosmosdb", &[]));
        assert!(estimate_allowed(
            &cfg,
            cosmos,
            "-- container: items\nSELECT * FROM c"
        ));
        assert!(!estimate_allowed(
            &cfg,
            cosmos,
            "-- container: items\nSELECT * FROM c;\nSELECT VALUE COUNT(1) FROM c"
        ));
    }

    #[test]
    fn no_second_login_that_could_ask_the_user() {
        let ms = driver("sqlserver");
        for auth in ["sql", "entra_password", "entra_sp", "entra_token"] {
            assert!(
                estimate_allowed(&with("sqlserver", &[("auth", auth)]), ms, "SELECT 1"),
                "{auth}"
            );
        }
        for auth in [
            "entra_interactive",
            "ActiveDirectoryInteractive",
            "externalbrowser",
            "device_code",
            "entra_mfa",
            "sso",
        ] {
            assert!(
                !estimate_allowed(&with("sqlserver", &[("auth", auth)]), ms, "SELECT 1"),
                "{auth}"
            );
        }
        let pg = driver("postgres");
        assert!(!estimate_allowed(
            &with("postgres", &[("authenticator", "externalbrowser")]),
            pg,
            "SELECT 1"
        ));
        let ssh = |auth: &str| with("postgres", &[("ssh.enabled", "true"), ("ssh.auth", auth)]);
        assert!(estimate_allowed(&ssh("password"), pg, "SELECT 1"));
        assert!(estimate_allowed(&ssh("key"), pg, "SELECT 1"));
        assert!(!estimate_allowed(&ssh("agent"), pg, "SELECT 1"));
        // The agent only matters while the tunnel is on.
        assert!(estimate_allowed(
            &with(
                "postgres",
                &[("ssh.enabled", "false"), ("ssh.auth", "agent")]
            ),
            pg,
            "SELECT 1"
        ));
    }

    /// The export's row estimate on real servers (read-only session, as the
    /// export opens it): `DBINE_TEST_SQLSERVER_URL`, `DBINE_TEST_POSTGRES_URL`.
    /// `cargo test -p dbine --lib export::tests -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn estimates_the_rows_of_a_query() {
        let cases = [
            (
                "sqlserver",
                "DBINE_TEST_SQLSERVER_URL",
                "SELECT TOP 5000 a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b",
            ),
            (
                "postgres",
                "DBINE_TEST_POSTGRES_URL",
                "SELECT g FROM generate_series(1, 5000) g",
            ),
        ];
        let mut ran = 0;
        for (driver, env, sql) in cases {
            let Some(cfg) = cfg(driver, env) else {
                eprintln!("{env} not set; skipping");
                continue;
            };
            let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
            let n = explain_rows(&mut *s, sql).await;
            eprintln!("{driver}: {n:?}");
            // TOP / generate_series bounds: the plans say 5000 (PostgreSQL
            // before 12 guessed 1000 for set-returning functions).
            assert!(matches!(n, Some(5000 | 1000)), "{driver}: {n:?}");
            // A script with two plans gives no estimate.
            let two = format!("{sql};\n{sql}");
            assert_eq!(explain_rows(&mut *s, &two).await, None, "{driver}");
            ran += 1;
        }
        assert!(ran > 0, "no test server configured");
    }

    /// Every SQL driver, with how its SQL exports write strings. A new SQL
    /// driver fails here until it is classified.
    #[test]
    fn sql_exports_know_how_each_source_reads_strings() {
        use SourceStrings::*;
        let cases = [
            // Reads backslash escapes: the dual-safe form, exact there.
            ("mysql", Backslash),
            ("mariadb", Backslash),
            ("aurora-mysql", Backslash),
            ("cloudsql-mysql", Backslash),
            ("tidb", Backslash),
            ("oceanbase", Backslash),
            ("singlestore", Backslash),
            ("doris", Backslash),
            ("starrocks", Backslash),
            ("velodb", Backslash),
            ("databend", Backslash),
            ("greptimedb", Backslash),
            ("manticore", Backslash),
            ("clickhouse", Backslash),
            ("timeplus", Backslash),
            ("bigquery", Backslash),
            ("snowflake", Backslash),
            ("databricks", Backslash),
            ("azure_databricks", Backslash),
            ("spark", Backslash),
            ("kyuubi", Backslash),
            ("hive", Backslash),
            ("impala", Backslash),
            ("cloudera", Backslash),
            ("spanner", Backslash),
            ("couchbase", Backslash),
            ("orientdb", Backslash),
            ("tdengine", Backslash),
            ("redshift", Backslash),
            // Standard strings, with the engine's character function.
            ("postgres", Postgres),
            ("alloydb", Postgres),
            ("aurora_postgres", Postgres),
            ("cloudsql_postgres", Postgres),
            ("cockroachdb", Postgres),
            ("cratedb", Postgres),
            ("denodo", Postgres),
            ("dsql", Postgres),
            ("edb", Postgres),
            ("fujitsu", Postgres),
            ("greengage", Postgres),
            ("greenplum", Postgres),
            ("cloudberry", Postgres),
            ("h2", Postgres),
            ("kingbase", Postgres),
            ("materialize", Postgres),
            ("opengauss", Postgres),
            ("risingwave", Postgres),
            ("timescaledb", Postgres),
            ("yellowbrick", Postgres),
            ("yugabytedb", Postgres),
            ("sqlserver", SqlServer),
            ("azuresql", SqlServer),
            ("fabric", SqlServer),
            ("babelfish", SqlServer),
            ("oracle", Oracle),
            ("oracle_adb", Oracle),
            ("dameng", Oracle),
            ("sqlite", Sqlite),
            ("libsql", Sqlite),
            ("duckdb", DuckDb),
            ("duckdb_files", DuckDb),
            ("firebird", Firebird),
            ("hana", Hana),
            ("trino", Trino),
            ("presto", Trino),
            ("starburst", Trino),
            ("athena", Trino),
            ("db2", Db2),
            ("teradata", Teradata),
            ("vertica", Vertica),
            ("exasol", Exasol),
            ("netezza", Netezza),
            ("dremio", Dremio),
            // Unknown: the dual-safe form, or the standard one if the user
            // says the target reads strings the standard way.
            // No INSERT … VALUES on the engine, so the script never runs on
            // the source: the target decides.
            ("drill", Unknown),
            ("cosmosdb", Unknown),
            ("influxdb1", Unknown),
            ("influxdb3", Unknown),
            ("netsuite", Unknown),
            // The engine behind the connection isn't known.
            ("odbc", Unknown),
            ("avatica", Unknown),
            ("flightsql", Unknown),
            // No character function to splice a backslash with.
            ("dynamodb", Unknown),
            ("iotdb", Unknown),
            ("timechodb", Unknown),
            ("ksqldb", Unknown),
            ("heavydb", Unknown),
            // Not verified: the character function, the concatenation
            // operator or how the engine reads a backslash in a literal
            // (some read escapes, or do so by a server setting).
            ("phoenix", Unknown),
            ("db2i", Unknown),
            ("db2zos", Unknown),
            ("informix", Unknown),
            ("gbase8s", Unknown),
            ("sybase", Unknown),
            ("sqlanywhere", Unknown),
            ("cubrid", Unknown),
            ("monetdb", Unknown),
            ("altibase", Unknown),
            ("access", Unknown),
            ("dbase", Unknown),
            ("cache", Unknown),
            ("iris", Unknown),
            ("ignite", Unknown),
            ("ignite3", Unknown),
            ("ingres", Unknown),
            ("machbase", Unknown),
            ("maxdb", Unknown),
            ("mimer", Unknown),
            ("nuodb", Unknown),
            ("ocient", Unknown),
            ("openedge", Unknown),
            ("sqream", Unknown),
            ("virtuoso", Unknown),
            ("zen", Unknown),
        ];
        let known: std::collections::HashMap<_, _> = cases.into_iter().collect();
        assert_eq!(known.len(), cases.len(), "an id listed twice");
        for d in dbine_drivers::all()
            .iter()
            .filter(|d| d.info().language == Language::Sql)
        {
            let id = d.info().id;
            let want = known.get(id).unwrap_or_else(|| {
                panic!("{id}: SQL driver without a SourceStrings classification here")
            });
            assert_eq!(SourceStrings::of(d.as_ref()), *want, "{id}");
        }
    }

    /// Values that have broken or bent string literals.
    const TRICKY: [&str; 12] = [
        "a\\b",
        "\\",
        "x\\'",
        "x\\');DROP TABLE users;--",
        "x\\');DROP TABLE users;#",
        "ends with\\",
        "\\\\",
        "it's",
        "line\nbreak\r\n",
        "",
        "x'); DROP TABLE users; --",
        "C:\\temp\\new\\'' \\0 \\Z",
    ];

    async fn run(s: &mut Box<dyn dbine_driver::Session>, sql: &str) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        s.execute(sql, 10_000, &mut out)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(out.error.is_none(), "{sql}: {:?}", out.error);
        out
    }

    /// Stores `values` in a table (through hex, which no string escaping
    /// touches), reads them back, exports them as an SQL script with the
    /// source set to the connection's driver, runs the script on the same
    /// engine into a second table and checks every value came back exactly.
    async fn sql_export_round_trip(
        cfg: ConnectionConfig,
        text_type: &str,
        from_hex: fn(&str) -> String,
        quote: &str,
        tables: (&str, &str),
        values: &[String],
    ) {
        let d = dbine_drivers::find(&cfg.driver).unwrap();
        let source = SourceStrings::of(d.as_ref());
        let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
        let (src, dst) = tables;
        // Firebird 5 has neither DROP TABLE IF EXISTS (RECREATE drops the
        // table first) nor a VALUES list of more than one row.
        let firebird = cfg.driver == "firebird";
        let drop = |t: &str| {
            if firebird {
                format!("DROP TABLE {t}")
            } else {
                format!("DROP TABLE IF EXISTS {t}")
            }
        };
        for t in [src, dst] {
            if firebird {
                run(
                    &mut s,
                    &format!("RECREATE TABLE {t} (id INT, v {text_type})"),
                )
                .await;
            } else {
                run(&mut s, &drop(t)).await;
                run(&mut s, &format!("CREATE TABLE {t} (id INT, v {text_type})")).await;
            }
        }
        for (i, v) in values.iter().enumerate() {
            let hex: String = v.bytes().map(|b| format!("{b:02x}")).collect();
            run(
                &mut s,
                &format!("INSERT INTO {src} (id, v) VALUES ({i}, {})", from_hex(&hex)),
            )
            .await;
        }
        let select = |t: &str| format!("SELECT id, v FROM {t} ORDER BY id");
        let read = run(&mut s, &select(src)).await.results.remove(0);
        let stored: Vec<_> = read.rows.iter().map(|r| r[1].clone()).collect();
        let want: Vec<_> = values.iter().map(|v| serde_json::json!(v)).collect();
        assert_eq!(
            stored, want,
            "{}: the source table holds the values",
            cfg.driver
        );

        let path = std::env::temp_dir().join(format!(
            "dbine-export-rt-{}-{}.sql",
            cfg.driver,
            std::process::id()
        ));
        let options = ExportOptions {
            format: dbine_core::export::Format::Sql,
            table: dst.into(),
            quote: quote.into(),
            rows_per_insert: 1000,
            source,
            ..Default::default()
        };
        // One INSERT for all the rows, or one per row where VALUES takes one.
        let batches: Vec<&[Vec<serde_json::Value>]> = if firebird {
            read.rows.chunks(1).collect()
        } else {
            vec![&read.rows[..]]
        };
        let mut script = String::new();
        for rows in batches {
            export_rows(&path, options.clone(), &read.columns, rows).unwrap();
            let one = std::fs::read_to_string(&path).unwrap();
            assert!(
                !one.contains('\\'),
                "{}: no backslash in the script: {one}",
                cfg.driver
            );
            // Without its `;` (Oracle's OCI refuses it).
            run(&mut s, one.trim_end().trim_end_matches(';')).await;
            script.push_str(&one);
        }
        let _ = std::fs::remove_file(&path);
        let back: Vec<_> = run(&mut s, &select(dst))
            .await
            .results
            .remove(0)
            .rows
            .iter()
            .map(|r| r[1].clone())
            .collect();
        for t in [src, dst] {
            run(&mut s, &drop(t)).await;
        }
        for (got, want) in back.iter().zip(&want) {
            eprintln!("{} {source:?}: {want} -> {got}", cfg.driver);
        }
        assert_eq!(
            back, want,
            "{}: the script gives back the values exactly:\n{script}",
            cfg.driver
        );
    }

    fn tricky(extra: &[String]) -> Vec<String> {
        TRICKY
            .iter()
            .map(|v| v.to_string())
            .chain(extra.iter().cloned())
            .collect()
    }

    /// A multi-database run's grid keeps 5,000 rows per database; its export
    /// writes every row of every database, each after its database's name.
    /// SQLite files stand for the connection's databases (no server needed).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_multi_database_export_writes_every_row_of_every_database() {
        let dir = std::env::temp_dir().join(format!("dbine-export-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sqlite = driver("sqlite");
        let mut dbs = Vec::new();
        for (name, cols) in [
            ("uno", "id, name"),
            ("dos", "id, name"),
            ("tres", "id, other"),
        ] {
            let cfg = ConnectionConfig {
                driver: "sqlite".into(),
                host: dir.join(format!("{name}.db")).to_string_lossy().into(),
                ..Default::default()
            };
            let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
            run(&mut s, &format!("CREATE TABLE t ({cols})")).await;
            run(
                &mut s,
                &format!(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 6000) \
                     INSERT INTO t SELECT i, '{name}' || i FROM n"
                ),
            )
            .await;
            dbs.push((name, s));
        }
        // The script as the multi-database run ran it on each database.
        async fn export(
            dbs: &mut [(&str, Box<dyn dbine_driver::Session>)],
            sqlite: &dyn Driver,
            sql: &str,
            set: usize,
            base: bool,
            file: &std::path::Path,
        ) -> (Option<String>, u64) {
            let opts = ExportOptions {
                format: dbine_core::export::Format::Csv,
                ..Default::default()
            };
            let sink = Arc::new(Mutex::new(DatabasesSink::new(
                Exporter::new(file, 0, opts),
                set,
                base,
            )));
            let mut problem = None;
            for (name, s) in dbs.iter_mut() {
                sink.lock().unwrap().next_database(name);
                let mut out = QueryOutcome {
                    sink: Some(RowSinkRef(sink.clone())),
                    ..Default::default()
                };
                let never = std::sync::atomic::AtomicBool::new(false);
                run_script_on(&mut **s, sqlite, sql, usize::MAX, false, &never, &mut out)
                    .await
                    .unwrap();
                out.sink = None;
                let g = sink.lock().unwrap();
                problem = g
                    .problem
                    .clone()
                    .or_else(|| (!g.found()).then(|| format!("{name}: no result")));
                if problem.is_some() {
                    break;
                }
            }
            let rows = sink.lock().unwrap().finish().unwrap();
            (problem, rows)
        }

        // The merged grid: every row of both databases, `base` first.
        let file = dir.join("merged.csv");
        let (problem, rows) = export(
            &mut dbs[..2],
            sqlite,
            "SELECT id, name FROM t ORDER BY id",
            0,
            true,
            &file,
        )
        .await;
        assert_eq!(problem, None);
        assert_eq!(rows, 12_000);
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 12_001);
        assert_eq!(lines[0], "base,id,name");
        assert_eq!(lines[1], "uno,1,uno1");
        assert_eq!(lines[6000], "uno,6000,uno6000");
        assert_eq!(lines[6001], "dos,1,dos1");
        assert_eq!(lines[12_000], "dos,6000,dos6000");

        // One database's second result set: no `base`, only that set.
        let file = dir.join("second.csv");
        let sql = "SELECT count(*) AS n FROM t; SELECT id, name FROM t WHERE id > 5990 ORDER BY id";
        let (problem, rows) = export(&mut dbs[1..2], sqlite, sql, 1, false, &file).await;
        assert_eq!(problem, None);
        assert_eq!(rows, 10);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().next(), Some("id,name"));
        assert_eq!(text.lines().nth(1), Some("5991,dos5991"));

        // A database whose result no longer has the first one's columns stops it.
        let file = dir.join("changed.csv");
        let (problem, _) = export(&mut dbs, sqlite, "SELECT * FROM t", 0, true, &file).await;
        assert_eq!(
            problem.as_deref(),
            Some(format!("en la base tres: {COLUMNS_CHANGED}").as_str())
        );

        drop(dbs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No server needed: a SQLite file.
    #[tokio::test(flavor = "multi_thread")]
    async fn sql_export_round_trips_on_sqlite() {
        let path = std::env::temp_dir().join(format!("dbine-export-rt-{}.db", std::process::id()));
        let cfg = ConnectionConfig {
            driver: "sqlite".into(),
            host: path.to_string_lossy().into(),
            ..Default::default()
        };
        let values = tricky(&["a\0b".into(), "ab\\".repeat(3000)]);
        sql_export_round_trip(
            cfg,
            "TEXT",
            |h| format!("CAST(X'{h}' AS TEXT)"),
            "double",
            ("rt_src", "rt_dst"),
            &values,
        )
        .await;
        let _ = std::fs::remove_file(&path);
    }

    /// `DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres`
    /// `cargo test -p dbine --lib sql_export_round_trips -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_postgres() {
        let Some(mut cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
            panic!("DBINE_TEST_POSTGRES_URL not set");
        };
        cfg.read_only = false;
        cfg.database = "postgres".into();
        // PostgreSQL text can't hold NUL.
        let values = tricky(&["ab\\".repeat(3000), "ñandú \\ €".into()]);
        sql_export_round_trip(
            cfg,
            "text",
            |h| format!("convert_from(decode('{h}', 'hex'), 'UTF8')"),
            "double",
            ("dbine_rt_src", "dbine_rt_dst"),
            &values,
        )
        .await;
    }

    /// `DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013'`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_sqlserver() {
        let Some(mut cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
            panic!("DBINE_TEST_SQLSERVER_URL not set");
        };
        cfg.read_only = false;
        cfg.database = "tempdb".into();
        cfg.options.insert("auth".into(), "sql".into());
        // Past 8000 characters: CONCAT has to return varchar(max).
        let values = tricky(&["a\0b".into(), "ab\\".repeat(3000)]);
        sql_export_round_trip(
            cfg,
            "varchar(max)",
            |h| format!("CAST(0x{h} AS varchar(max))"),
            "bracket",
            ("dbo.dbine_rt_src", "dbo.dbine_rt_dst"),
            &values,
        )
        .await;
    }

    /// `DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_oracle() {
        let Ok(url) = std::env::var("DBINE_TEST_ORACLE_URL") else {
            panic!("DBINE_TEST_ORACLE_URL not set");
        };
        let rest = url
            .strip_prefix("oracle://")
            .expect("oracle://user:pass@host:port/service");
        let (cred, addr) = rest.split_once('@').unwrap();
        let (user, pass) = cred.split_once(':').unwrap();
        let (hostport, service) = addr.split_once('/').unwrap();
        let (host, port) = hostport.split_once(':').unwrap();
        let mut cfg = ConnectionConfig {
            driver: "oracle".into(),
            host: host.into(),
            port: port.parse().unwrap(),
            username: Some(user.into()),
            password: Some(pass.into()),
            ..Default::default()
        };
        cfg.options.insert("service".into(), service.into());
        // Oracle reads '' as NULL: the empty string can't round-trip there
        // by any script. A literal (the hex that stores the value) stops at 4000
        // characters.
        let values: Vec<String> = tricky(&["a\0b".into(), "ab\\".repeat(600)])
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect();
        sql_export_round_trip(
            cfg,
            "VARCHAR2(4000)",
            |h| format!("UTL_RAW.CAST_TO_VARCHAR2(HEXTORAW('{h}'))"),
            "double",
            ("DBINE_RT_SRC", "DBINE_RT_DST"),
            &values,
        )
        .await;
    }

    /// `DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_firebird() {
        let Ok(url) = std::env::var("DBINE_TEST_FIREBIRD_URL") else {
            panic!("DBINE_TEST_FIREBIRD_URL not set");
        };
        let rest = url
            .strip_prefix("firebird://")
            .expect("firebird://user:pass@host:port/path");
        let (cred, addr) = rest.split_once('@').unwrap();
        let (user, pass) = cred.split_once(':').unwrap();
        let (hostport, path) = addr.split_once('/').unwrap();
        let (host, port) = hostport.split_once(':').unwrap();
        let cfg = ConnectionConfig {
            driver: "firebird".into(),
            host: host.into(),
            port: port.parse().unwrap(),
            database: path.into(),
            username: Some(user.into()),
            password: Some(pass.into()),
            ..Default::default()
        };
        // Firebird keeps '' apart from NULL and stores NUL. A UTF8 VARCHAR
        // holds 8191 characters at most.
        let values = tricky(&["a\0b".into(), "ñandú \\ €".into(), "ab\\".repeat(2700)]);
        let text = "VARCHAR(8191) CHARACTER SET UTF8";
        sql_export_round_trip(
            cfg,
            text,
            |h| format!("CAST(_UTF8 x'{h}' AS VARCHAR(8191) CHARACTER SET UTF8)"),
            "double",
            ("DBINE_RT_SRC", "DBINE_RT_DST"),
            &values,
        )
        .await;
    }

    /// `DBINE_TEST_TRINO_URL=http://localhost:25180` (the memory catalog).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_trino() {
        let Ok(url) = std::env::var("DBINE_TEST_TRINO_URL") else {
            panic!("DBINE_TEST_TRINO_URL not set");
        };
        let url = reqwest::Url::parse(&url).expect("URL");
        let cfg = ConnectionConfig {
            driver: "trino".into(),
            host: url.host_str().unwrap().into(),
            port: url.port().unwrap_or(8080),
            username: Some("dbine".into()),
            database: "memory".into(),
            ..Default::default()
        };
        let values = tricky(&["a\0b".into(), "ñandú \\ €".into(), "ab\\".repeat(3000)]);
        sql_export_round_trip(
            cfg,
            "varchar",
            |h| format!("from_utf8(from_hex('{h}'))"),
            "double",
            ("memory.default.dbine_rt_src", "memory.default.dbine_rt_dst"),
            &values,
        )
        .await;
    }
}

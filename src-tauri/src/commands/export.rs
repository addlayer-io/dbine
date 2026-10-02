use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::export::{export_rows, ExportOptions, Exporter};
use dbine_driver::sql::leading_keyword;
use dbine_driver::{ConnectionConfig, Driver, Language, QueryOutcome, ResultColumn, RowSinkRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Deserialize)]
pub struct ExportRowsArgs {
    pub path: String,
    pub options: ExportOptions,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Serialize)]
pub struct ExportResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

/// Export the rows the grid already has.
#[tauri::command(rename_all = "camelCase")]
pub async fn export_rows_to_file(args: ExportRowsArgs) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(&args.path);
    let rows = tokio::task::spawn_blocking(move || export_rows(&path, args.options, &args.columns, &args.rows))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
        .map_err(|e| CommandError::Internal(format!("no se pudo escribir el archivo: {e}")))?;
    Ok(ExportResult { rows, elapsed_ms: started.elapsed().as_millis() as u64 })
}

#[derive(Deserialize)]
pub struct ExportQueryArgs {
    /// Chosen by the UI; progress events carry it and `cancel_query` takes
    /// `export:<id>` to stop it.
    pub export_id: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    /// Which result set of the script to write.
    pub result_index: usize,
    pub path: String,
    pub options: ExportOptions,
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
const AUTH_KEYS: &[&str] = &["auth", "auth_mode", "authentication", "authenticator", "auth_method"];
/// Words in a sign-in method that mean a person takes part (a browser, a
/// device code, MFA, a prompt…): a second login for an estimate would ask
/// them again.
const INTERACTIVE: &[&str] = &["interactive", "browser", "device", "mfa", "sso", "prompt", "okta"];

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
    let agent_tunnel = crate::tunnels::enabled(cfg) && cfg.option("ssh.auth") == Some("agent");
    if interactive || agent_tunnel || !driver.supports_explain() {
        return false;
    }
    if driver.info().language != Language::Sql {
        return !sql.trim().is_empty();
    }
    match driver.split_script(sql).as_slice() {
        [one] if one.error.is_none() && one.repeat <= 1 => {
            matches!(leading_keyword(&one.text, &driver.script_dialect()).as_deref(), Some("select" | "with" | "values" | "table" | "from"))
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
async fn estimate_rows(state: &AppState, key: &str, connection_id: &str, database: &str, sql: &str) -> Option<u64> {
    let deadline = tokio::time::Instant::now() + ESTIMATE_TIMEOUT;
    let opened = tokio::time::timeout_at(deadline, state.dedicated_session(key, connection_id, database, true)).await;
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
        [p] => p.root.est_rows.filter(|n| n.is_finite() && *n >= 1.0).map(|n| n.round() as u64),
        _ => None,
    }
}

/// Run the script again and stream the chosen result set to the file, all
/// rows, without holding them in memory. The session is read-only whatever
/// the connection says: an export never writes to the database (SQL
/// engines refuse writing statements; the others enforce read-only in
/// their drivers).
#[tauri::command(rename_all = "camelCase")]
pub async fn export_query_to_file(
    app: AppHandle,
    state: State<'_, AppState>,
    args: ExportQueryArgs,
) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let key = format!("export:{}", args.export_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;

    let path = PathBuf::from(&args.path);
    let written = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(UNKNOWN));
    let known = |total: &AtomicU64| Some(total.load(Ordering::Relaxed)).filter(|t| *t != UNKNOWN);

    // The estimate comes in while the export runs; its event carries the
    // rows written so far so the count never goes back.
    let estimate_key = format!("export-estimate:{}", args.export_id);
    let allowed = match (state.store.get_connection(&args.connection_id), driver_of(&state, &args.connection_id)) {
        (Ok(Some(saved)), Ok(driver)) => estimate_allowed(&saved.config, driver.as_ref(), &args.sql),
        _ => false,
    };
    let estimator = allowed.then(|| {
        let (app, key, id) = (app.clone(), estimate_key.clone(), args.export_id.clone());
        let (conn, db, sql) = (args.connection_id.clone(), args.database.clone(), args.sql.clone());
        let (written, total) = (written.clone(), total.clone());
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let est = estimate_rows(&state, &key, &conn, &db, &sql).await;
            if let Some(n) = est {
                total.store(n, Ordering::Relaxed);
                let rows = written.load(Ordering::Relaxed);
                let _ = app.emit("export-progress", Progress { id, rows, total: known(&total) });
            }
        })
    });

    let id = args.export_id.clone();
    let emitter = app.clone();
    let (written_cb, total_cb) = (written.clone(), total.clone());
    let mut last_emit: Option<Instant> = None;
    let exporter = Arc::new(Mutex::new(
        Exporter::new(&path, args.result_index, args.options).on_progress(move |rows| {
            written_cb.store(rows, Ordering::Relaxed);
            if last_emit.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
                return;
            }
            last_emit = Some(Instant::now());
            let _ = emitter.emit("export-progress", Progress { id: id.clone(), rows, total: known(&total_cb) });
        }),
    ));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(exporter.clone())), ..Default::default() };

    let finished = {
        let mut s = entry.session.lock().await;
        tokio::select! {
            r = s.execute(&args.sql, usize::MAX, &mut out) => Some(r),
            _ = entry.cancel.notified() => None,
        }
    };
    state.sessions.remove(&key);
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
            .or_else(|| (out.results.len() <= args.result_index).then(|| "la consulta no devolvió ese resultado".to_string()))
            .or_else(|| out.results[args.result_index].columns.is_empty().then(|| "ese resultado no tiene filas para exportar".to_string())),
    };
    let rows = exporter.lock().map_err(|_| CommandError::Internal("exportador".into()))?.finish();
    if let Some(msg) = failure {
        // Don't leave half a file behind.
        let _ = std::fs::remove_file(&path);
        return Err(CommandError::Sql(msg));
    }
    let rows = rows.map_err(|e| CommandError::Internal(format!("no se pudo cerrar el archivo: {e}")))?;
    // The final count from `finish` may have fallen inside the throttle window.
    let _ = app.emit("export-progress", Progress { id: args.export_id.clone(), rows, total: known(&total) });
    Ok(ExportResult { rows, elapsed_ms: started.elapsed().as_millis() as u64 })
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
        dbine_drivers::find(id).unwrap_or_else(|| panic!("driver {id} not built in")).as_ref()
    }

    fn with(driver: &str, opts: &[(&str, &str)]) -> ConnectionConfig {
        let mut c = ConnectionConfig { driver: driver.into(), ..Default::default() };
        for (k, v) in opts {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn estimates_only_plain_queries() {
        let (pg, cfg) = (driver("postgres"), with("postgres", &[]));
        assert!(estimate_allowed(&cfg, pg, "SELECT * FROM t"));
        assert!(estimate_allowed(&cfg, pg, "-- rows\nWITH x AS (SELECT 1) SELECT * FROM x;"));
        assert!(!estimate_allowed(&cfg, pg, "SELECT 1; SELECT 2"));
        assert!(!estimate_allowed(&cfg, pg, "CALL p()"));
        assert!(!estimate_allowed(&cfg, pg, "DO $$ BEGIN END $$"));
        assert!(!estimate_allowed(&cfg, pg, "   "));
        let (ms, cfg) = (driver("sqlserver"), with("sqlserver", &[("auth", "sql")]));
        assert!(estimate_allowed(&cfg, ms, "SELECT TOP 10 * FROM t"));
        assert!(!estimate_allowed(&cfg, ms, "EXEC dbo.p"));
        assert!(!estimate_allowed(&cfg, ms, "SELECT 1\nGO\nSELECT 2"));
        assert!(!estimate_allowed(&cfg, ms, "SELECT 1\nGO 3"));
    }

    #[test]
    fn no_second_login_that_could_ask_the_user() {
        let ms = driver("sqlserver");
        for auth in ["sql", "entra_password", "entra_sp", "entra_token"] {
            assert!(estimate_allowed(&with("sqlserver", &[("auth", auth)]), ms, "SELECT 1"), "{auth}");
        }
        for auth in ["entra_interactive", "ActiveDirectoryInteractive", "externalbrowser", "device_code", "entra_mfa", "sso"] {
            assert!(!estimate_allowed(&with("sqlserver", &[("auth", auth)]), ms, "SELECT 1"), "{auth}");
        }
        let pg = driver("postgres");
        assert!(!estimate_allowed(&with("postgres", &[("authenticator", "externalbrowser")]), pg, "SELECT 1"));
        let ssh = |auth: &str| with("postgres", &[("ssh.enabled", "true"), ("ssh.auth", auth)]);
        assert!(estimate_allowed(&ssh("password"), pg, "SELECT 1"));
        assert!(estimate_allowed(&ssh("key"), pg, "SELECT 1"));
        assert!(!estimate_allowed(&ssh("agent"), pg, "SELECT 1"));
        // The agent only matters while the tunnel is on.
        assert!(estimate_allowed(&with("postgres", &[("ssh.enabled", "false"), ("ssh.auth", "agent")]), pg, "SELECT 1"));
    }

    /// The export's row estimate on real servers (read-only session, as the
    /// export opens it): `DBINE_TEST_SQLSERVER_URL`, `DBINE_TEST_POSTGRES_URL`.
    /// `cargo test -p dbine --lib export::tests -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn estimates_the_rows_of_a_query() {
        let cases = [
            ("sqlserver", "DBINE_TEST_SQLSERVER_URL", "SELECT TOP 5000 a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b"),
            ("postgres", "DBINE_TEST_POSTGRES_URL", "SELECT g FROM generate_series(1, 5000) g"),
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
}

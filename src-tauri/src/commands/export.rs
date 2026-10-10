use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::export::{export_rows, ExportOptions, Exporter, SourceStrings};
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
pub async fn export_rows_to_file(state: State<'_, AppState>, args: ExportRowsArgs) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(&args.path);
    let mut options = args.options;
    // String literals of an SQL export follow the source engine; an unknown
    // source (a multi-database grid, a connection gone) gets the form that
    // ends in the same place on every engine.
    options.source = match args.connection_id.as_deref().map(|id| driver_of(&state, id)) {
        Some(Ok(d)) => SourceStrings::of(d.as_ref()),
        _ => SourceStrings::Unknown,
    };
    let rows = tokio::task::spawn_blocking(move || export_rows(&path, options, &args.columns, &args.rows))
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
    // String literals of an SQL export follow the source engine.
    let mut options = args.options;
    options.source = driver_of(&state, &args.connection_id).map_or(SourceStrings::Unknown, |d| SourceStrings::of(d.as_ref()));
    let exporter = Arc::new(Mutex::new(
        Exporter::new(&path, args.result_index, options).on_progress(move |rows| {
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
        for d in dbine_drivers::all().iter().filter(|d| d.info().language == Language::Sql) {
            let id = d.info().id;
            let want = known.get(id).unwrap_or_else(|| panic!("{id}: SQL driver without a SourceStrings classification here"));
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
        s.execute(sql, 10_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(out.error.is_none(), "{sql}: {:?}", out.error);
        out
    }

    /// Stores `values` in a table (through hex, which no string escaping
    /// touches), reads them back, exports them as an SQL script with the
    /// source set to the connection's driver, runs the script on the same
    /// engine into a second table and checks every value came back exactly.
    async fn sql_export_round_trip(cfg: ConnectionConfig, text_type: &str, from_hex: fn(&str) -> String, quote: &str, tables: (&str, &str), values: &[String]) {
        let d = dbine_drivers::find(&cfg.driver).unwrap();
        let source = SourceStrings::of(d.as_ref());
        let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
        let (src, dst) = tables;
        // Firebird 5 has neither DROP TABLE IF EXISTS (RECREATE drops the
        // table first) nor a VALUES list of more than one row.
        let firebird = cfg.driver == "firebird";
        let drop = |t: &str| if firebird { format!("DROP TABLE {t}") } else { format!("DROP TABLE IF EXISTS {t}") };
        for t in [src, dst] {
            if firebird {
                run(&mut s, &format!("RECREATE TABLE {t} (id INT, v {text_type})")).await;
            } else {
                run(&mut s, &drop(t)).await;
                run(&mut s, &format!("CREATE TABLE {t} (id INT, v {text_type})")).await;
            }
        }
        for (i, v) in values.iter().enumerate() {
            let hex: String = v.bytes().map(|b| format!("{b:02x}")).collect();
            run(&mut s, &format!("INSERT INTO {src} (id, v) VALUES ({i}, {})", from_hex(&hex))).await;
        }
        let select = |t: &str| format!("SELECT id, v FROM {t} ORDER BY id");
        let read = run(&mut s, &select(src)).await.results.remove(0);
        let stored: Vec<_> = read.rows.iter().map(|r| r[1].clone()).collect();
        let want: Vec<_> = values.iter().map(|v| serde_json::json!(v)).collect();
        assert_eq!(stored, want, "{}: the source table holds the values", cfg.driver);

        let path = std::env::temp_dir().join(format!("dbine-export-rt-{}-{}.sql", cfg.driver, std::process::id()));
        let options = ExportOptions {
            format: dbine_core::export::Format::Sql,
            table: dst.into(),
            quote: quote.into(),
            rows_per_insert: 1000,
            source,
            ..Default::default()
        };
        // One INSERT for all the rows, or one per row where VALUES takes one.
        let batches: Vec<&[Vec<serde_json::Value>]> = if firebird { read.rows.chunks(1).collect() } else { vec![&read.rows[..]] };
        let mut script = String::new();
        for rows in batches {
            export_rows(&path, options.clone(), &read.columns, rows).unwrap();
            let one = std::fs::read_to_string(&path).unwrap();
            assert!(!one.contains('\\'), "{}: no backslash in the script: {one}", cfg.driver);
            // Without its `;` (Oracle's OCI refuses it).
            run(&mut s, one.trim_end().trim_end_matches(';')).await;
            script.push_str(&one);
        }
        let _ = std::fs::remove_file(&path);
        let back: Vec<_> = run(&mut s, &select(dst)).await.results.remove(0).rows.iter().map(|r| r[1].clone()).collect();
        for t in [src, dst] {
            run(&mut s, &drop(t)).await;
        }
        for (got, want) in back.iter().zip(&want) {
            eprintln!("{} {source:?}: {want} -> {got}", cfg.driver);
        }
        assert_eq!(back, want, "{}: the script gives back the values exactly:\n{script}", cfg.driver);
    }

    fn tricky(extra: &[String]) -> Vec<String> {
        TRICKY.iter().map(|v| v.to_string()).chain(extra.iter().cloned()).collect()
    }

    /// No server needed: a SQLite file.
    #[tokio::test(flavor = "multi_thread")]
    async fn sql_export_round_trips_on_sqlite() {
        let path = std::env::temp_dir().join(format!("dbine-export-rt-{}.db", std::process::id()));
        let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
        let values = tricky(&["a\0b".into(), "ab\\".repeat(3000)]);
        sql_export_round_trip(cfg, "TEXT", |h| format!("CAST(X'{h}' AS TEXT)"), "double", ("rt_src", "rt_dst"), &values).await;
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
        sql_export_round_trip(cfg, "text", |h| format!("convert_from(decode('{h}', 'hex'), 'UTF8')"), "double", ("dbine_rt_src", "dbine_rt_dst"), &values).await;
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
        sql_export_round_trip(cfg, "varchar(max)", |h| format!("CAST(0x{h} AS varchar(max))"), "bracket", ("dbo.dbine_rt_src", "dbo.dbine_rt_dst"), &values).await;
    }

    /// `DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_oracle() {
        let Ok(url) = std::env::var("DBINE_TEST_ORACLE_URL") else {
            panic!("DBINE_TEST_ORACLE_URL not set");
        };
        let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
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
        let values: Vec<String> = tricky(&["a\0b".into(), "ab\\".repeat(600)]).into_iter().filter(|v| !v.is_empty()).collect();
        sql_export_round_trip(cfg, "VARCHAR2(4000)", |h| format!("UTL_RAW.CAST_TO_VARCHAR2(HEXTORAW('{h}'))"), "double", ("DBINE_RT_SRC", "DBINE_RT_DST"), &values).await;
    }

    /// `DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sql_export_round_trips_on_firebird() {
        let Ok(url) = std::env::var("DBINE_TEST_FIREBIRD_URL") else {
            panic!("DBINE_TEST_FIREBIRD_URL not set");
        };
        let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
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
        sql_export_round_trip(cfg, text, |h| format!("CAST(_UTF8 x'{h}' AS VARCHAR(8191) CHARACTER SET UTF8)"), "double", ("DBINE_RT_SRC", "DBINE_RT_DST"), &values).await;
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
        sql_export_round_trip(cfg, "varchar", |h| format!("from_utf8(from_hex('{h}'))"), "double", ("memory.default.dbine_rt_src", "memory.default.dbine_rt_dst"), &values).await;
    }
}

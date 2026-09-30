//! The profiler ([`dbine_driver::profiler`]) per variant.
//!
//! - SQL Server (and Managed Instance) and Azure SQL Database: complete,
//!   from an Extended Events session the profiler creates
//!   (`dbine_profiler_<spid>_<login time>`, `ON SERVER`, or `ON DATABASE` in
//!   Azure SQL Database) with `sql_batch_completed`, `rpc_completed` and
//!   `error_reported`, filtered to the database and leaving out the
//!   profiler's own session. Stop drops it. On a server the events go to
//!   `event_file` files in the server's log folder, read on from the last
//!   file and offset seen: a ring buffer turns over in a fraction of a
//!   second on a busy server and the events are gone before the next poll.
//!   Azure SQL Database (whose event files must live in Blob Storage) keeps
//!   the ring buffer. Without the
//!   permission to create it (ALTER ANY EVENT SESSION, or ALTER ANY
//!   DATABASE EVENT SESSION), or on read-only connections, it samples the
//!   DMVs instead.
//! - Fabric Data Warehouse: no Extended Events; sampled from the DMVs
//!   (Query Insights keeps a history, but minutes late).
//! - Babelfish: sampled from PostgreSQL's `pg_stat_activity`, scoped to
//!   the T-SQL database through `sys.dm_exec_sessions`.

use crate::monitor::f;
use crate::variant::Variant;
use crate::{err, text, SqlServerSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::HashMap;
use std::time::Instant;
use tiberius::Row;

pub(crate) enum State {
    Sampled { sampler: Sampler, sql: String },
    /// An Extended Events session read past event `after` (its
    /// `event_sequence`); `errors` holds each session's last error, which
    /// XE reports before the batch that failed completes.
    Events {
        name: String,
        database_scoped: bool,
        after: u64,
        errors: HashMap<i64, String>,
        /// The event files' path without `_<n>.xel` (`None`: ring buffer).
        files: Option<String>,
        /// Where the last read of the files ended (file name, offset).
        cursor: Option<(String, i64)>,
    },
}

/// Current UTC time on the server, in the statements' format.
const NOW_UTC: &str = "SELECT CONVERT(varchar(23), SYSUTCDATETIME(), 121)";

/// A local server time (`datetime`) as UTC text.
fn utc(col: &str) -> String {
    format!("CONVERT(varchar(23), DATEADD(minute, -DATEPART(TZOFFSET, SYSDATETIMEOFFSET()), {col}), 121)")
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

async fn exec(s: &mut SqlServerSession, sql: &str) -> Result<()> {
    s.client.simple_query(sql).await.map_err(err)?.into_results().await.map_err(err)?;
    Ok(())
}

pub(crate) async fn start(s: &mut SqlServerSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let info = s
        .rows("SELECT CAST(SERVERPROPERTY('EngineEdition') AS int), CAST(SERVERPROPERTY('Babelfish') AS int)", &[])
        .await?;
    let edition = info.first().and_then(|r| f(r, 0)).unwrap_or(0.0) as i64;
    let babelfish = s.variant == Variant::Babelfish || info.first().and_then(|r| f(r, 1)) == Some(1.0);
    let since = s.rows(NOW_UTC, &[]).await?.first().and_then(|r| text(r, 0)).unwrap_or_default();
    let db = if opts.database.is_empty() { None } else { Some(quote(&opts.database)) };

    if babelfish {
        let started = ProfilerStarted::new(ProfilerMode::Sampled, "pg_stat_activity (Babelfish)");
        return Ok((State::Sampled { sampler: Sampler::new(since), sql: babelfish_sql(db.as_deref()) }, started));
    }
    // EngineEdition: 5 Azure SQL Database, 12 SQL database in Fabric,
    // 6 Synapse dedicated pool, 11 Synapse serverless / Fabric warehouse.
    let warehouse = s.variant == Variant::Fabric || matches!(edition, 6 | 11);
    let database_scoped = matches!(edition, 5 | 12);
    let sampled = |note: &str| {
        (
            State::Sampled { sampler: Sampler::new(since.clone()), sql: sample_sql(db.as_deref()) },
            ProfilerStarted::new(ProfilerMode::Sampled, "sys.dm_exec_requests / sys.dm_exec_sessions")
                .units(Some("páginas"), Some("páginas"))
                .note(note.to_string()),
        )
    };
    if warehouse {
        return Ok(sampled("El almacén no tiene Extended Events: se muestrean las consultas en curso y la última de cada sesión."));
    }
    if !opts.change_server {
        return Ok(sampled(
            "Conexión de solo lectura: sin crear una sesión de Extended Events solo se muestrean las consultas en curso \
             y la última de cada sesión; las muy rápidas pueden no verse.",
        ));
    }
    match create_session(s, db.as_deref(), database_scoped).await {
        Ok((name, files)) => {
            let scope = if database_scoped { "de la base" } else { "del servidor" };
            let mut started = ProfilerStarted::new(ProfilerMode::Complete, "Extended Events")
                .units(Some("páginas"), Some("páginas"))
                .change(format!("Sesión de Extended Events {scope} «{name}» (se elimina al detener)"));
            if let Some(f) = &files {
                started = started.change(format!(
                    "sus archivos {f}_*.xel, de hasta 60 MB en total (se borran al detener; si el servidor no lo permite, quedan en su carpeta de logs)"
                ));
            }
            let state = State::Events { name, database_scoped, after: 0, errors: HashMap::new(), files, cursor: None };
            Ok((state, started))
        }
        Err(Error::Query(e)) if denied(&e) => {
            let perm = if database_scoped { "ALTER ANY DATABASE EVENT SESSION" } else { "ALTER ANY EVENT SESSION" };
            Ok(sampled(&format!(
                "Sin el permiso {perm} no se puede crear una sesión de Extended Events: se muestrean las consultas en curso \
                 y la última de cada sesión; las muy rápidas pueden no verse."
            )))
        }
        Err(e) => Err(e),
    }
}

fn denied(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("permission") || m.contains("permiso")
}

/// Create and start the event session; returns its name and, when it
/// writes to files, their path without the extension.
async fn create_session(s: &mut SqlServerSession, db: Option<&str>, database_scoped: bool) -> Result<(String, Option<String>)> {
    let on = if database_scoped { "DATABASE" } else { "SERVER" };
    // The name carries the owner's session and login time, so a later start
    // can tell the sessions of a profiler that died from live ones.
    let stamp = "N'dbine_profiler_' + CAST(session_id AS nvarchar(10)) + N'_' + CONVERT(nvarchar(8), login_time, 112) \
                 + REPLACE(CONVERT(nvarchar(12), login_time, 114), N':', N'')";
    let name = s
        .rows(&format!("SELECT {stamp} FROM sys.dm_exec_sessions WHERE session_id = @@SPID"), &[])
        .await?
        .first()
        .and_then(|r| text(r, 0))
        .ok_or_else(|| Error::Query("no se pudo leer la sesión propia".into()))?;
    // The server's log folder, next to its error log (event files go there).
    let log_dir = if database_scoped {
        None
    } else {
        s.rows("SELECT CAST(SERVERPROPERTY('ErrorLogFileName') AS nvarchar(4000))", &[])
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| text(r, 0)))
            .and_then(|log| log.rfind(['/', '\\']).map(|i| log[..=i].to_string()))
    };
    let catalog = if database_scoped { "sys.database_event_sessions" } else { "sys.server_event_sessions" };
    let leftovers = s
        .rows(
            &format!(
                "SELECT e.name FROM {catalog} e WHERE e.name LIKE N'dbine[_]profiler[_]%' \
                 AND NOT EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE {stamp} = e.name)"
            ),
            &[],
        )
        .await
        .unwrap_or_default();
    for r in &leftovers {
        if let Some(old) = text(r, 0) {
            if let Err(e) = exec(s, &format!("DROP EVENT SESSION [{old}] ON {on}")).await {
                tracing::debug!("sqlserver profiler: dropping {old}: {e}");
            }
            if let Some(dir) = &log_dir {
                delete_files(s, &format!("{dir}{old}")).await;
            }
        }
    }

    let mut filter = format!("sqlserver.session_id <> {} AND sqlserver.is_system = 0", s.spid);
    if let (Some(db), false) = (db, database_scoped) {
        let id = s
            .rows(&format!("SELECT CAST(DB_ID(N'{db}') AS int)"), &[])
            .await?
            .first()
            .and_then(|r| f(r, 0))
            .ok_or_else(|| Error::Query(format!("no existe la base {db}")))?;
        filter.push_str(&format!(" AND sqlserver.database_id = {id}"));
    }
    let actions = "ACTION(package0.event_sequence, sqlserver.database_name, sqlserver.username, sqlserver.client_hostname, \
                   sqlserver.client_app_name, sqlserver.session_id)";
    let files = log_dir.map(|dir| format!("{dir}{name}"));
    let target = match &files {
        Some(f) => format!(
            "package0.event_file(SET filename = N'{}.xel', max_file_size = (20), max_rollover_files = (3))",
            quote(f)
        ),
        None => "package0.ring_buffer(SET max_memory = 4096)".to_string(),
    };
    let sql = format!(
        "CREATE EVENT SESSION [{name}] ON {on}
         ADD EVENT sqlserver.sql_batch_completed({actions} WHERE {filter}),
         ADD EVENT sqlserver.rpc_completed({actions} WHERE {filter}),
         ADD EVENT sqlserver.error_reported(ACTION(package0.event_sequence, sqlserver.session_id) WHERE severity >= 11 AND {filter})
         ADD TARGET {target}
         WITH (MAX_DISPATCH_LATENCY = 1 SECONDS, EVENT_RETENTION_MODE = ALLOW_SINGLE_EVENT_LOSS, STARTUP_STATE = OFF)"
    );
    exec(s, &sql).await?;
    if let Err(e) = exec(s, &format!("ALTER EVENT SESSION [{name}] ON {on} STATE = START")).await {
        let _ = exec(s, &format!("DROP EVENT SESSION [{name}] ON {on}")).await;
        return Err(e);
    }
    Ok((name, files))
}

pub(crate) async fn poll(s: &mut SqlServerSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    match state {
        State::Sampled { sampler, sql } => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let rows = s.rows(sql, &[]).await?;
                out.extend(sampler.feed(rows.iter().map(sample).collect()));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
        State::Events { name, database_scoped, after, errors, files, cursor } => {
            let sql = match files {
                Some(f) => file_events_sql(f, cursor.as_ref(), *after),
                None => events_sql(name, *database_scoped, *after),
            };
            let rows = s.rows(&sql, &[]).await?;
            if files.is_some() {
                if let Some(last) = rows.last() {
                    if let (Some(file), Some(offset)) = (text(last, 16), last.try_get::<i64, _>(17).ok().flatten()) {
                        *cursor = Some((file, offset));
                    }
                }
            }
            let own = s.spid as i64;
            let mut out = Vec::new();
            for r in &rows {
                let seq = r.try_get::<i64, _>(0).ok().flatten().unwrap_or(0) as u64;
                *after = (*after).max(seq);
                let session = r.try_get::<i64, _>(15).ok().flatten().unwrap_or(0);
                if session == own {
                    continue;
                }
                let event = text(r, 1).unwrap_or_default();
                if event == "error_reported" {
                    if let Some(m) = text(r, 10) {
                        errors.insert(session, m);
                    }
                    continue;
                }
                let error = errors.remove(&session).or_else(|| text(r, 8).filter(|v| v != "OK"));
                let body = text(r, 9).unwrap_or_default();
                if body.trim().is_empty() {
                    continue;
                }
                let us = f(r, 3).unwrap_or(0.0);
                let time = text(r, 2).map(|ts| start_of(&ts, us)).unwrap_or_default();
                let n = |i| r.try_get::<i64, _>(i).ok().flatten();
                let client = text(r, 13).filter(|h| !h.is_empty());
                let application = text(r, 14).filter(|a| !a.is_empty());
                out.push(ProfiledStatement {
                    time,
                    duration_ms: Some(us / 1000.0),
                    text: body,
                    database: text(r, 11),
                    user: text(r, 12),
                    client,
                    rows: n(4).map(|v| v as u64),
                    error,
                    application,
                    cpu_ms: f(r, 5).map(us_to_ms),
                    reads: n(6).map(|v| v.max(0) as u64),
                    writes: n(7).map(|v| v.max(0) as u64),
                    ..Default::default()
                });
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
    }
}

pub(crate) async fn stop(s: &mut SqlServerSession, state: State) -> Result<()> {
    if let State::Events { name, database_scoped, files, .. } = state {
        let (catalog, on) =
            if database_scoped { ("sys.database_event_sessions", "DATABASE") } else { ("sys.server_event_sessions", "SERVER") };
        exec(s, &format!("IF EXISTS (SELECT 1 FROM {catalog} WHERE name = N'{name}') DROP EVENT SESSION [{name}] ON {on}")).await?;
        if let Some(f) = files {
            delete_files(s, &f).await;
        }
    }
    Ok(())
}

/// Delete an event session's files (`xp_delete_files`: SQL Server 2019+,
/// sysadmin). Where it isn't allowed they stay; that's said at start.
async fn delete_files(s: &mut SqlServerSession, files: &str) {
    if let Err(e) = exec(s, &format!("EXEC sys.xp_delete_files N'{}_*.xel'", quote(files))).await {
        tracing::debug!("sqlserver profiler: deleting {files}_*.xel: {e}");
    }
}

/// When a statement began: XE stamps the end, in UTC
/// (`2024-01-31T10:00:00.123Z`).
fn start_of(end: &str, duration_us: f64) -> String {
    match chrono::DateTime::parse_from_rfc3339(end) {
        Ok(t) => (t - chrono::Duration::microseconds(duration_us as i64)).format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        Err(_) => end.replace('T', " ").trim_end_matches('Z').chars().take(23).collect(),
    }
}

/// The events after `after`, as columns, from the ring buffer's XML.
fn events_sql(name: &str, database_scoped: bool, after: u64) -> String {
    let (sessions, targets) = if database_scoped {
        ("sys.dm_xe_database_sessions", "sys.dm_xe_database_session_targets")
    } else {
        ("sys.dm_xe_sessions", "sys.dm_xe_session_targets")
    };
    let data = |n: &str, ty: &str| format!("e.value('(data[@name=\"{n}\"]/value)[1]', '{ty}')");
    let action = |n: &str, ty: &str| format!("e.value('(action[@name=\"{n}\"]/value)[1]', '{ty}')");
    format!(
        "SELECT {seq} AS seq, e.value('@name', 'nvarchar(60)'), e.value('@timestamp', 'nvarchar(40)'),
                {dur}, {rows}, {cpu}, {reads}, {writes},
                e.value('(data[@name=\"result\"]/text)[1]', 'nvarchar(20)'),
                COALESCE({batch}, {stmt}), {msg},
                {db}, {usr}, {host}, {app}, {sid}
           FROM (SELECT CAST(t.target_data AS xml) AS d
                   FROM {sessions} s JOIN {targets} t ON t.event_session_address = s.address
                  WHERE s.name = N'{name}' AND t.target_name = N'ring_buffer') x
          CROSS APPLY x.d.nodes('/RingBufferTarget/event[(action[@name=\"event_sequence\"]/value)[1] > {after}]') n(e)
          ORDER BY seq",
        seq = action("event_sequence", "bigint"),
        dur = data("duration", "float"),
        rows = data("row_count", "bigint"),
        cpu = data("cpu_time", "float"),
        reads = data("logical_reads", "bigint"),
        writes = data("writes", "bigint"),
        batch = data("batch_text", "nvarchar(max)"),
        stmt = data("statement", "nvarchar(max)"),
        msg = data("message", "nvarchar(max)"),
        db = action("database_name", "nvarchar(256)"),
        usr = action("username", "nvarchar(256)"),
        host = action("client_hostname", "nvarchar(256)"),
        app = action("client_app_name", "nvarchar(256)"),
        sid = action("session_id", "bigint"),
    )
}

/// The events in the files after `cursor` (the whole files at first), as
/// `events_sql`'s columns plus the file and offset of each.
fn file_events_sql(files: &str, cursor: Option<&(String, i64)>, after: u64) -> String {
    let (file, offset) = match cursor {
        Some((f, o)) => (format!("N'{}'", quote(f)), o.to_string()),
        None => ("NULL".to_string(), "NULL".to_string()),
    };
    let ring = events_sql("", false, after);
    // Same columns, read from the files instead of the ring buffer.
    let columns = &ring[..ring.find("FROM (SELECT CAST(t.target_data").unwrap_or(0)];
    format!(
        "{columns}, x.f, x.o
           FROM (SELECT CAST(event_data AS xml) AS d, file_name AS f, file_offset AS o
                   FROM sys.fn_xe_file_target_read_file(N'{pattern}_*.xel', NULL, {file}, {offset})) x
          CROSS APPLY x.d.nodes('/event[(action[@name=\"event_sequence\"]/value)[1] > {after}]') n(e)
          ORDER BY x.f, x.o, seq",
        pattern = quote(files),
    )
}

/// Each user session's running request, or its last one when idle, except
/// this session's.
fn sample_sql(db: Option<&str>) -> String {
    let scope = db.map(|d| format!(" AND COALESCE(r.database_id, s.database_id) = DB_ID(N'{d}')")).unwrap_or_default();
    format!(
        "SELECT CAST(s.session_id AS nvarchar(10)), {started}, t.text,
                CAST(CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END AS int),
                CAST(CASE WHEN r.session_id IS NULL THEN DATEDIFF(millisecond, s.last_request_start_time, s.last_request_end_time)
                          ELSE r.total_elapsed_time END AS float),
                DB_NAME(COALESCE(r.database_id, s.database_id)), s.login_name,
                NULLIF(s.host_name, N''),
                CAST(r.row_count AS bigint),
                N'espera ' + r.wait_type,
                NULLIF(s.program_name, N''),
                CAST(r.cpu_time AS float), CAST(r.logical_reads AS bigint), CAST(r.writes AS bigint)
           FROM sys.dm_exec_sessions s
           LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
          OUTER APPLY (SELECT TOP (1) c.most_recent_sql_handle FROM sys.dm_exec_connections c WHERE c.session_id = s.session_id) c
          OUTER APPLY sys.dm_exec_sql_text(COALESCE(r.sql_handle, c.most_recent_sql_handle)) t
          WHERE s.is_user_process = 1 AND s.session_id <> @@SPID{scope}",
        started = utc("COALESCE(r.start_time, s.last_request_start_time)"),
    )
}

/// Babelfish: `pg_stat_activity` is PostgreSQL's, with one physical
/// database for every T-SQL one; `sys.dm_exec_sessions` knows which T-SQL
/// database each backend is in.
fn babelfish_sql(db: Option<&str>) -> String {
    let scope = db.map(|d| format!(" AND s.database_id = DB_ID(N'{d}')")).unwrap_or_default();
    format!(
        "SELECT CAST(a.pid AS varchar(12)),
                CAST(pg_catalog.to_char(pg_catalog.timezone(CAST('UTC' AS pg_catalog.text), a.query_start), 'YYYY-MM-DD HH24:MI:SS.MS') AS varchar(23)),
                CAST(a.query AS nvarchar(max)),
                CAST(CASE WHEN a.state = 'active' THEN 1 ELSE 0 END AS int),
                CAST(pg_catalog.date_part('epoch', CASE WHEN a.state = 'active' THEN pg_catalog.clock_timestamp() ELSE a.state_change END
                     - a.query_start) * 1000 AS float),
                DB_NAME(s.database_id), CAST(a.usename AS nvarchar(256)),
                CAST(COALESCE(CAST(a.client_addr AS varchar(64)), 'local') AS nvarchar(400)),
                CAST(NULL AS bigint), CAST(NULL AS nvarchar(10)),
                CAST(NULLIF(a.application_name, '') AS nvarchar(256)),
                CAST(NULL AS float), CAST(NULL AS bigint), CAST(NULL AS bigint)
           FROM pg_catalog.pg_stat_activity a
           JOIN sys.dm_exec_sessions s ON s.session_id = a.pid
          WHERE a.backend_type = 'client backend' AND a.pid <> @@SPID AND a.query_start IS NOT NULL{scope}"
    )
}

fn sample(r: &Row) -> Sample {
    Sample {
        session: text(r, 0).unwrap_or_default(),
        started: text(r, 1).unwrap_or_default(),
        text: text(r, 2).unwrap_or_default(),
        running: f(r, 3) == Some(1.0),
        duration_ms: f(r, 4).map(|ms| ms.max(0.0)),
        database: text(r, 5),
        user: text(r, 6),
        client: text(r, 7).filter(|c| !c.is_empty()),
        rows: r.try_get::<i64, _>(8).ok().flatten().map(|v| v as u64),
        detail: text(r, 9),
        application: text(r, 10).filter(|a| !a.is_empty()),
        // Only a running request has its own figures (the session's are
        // totals over all it ran).
        cpu_ms: f(r, 11),
        reads: r.try_get::<i64, _>(12).ok().flatten().map(|v| v.max(0) as u64),
        writes: r.try_get::<i64, _>(13).ok().flatten().map(|v| v.max(0) as u64),
        ..Default::default()
    }
}

/// Extended Events report `cpu_time` in microseconds.
fn us_to_ms(us: f64) -> f64 {
    us / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_start_before_they_end() {
        assert_eq!(start_of("2024-01-31T10:00:01.250Z", 500_000.0), "2024-01-31 10:00:00.750");
        assert_eq!(start_of("2024-01-31T10:00:01.2500000Z", 0.0), "2024-01-31 10:00:01.250");
    }

    #[test]
    fn events_read_cpu_reads_and_writes() {
        assert_eq!(us_to_ms(15_500.0), 15.5);
        let ring = events_sql("x", false, 0);
        for field in ["cpu_time", "logical_reads", "writes"] {
            assert!(ring.contains(&format!("data[@name=\"{field}\"]")), "{field}");
        }
        let sampled = sample_sql(None);
        assert!(sampled.contains("CAST(r.cpu_time AS float), CAST(r.logical_reads AS bigint), CAST(r.writes AS bigint)"));
        assert!(!sampled.contains("lecturas"));
    }
}

//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another session's statement ([`dbine_driver::Session::cancel_query`])
//! per variant.
//!
//! - PostgreSQL and the engines that keep its `pg_stat_activity` (Aurora,
//!   AlloyDB, Cloud SQL, Timescale, Yugabyte, EDB, Greenplum and its forks,
//!   openGauss…): one row per backend, `pg_blocking_pids()` from 9.6, and
//!   `pg_cancel_backend()` to cancel.
//! - CockroachDB: `crdb_internal.cluster_sessions` with its running
//!   statement from `cluster_queries` (or the `SHOW CLUSTER …` equivalents);
//!   ids are session ids, as `kill_session` takes, and cancelling looks up
//!   the session's query ids for `CANCEL QUERY`.
//! - Redshift: `stv_sessions` and `stv_recents` (provisioned) or
//!   `sys_session_history` and `sys_query_history` (Serverless);
//!   `pg_cancel_backend()`.
//! - Yellowbrick: `sys.session`, `sys.query` and `sys.lock`; `CANCEL
//!   <query_id>` for the session's queries.
//! - Materialize: `mz_sessions` (statements only show with statement
//!   logging, so none here); `pg_cancel_backend()` by connection id.
//! - RisingWave: `SHOW PROCESSLIST` (the frontend's sessions) and `KILL`,
//!   which cancels the statement and keeps the session.
//! - CrateDB: `sys.sessions` with its jobs from `sys.jobs`; `KILL` per job.
//! - H2: `information_schema.sessions`; `CANCEL_SESSION()`.
//! - Denodo: `GET_SESSIONS()`, with the running query; VQL has no way to
//!   cancel one.

use crate::catalog::cell;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, Result, ServerProcess};
use std::collections::HashSet;
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most: a server with thousands of connections still answers fast.
const MAX_ROWS: usize = 2000;

const RUNNING: &str = "en ejecución";
const IDLE: &str = "inactiva";
const IDLE_IN_TXN: &str = "inactiva en transacción";
const OWN: &str = "esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá";

/// Variants whose sessions answer `cancel_query` (all of them list).
pub(crate) fn cancel_supported(v: Variant) -> bool {
    cancel_unsupported_reason(v).is_none()
}

/// Why a variant can't cancel, in Spanish (for the error and the docs).
pub(crate) fn cancel_unsupported_reason(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Denodo => Some(
            "Denodo no cancela consultas por VQL: se cancelan desde Diagnostic & Monitoring Tool o por JMX",
        ),
        _ => None,
    }
}

fn text(r: &SimpleQueryRow, name: &str) -> Option<String> {
    cell(r, name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn ms(r: &SimpleQueryRow, name: &str) -> Option<u64> {
    cell(r, name).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn truthy(r: &SimpleQueryRow, name: &str) -> bool {
    matches!(cell(r, name).map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1"))
}

/// The statement's first word, upper-cased (`SELECT`, `INSERT`…).
fn command_of(sql: &str) -> Option<String> {
    sql.split_whitespace().next().map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_uppercase()).filter(|w| !w.is_empty())
}

/// A token unique to one listing query, so the engines without a "my
/// session id" function find their own session by its statement text.
fn nonce() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    format!("dbine_{:x}_{:x}", t, std::process::id())
}

fn first(rows: &[SimpleQueryRow]) -> Option<String> {
    rows.first().and_then(|r| r.get(0)).map(|v| v.trim().to_string())
}

/// The first row per id: an engine that joins sessions to their running
/// statements gives one row per statement.
fn dedup(list: Vec<ServerProcess>) -> Vec<ServerProcess> {
    let mut seen = HashSet::new();
    list.into_iter().filter(|p| !p.id.is_empty() && seen.insert(p.id.clone())).collect()
}

fn int_id(id: &str, engine: &str) -> Result<i64> {
    id.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión de {engine}")))
}

fn nothing_running(id: &str) -> Error {
    Error::Query(format!("la sesión {id} no está ejecutando nada (o tu usuario no ve sus consultas)"))
}

pub(crate) async fn processes(s: &PgSession) -> Result<Vec<ServerProcess>> {
    match s.variant {
        Variant::Cockroach => cockroach(s).await,
        Variant::Redshift => redshift(s).await,
        Variant::Yellowbrick => yellowbrick(s).await,
        Variant::Materialize => materialize(s).await,
        Variant::RisingWave => risingwave(s).await,
        Variant::CrateDb => cratedb(s).await,
        Variant::H2 => h2(s).await,
        Variant::Denodo => denodo(s).await,
        _ => postgres(s).await,
    }
}

pub(crate) async fn cancel(s: &PgSession, id: &str) -> Result<()> {
    if let Some(why) = cancel_unsupported_reason(s.variant) {
        return Err(Error::Unsupported(why.into()));
    }
    let id = id.trim();
    match s.variant {
        Variant::Cockroach => cockroach_cancel(s, id).await,
        Variant::Yellowbrick => yellowbrick_cancel(s, id).await,
        Variant::RisingWave => risingwave_cancel(s, id).await,
        Variant::CrateDb => cratedb_cancel(s, id).await,
        Variant::H2 => {
            let n = int_id(id, "H2")?;
            if first(&s.text("SELECT CAST(SESSION_ID() AS VARCHAR)").await?).as_deref() == Some(id) {
                return Err(Error::Query(OWN.into()));
            }
            let r = first(&s.text(&format!("SELECT CASE WHEN CANCEL_SESSION({n}) THEN 't' ELSE 'f' END")).await?);
            cancelled(r, id)
        }
        Variant::Redshift | Variant::Materialize => {
            let n = int_id(id, if s.variant == Variant::Redshift { "Redshift" } else { "Materialize" })?;
            if first(&s.text("SELECT pg_backend_pid()").await?).as_deref() == Some(id) {
                return Err(Error::Query(OWN.into()));
            }
            // Redshift's returns nothing; Materialize's says whether the
            // connection exists (and it's a side-effecting function: no
            // casts or clauses around it).
            let r = first(&s.text(&format!("SELECT pg_cancel_backend({n})")).await?);
            if s.variant == Variant::Materialize && !matches!(r.as_deref(), Some("t" | "true")) {
                return Err(Error::Query(format!("no existe la sesión {id}")));
            }
            Ok(())
        }
        _ => postgres_cancel(s, id).await,
    }
}

fn cancelled(r: Option<String>, id: &str) -> Result<()> {
    match r.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("t" | "true" | "1") => Ok(()),
        _ => Err(Error::Query(format!(
            "no se pudo cancelar la consulta de la sesión {id}: no existe, no está ejecutando nada o tu usuario no tiene permiso"
        ))),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL

async fn postgres(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let ver = s.version;
    // Before 10 only client backends show up, except in openGauss, whose
    // background threads have no client port.
    let system = if ver >= 100000 { "coalesce(backend_type, '') <> 'client backend'" } else { "client_port IS NULL" };
    let wait = if ver >= 90600 { "coalesce(wait_event_type || ': ' || wait_event, '')" } else { "CASE WHEN waiting THEN 'Lock' ELSE '' END" };
    let blocked_by = if ver >= 90600 { "(pg_blocking_pids(pid))[1]::text" } else { "NULL::text" };
    let rows = s
        .text_within(
            &format!(
                "SELECT pid::text AS id, state, usename,
                        coalesce(host(client_addr), CASE WHEN client_port = -1 THEN 'socket local' END) AS host,
                        application_name, datname,
                        CASE WHEN state = 'active' THEN upper(split_part(ltrim(query), ' ', 1)) END AS command,
                        round(extract(epoch FROM now() - CASE WHEN state = 'active' THEN query_start ELSE coalesce(state_change, backend_start) END) * 1000) AS elapsed,
                        {wait} AS wait, {blocked_by} AS blocked_by,
                        left(query, {MAX_TEXT}) AS query,
                        (state = 'active') AS active, ({system}) AS system, (pid = pg_backend_pid()) AS own
                 FROM pg_stat_activity
                 ORDER BY (state = 'active') DESC, pid LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let active = truthy(r, "active");
            ServerProcess {
                id: text(r, "id").unwrap_or_default(),
                status: text(r, "state"),
                active,
                system: truthy(r, "system"),
                own: truthy(r, "own"),
                user: text(r, "usename"),
                host: text(r, "host"),
                program: text(r, "application_name"),
                database: text(r, "datname"),
                command: text(r, "command"),
                elapsed_ms: ms(r, "elapsed"),
                wait: text(r, "wait"),
                blocked_by: text(r, "blocked_by"),
                // An idle session's text is its last statement, not one running.
                sql: text(r, "query").filter(|_| active || text(r, "state").is_some_and(|st| st.starts_with("idle in transaction"))),
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect())
}

async fn postgres_cancel(s: &PgSession, id: &str) -> Result<()> {
    // Backend pids are int4; openGauss's are thread ids (int8).
    let n: i64 = if s.variant == Variant::OpenGauss { id.parse().ok() } else { id.parse::<i32>().ok().map(i64::from) }
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión (PID)")))?;
    if first(&s.text("SELECT pg_backend_pid()::text").await?).as_deref() == Some(id) {
        return Err(Error::Query(OWN.into()));
    }
    let sql = if s.variant == Variant::OpenGauss { format!("SELECT pg_cancel_backend({n})::text") } else { format!("SELECT pg_cancel_backend({n}::int)::text") };
    cancelled(first(&s.text(&sql).await?), id)
}

// ---------------------------------------------------------------------------
// CockroachDB

fn crdb_id(id: &str) -> Result<&str> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Query(format!("«{id}» no es un id de sesión de CockroachDB")));
    }
    Ok(id)
}

/// Sessions and their running statement, cluster-wide. `crdb_internal`
/// adds the open transaction and the statement's database; without it
/// (v25+ closes it to non-admins) the `SHOW CLUSTER …` statements give the
/// rest.
async fn cockroach(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let _ = s.text_within("SET allow_unsafe_internals = true", QUERY_LIMIT).await;
    let sql = |sessions: &str, queries: &str, txn: &str, db: &str| {
        format!(
            "SELECT s.session_id AS id, s.user_name, s.client_address, s.application_name, {db} AS db,
                    CASE WHEN q.query_id IS NULL THEN 'f' ELSE 't' END AS active,
                    CASE WHEN {txn} THEN 't' ELSE 'f' END AS in_txn,
                    CASE WHEN s.session_id = (SELECT session_id FROM [SHOW session_id]) THEN 't' ELSE 'f' END AS own,
                    (extract(epoch FROM now() - q.start) * 1000)::INT8 AS elapsed,
                    substring(coalesce(q.query, s.last_active_query), 1, {MAX_TEXT}) AS query
             FROM {sessions} AS s LEFT JOIN {queries} AS q ON q.session_id = s.session_id
             WHERE s.status <> 'CLOSED'
             ORDER BY q.query_id IS NULL, q.start, s.session_start LIMIT {MAX_ROWS}"
        )
    };
    let rows = match s
        .text_within(
            &sql("crdb_internal.cluster_sessions", "crdb_internal.cluster_queries", "coalesce(s.kv_txn, '') <> ''", "q.database"),
            QUERY_LIMIT,
        )
        .await
    {
        Ok(rows) => rows,
        Err(_) => s.text_within(&sql("[SHOW CLUSTER SESSIONS]", "[SHOW CLUSTER STATEMENTS]", "false", "NULL::STRING"), QUERY_LIMIT).await?,
    };
    Ok(dedup(
        rows.iter()
            .map(|r| {
                let active = truthy(r, "active");
                let in_txn = !active && truthy(r, "in_txn");
                let sql = text(r, "query").filter(|_| active || in_txn);
                ServerProcess {
                    id: text(r, "id").unwrap_or_default(),
                    status: Some(if active { RUNNING } else if in_txn { IDLE_IN_TXN } else { IDLE }.into()),
                    active,
                    // The internal executor's sessions are named `$ internal-…`.
                    system: text(r, "application_name").is_some_and(|a| a.starts_with("$ internal")),
                    own: truthy(r, "own"),
                    user: text(r, "user_name"),
                    host: text(r, "client_address"),
                    program: text(r, "application_name"),
                    database: text(r, "db"),
                    command: sql.as_deref().filter(|_| active).and_then(command_of),
                    elapsed_ms: if active { ms(r, "elapsed") } else { None },
                    sql,
                    ..Default::default()
                }
            })
            .collect(),
    ))
}

async fn cockroach_cancel(s: &PgSession, id: &str) -> Result<()> {
    let id = crdb_id(id)?;
    if first(&s.text("SHOW session_id").await?).as_deref() == Some(id) {
        return Err(Error::Query(OWN.into()));
    }
    let rows = s.text_within(&format!("SELECT query_id FROM [SHOW CLUSTER STATEMENTS] WHERE session_id = '{id}'"), QUERY_LIMIT).await?;
    let ids: Vec<String> = rows.iter().filter_map(|r| text(r, "query_id")).filter(|q| q.chars().all(|c| c.is_ascii_hexdigit())).collect();
    if ids.is_empty() {
        return Err(nothing_running(id));
    }
    for q in ids {
        s.text(&format!("CANCEL QUERY IF EXISTS '{q}'")).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Redshift

/// Provisioned clusters keep sessions in `stv_sessions` and the running
/// statements in `stv_recents`; Serverless has only the `sys_*` views.
async fn redshift(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let provisioned = s
        .text_within(
            &format!(
                "SELECT s.process::varchar AS id, trim(s.user_name) AS usr, trim(s.db_name) AS db,
                        CASE WHEN r.pid IS NULL THEN 'f' ELSE 't' END AS active,
                        CASE WHEN s.process = pg_backend_pid() THEN 't' ELSE 'f' END AS own,
                        datediff(ms, r.starttime, getdate()) AS elapsed, trim(r.query) AS query
                 FROM stv_sessions s LEFT JOIN stv_recents r ON r.pid = s.process AND r.status = 'Running'
                 ORDER BY 4 DESC, s.process LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await;
    let rows = match provisioned {
        Ok(rows) => rows,
        Err(_) => {
            s.text_within(
                &format!(
                    "SELECT s.session_id::varchar AS id, trim(s.user_id) AS usr, trim(s.database_name) AS db,
                            CASE WHEN q.query_id IS NULL THEN 'f' ELSE 't' END AS active,
                            CASE WHEN s.session_id = pg_backend_pid() THEN 't' ELSE 'f' END AS own,
                            datediff(ms, q.start_time, getdate()) AS elapsed, left(q.query_text, {MAX_TEXT}) AS query
                     FROM sys_session_history s LEFT JOIN sys_query_history q ON q.session_id = s.session_id AND q.status = 'running'
                     WHERE s.status = 'active'
                     ORDER BY 4 DESC, s.session_id LIMIT {MAX_ROWS}"
                ),
                QUERY_LIMIT,
            )
            .await?
        }
    };
    Ok(dedup(
        rows.iter()
            .map(|r| {
                let active = truthy(r, "active");
                let sql = text(r, "query").filter(|_| active);
                ServerProcess {
                    id: text(r, "id").unwrap_or_default(),
                    status: Some(if active { RUNNING } else { IDLE }.into()),
                    active,
                    // rdsdb is the user AWS manages the cluster with.
                    system: text(r, "usr").as_deref() == Some("rdsdb"),
                    own: truthy(r, "own"),
                    user: text(r, "usr"),
                    database: text(r, "db"),
                    command: sql.as_deref().and_then(command_of),
                    elapsed_ms: if active { ms(r, "elapsed") } else { None },
                    sql,
                    ..Default::default()
                }
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Yellowbrick

async fn yellowbrick(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let tag = nonce();
    let rows = s
        .text_within(
            &format!(
                "SELECT '{tag}' AS tag, s.session_id::varchar AS id, s.state, u.name AS usr, d.name AS db,
                        s.client_ip_address AS host, s.application_name AS app,
                        q.query_id::varchar AS qid, q.state AS qstate, q.blocked,
                        CASE WHEN q.query_id IS NULL THEN datediff(ms, s.last_statement, now()) ELSE q.total_ms END AS elapsed,
                        left(q.query_text, {MAX_TEXT}) AS query, b.blocker::varchar AS blocked_by
                 FROM sys.session s
                 LEFT JOIN sys.user u ON u.user_id = s.user_id
                 LEFT JOIN sys.database d ON d.database_id = s.database_id
                 LEFT JOIN sys.query q ON q.session_id = s.session_id
                 LEFT JOIN (SELECT session_id, min(blocked_by_session_id) AS blocker FROM sys.lock
                            WHERE blocked_by_session_id IS NOT NULL AND NOT is_granted GROUP BY session_id) b
                        ON b.session_id = s.session_id
                 ORDER BY q.query_id IS NULL, s.session_id LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await?;
    // This very query names its session.
    let own: Option<String> = rows.iter().find(|r| text(r, "query").is_some_and(|q| q.contains(&tag))).and_then(|r| text(r, "id"));
    Ok(dedup(
        rows.iter()
            .map(|r| {
                let active = text(r, "qid").is_some();
                let sql = text(r, "query").filter(|_| active);
                let id = text(r, "id").unwrap_or_default();
                ServerProcess {
                    status: if active { text(r, "qstate").or_else(|| Some(RUNNING.into())) } else { text(r, "state") },
                    active,
                    // Internal applications are named yb-… (yb-lime, …).
                    system: text(r, "app").is_some_and(|a| a.starts_with("yb-")),
                    own: own.as_deref() == Some(id.as_str()),
                    user: text(r, "usr"),
                    host: text(r, "host"),
                    program: text(r, "app"),
                    database: text(r, "db"),
                    command: sql.as_deref().and_then(command_of),
                    elapsed_ms: ms(r, "elapsed"),
                    wait: text(r, "blocked").filter(|_| active),
                    blocked_by: text(r, "blocked_by"),
                    sql,
                    id,
                    ..Default::default()
                }
            })
            .collect(),
    ))
}

async fn yellowbrick_cancel(s: &PgSession, id: &str) -> Result<()> {
    let n = int_id(id, "Yellowbrick")?;
    let tag = nonce();
    let rows = s
        .text_within(
            &format!(
                "SELECT '{tag}' AS tag, query_id::varchar AS qid, session_id::varchar AS sid, left(query_text, 200) AS query
                 FROM sys.query"
            ),
            QUERY_LIMIT,
        )
        .await?;
    if rows.iter().any(|r| text(r, "sid").as_deref() == Some(id) && text(r, "query").is_some_and(|q| q.contains(&tag))) {
        return Err(Error::Query(OWN.into()));
    }
    let ids: Vec<i64> = rows.iter().filter(|r| text(r, "sid").and_then(|v| v.parse().ok()) == Some(n)).filter_map(|r| text(r, "qid")?.parse().ok()).collect();
    if ids.is_empty() {
        return Err(nothing_running(id));
    }
    for q in ids {
        s.text(&format!("CANCEL {q}")).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Materialize

/// Sessions only: running statements are recorded only with statement
/// logging, sampled and read through a cluster, too heavy to poll.
async fn materialize(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let rows = s
        .text_within(
            &format!(
                "SELECT s.connection_id::text AS id, coalesce(r.name, s.role_id) AS usr, s.client_ip::text AS host,
                        CASE WHEN s.connection_id = pg_backend_pid() THEN 't' ELSE 'f' END AS own
                 FROM mz_internal.mz_sessions s LEFT JOIN mz_catalog.mz_roles r ON r.id = s.role_id
                 ORDER BY s.connected_at LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await?;
    Ok(dedup(
        rows.iter()
            .map(|r| ServerProcess {
                id: text(r, "id").unwrap_or_default(),
                own: truthy(r, "own"),
                // mz_system, mz_support: Materialize's own roles.
                system: text(r, "usr").is_some_and(|u| u.starts_with("mz_")),
                user: text(r, "usr"),
                host: text(r, "host"),
                ..Default::default()
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// RisingWave

/// `SHOW PROCESSLIST` lists the sessions of the frontend this connection
/// is on; ids are `<worker>:<session>`, the session part being that
/// session's `pg_backend_pid()`.
async fn risingwave(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let own = first(&s.text_within("SELECT pg_backend_pid()::varchar", QUERY_LIMIT).await?);
    let rows = s.text_within("SHOW PROCESSLIST", QUERY_LIMIT).await?;
    let mut list: Vec<ServerProcess> = rows
        .iter()
        .take(MAX_ROWS)
        .map(|r| {
            let id = text(r, "Id").unwrap_or_default();
            let sql = text(r, "Info").map(|q| q.chars().take(MAX_TEXT).collect::<String>());
            let active = sql.is_some();
            ServerProcess {
                status: Some(if active { RUNNING } else { IDLE }.into()),
                active,
                own: own.is_some() && id.rsplit_once(':').map(|(_, n)| n) == own.as_deref(),
                user: text(r, "User"),
                host: text(r, "Host"),
                database: text(r, "Database"),
                command: sql.as_deref().and_then(command_of),
                // "4114ms".
                elapsed_ms: text(r, "Time").and_then(|t| rw_ms(&t)).filter(|_| active),
                sql,
                id,
                ..Default::default()
            }
        })
        .collect();
    list.sort_by_key(|p| !p.active);
    Ok(dedup(list))
}

fn rw_ms(t: &str) -> Option<u64> {
    let t = t.trim();
    let (n, scale) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000.0)
    } else {
        (t, 1.0)
    };
    n.trim().parse::<f64>().ok().map(|v| (v * scale).max(0.0) as u64)
}

fn rw_id(id: &str) -> Result<&str> {
    match id.split_once(':') {
        Some((w, n)) if !w.is_empty() && !n.is_empty() && w.chars().chain(n.chars()).all(|c| c.is_ascii_digit()) => Ok(id),
        _ => Err(Error::Query(format!("«{id}» no es un id de sesión de RisingWave (worker:sesión)"))),
    }
}

async fn risingwave_cancel(s: &PgSession, id: &str) -> Result<()> {
    let id = rw_id(id)?;
    let own = first(&s.text("SELECT pg_backend_pid()::varchar").await?);
    if own.is_some() && id.rsplit_once(':').map(|(_, n)| n) == own.as_deref() {
        return Err(Error::Query(OWN.into()));
    }
    // KILL cancels the statement; the session stays open.
    s.text(&format!("KILL '{id}'")).await.map(|_| ())
}

// ---------------------------------------------------------------------------
// CrateDB

/// `sys.sessions` (5.x) with the jobs each one runs; older versions list
/// the jobs alone, by job id.
async fn cratedb(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let tag = nonce();
    let sessions = s
        .text_within(
            &format!(
                "SELECT '{tag}' AS tag, s.id, s.\"session_user\" AS usr, s.client_address AS host,
                        s.settings['application_name'] AS app, s.last_statement,
                        j.id AS job, substr(j.stmt, 1, {MAX_TEXT}) AS stmt,
                        extract(epoch FROM now() - j.started) * 1000 AS elapsed
                 FROM sys.sessions s LEFT JOIN sys.jobs j ON j.session_id = s.id
                 ORDER BY j.started NULLS LAST, s.id LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await;
    let (rows, by_job) = match sessions {
        Ok(rows) => (rows, false),
        Err(_) => (
            s.text_within(
                &format!(
                    "SELECT '{tag}' AS tag, id, id AS job, username AS usr, substr(stmt, 1, {MAX_TEXT}) AS stmt, stmt AS last_statement,
                            extract(epoch FROM now() - started) * 1000 AS elapsed
                     FROM sys.jobs ORDER BY started LIMIT {MAX_ROWS}"
                ),
                QUERY_LIMIT,
            )
            .await?,
            true,
        ),
    };
    Ok(dedup(
        rows.iter()
            .map(|r| {
                let active = text(r, "job").is_some();
                let sql = text(r, "stmt").filter(|_| active);
                ServerProcess {
                    id: text(r, "id").unwrap_or_default(),
                    status: Some(if active { RUNNING } else { IDLE }.into()),
                    active,
                    own: text(r, "last_statement").is_some_and(|q| q.contains(&tag)),
                    user: text(r, "usr"),
                    host: text(r, "host"),
                    program: text(r, "app"),
                    database: text(r, "db"),
                    command: sql.as_deref().and_then(command_of),
                    elapsed_ms: if active { ms(r, "elapsed") } else { None },
                    sql,
                    ..Default::default()
                }
            })
            .filter(|p| !by_job || p.active)
            .collect(),
    ))
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36 && id.chars().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

async fn cratedb_cancel(s: &PgSession, id: &str) -> Result<()> {
    let tag = nonce();
    let by_job = is_uuid(id);
    if !by_job {
        int_id(id, "CrateDB")?;
    }
    // Every job, this one included: it names DBine's own session.
    let rows = s.text_within(&format!("SELECT '{tag}' AS tag, id, session_id::text AS sid, stmt FROM sys.jobs"), QUERY_LIMIT).await?;
    let own = rows.iter().find(|r| text(r, "stmt").is_some_and(|q| q.contains(&tag)));
    let mine = |r: &SimpleQueryRow| if by_job { text(r, "id").as_deref() == Some(id) } else { text(r, "sid").as_deref() == Some(id) };
    if own.is_some_and(&mine) {
        return Err(Error::Query(OWN.into()));
    }
    let jobs: Vec<String> = rows.iter().filter(|r| mine(r)).filter_map(|r| text(r, "id")).filter(|j| is_uuid(j)).collect();
    if jobs.is_empty() {
        return Err(nothing_running(id));
    }
    for j in jobs {
        s.text(&format!("KILL '{j}'")).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// H2

async fn h2(s: &PgSession) -> Result<Vec<ServerProcess>> {
    let rows = s
        .text_within(
            &format!(
                "SELECT CAST(session_id AS VARCHAR) AS id, user_name AS usr, client_addr AS host, client_info AS app,
                        session_state AS state, CAST(blocker_id AS VARCHAR) AS blocker,
                        CASE WHEN executing_statement IS NULL THEN 'f' ELSE 't' END AS active,
                        CASE WHEN contains_uncommitted THEN 't' ELSE 'f' END AS uncommitted,
                        CASE WHEN session_id = SESSION_ID() THEN 't' ELSE 'f' END AS own,
                        DATEDIFF('MILLISECOND', COALESCE(executing_statement_start, sleep_since), CURRENT_TIMESTAMP) AS elapsed,
                        LEFT(executing_statement, {MAX_TEXT}) AS query
                 FROM information_schema.sessions
                 ORDER BY executing_statement IS NULL, session_id LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await?;
    Ok(dedup(
        rows.iter()
            .map(|r| {
                let active = truthy(r, "active");
                let sql = text(r, "query");
                ServerProcess {
                    id: text(r, "id").unwrap_or_default(),
                    status: text(r, "state"),
                    active,
                    own: truthy(r, "own"),
                    user: text(r, "usr"),
                    host: text(r, "host"),
                    program: text(r, "app"),
                    database: Some(s.database.clone()).filter(|d| !d.is_empty()),
                    command: sql.as_deref().filter(|_| active).and_then(command_of),
                    elapsed_ms: ms(r, "elapsed"),
                    wait: text(r, "blocker").map(|_| "Lock".into()),
                    blocked_by: text(r, "blocker"),
                    sql,
                    ..Default::default()
                }
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Denodo

/// `GET_SESSIONS()` (needs an administrator) with each session's running
/// query; there's no "my session" function, so DBine's own row is the one
/// running this very call.
async fn denodo(s: &PgSession) -> Result<Vec<ServerProcess>> {
    const SQL: &str = "SELECT * FROM GET_SESSIONS()";
    let rows = s.text_within(SQL, QUERY_LIMIT).await?;
    Ok(dedup(
        rows.iter()
            .take(MAX_ROWS)
            .map(|r| {
                let sql = text(r, "query_running").map(|q| q.chars().take(MAX_TEXT).collect::<String>());
                let active = sql.is_some();
                let queued = truthy(r, "query_running_queued");
                ServerProcess {
                    id: text(r, "session_id").unwrap_or_default(),
                    status: Some(if queued { "en cola" } else if active { RUNNING } else { IDLE }.into()),
                    active,
                    own: sql.as_deref() == Some(SQL),
                    user: text(r, "user_name"),
                    host: text(r, "client_ip"),
                    program: text(r, "user_agent").or_else(|| text(r, "access_interface")),
                    database: text(r, "database_name"),
                    command: sql.as_deref().and_then(command_of),
                    wait: queued.then(|| "Límite de consultas concurrentes".into()),
                    sql,
                    ..Default::default()
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_lists_and_only_denodo_cannot_cancel() {
        for v in Variant::ALL {
            assert_eq!(cancel_supported(v), v != Variant::Denodo, "{v:?}");
        }
    }

    #[test]
    fn ids_are_validated() {
        assert!(crdb_id("18db24970813f2000000000000000001").is_ok());
        assert!(crdb_id("1'; DROP").is_err());
        assert!(rw_id("20:3").is_ok());
        for bad in ["20", ":3", "20:", "20:3'", "a:1"] {
            assert!(rw_id(bad).is_err(), "{bad}");
        }
        assert!(is_uuid("f95844c0-1b15-4f9b-2b2c-512bd62ee48d"));
        assert!(!is_uuid("f95844c0-1b15-4f9b-2b2c-512bd62ee48'"));
        assert!(int_id("12", "H2").is_ok());
        assert!(int_id("0", "H2").is_err() && int_id("1;", "H2").is_err());
    }

    #[test]
    fn parsing_helpers() {
        assert_eq!(rw_ms("4114ms"), Some(4114));
        assert_eq!(rw_ms("2s"), Some(2000));
        assert_eq!(command_of("  select 1"), Some("SELECT".into()));
        assert_eq!(command_of("(SELECT 1)"), Some("SELECT".into()));
        assert_eq!(command_of(""), None);
    }
}

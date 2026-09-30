//! Blocking chains ([`dbine_driver::Session::blocking`]) and ending another
//! session ([`dbine_driver::Session::kill_session`]) per variant.
//!
//! Every variant builds the same two things: the waits (who waits on whom,
//! as edges) and what each session involved is doing, then [`assemble`]
//! turns them into one row per session: the waiters, and the sessions they
//! wait for (the heads), even idle ones holding a transaction open.
//!
//! - PostgreSQL 9.6+ and the engines that keep its lock manager:
//!   `pg_blocking_pids()` over `pg_stat_activity`.
//! - Older servers (openGauss) and Greenplum and its forks: `pg_locks`
//!   paired by lock and mode (on MPP, per segment and mapped back to the
//!   coordinator's session).
//! - YugabyteDB: `pg_locks.ybdetails` (DocDB's row locks and waiters).
//! - Redshift: `svv_transactions`; Yellowbrick: `sys.lock`; CockroachDB:
//!   `crdb_internal.cluster_locks`; H2: `information_schema.sessions`.

use crate::catalog::cell;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{BlockedSession, Error, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Longest a blocking query may run: it's polled like the monitor.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 4000;

/// Variants whose sessions answer `blocking` and `kill_session`.
pub(crate) fn supported(v: Variant) -> bool {
    unsupported_reason(v).is_none()
}

/// Why a variant has neither, in Spanish (for the error and the docs).
pub(crate) fn unsupported_reason(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Materialize => {
            Some("Materialize no tiene bloqueos entre sesiones: ordena lecturas y escrituras por marcas de tiempo, sin locks")
        }
        Variant::RisingWave => Some("RisingWave no tiene bloqueos entre sesiones: es una base de streaming sin transacciones de escritura interactivas"),
        Variant::CrateDb => Some("CrateDB no tiene transacciones ni bloqueos de filas: ninguna sesión espera a otra"),
        Variant::Denodo => Some("Denodo es una capa de virtualización: los bloqueos ocurren en las fuentes de datos, no en Denodo"),
        _ => None,
    }
}

/// One wait: `waiter` waits on `blocker`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Edge {
    waiter: String,
    blocker: String,
    /// What it waits on, as the engine says ("Lock: transactionid").
    wait: Option<String>,
    /// The locked object, when this source knows it.
    object: Option<String>,
    /// How long it has been waiting, when this source knows it.
    waited_ms: Option<u64>,
}

/// What a session in a chain is doing.
#[derive(Debug, Clone, Default)]
struct Info {
    user: Option<String>,
    client: Option<String>,
    database: Option<String>,
    /// Its state as the engine says it, for a head that isn't idle.
    state: Option<String>,
    /// Idle with a transaction open: the usual head of a chain.
    idle_in_txn: bool,
    /// Since it started waiting (or its statement started).
    wait_ms: Option<u64>,
    /// Since its transaction (or statement) started.
    txn_ms: Option<u64>,
    object: Option<String>,
    sql: Option<String>,
}

pub(crate) const IDLE_IN_TXN: &str = "inactiva con transacción abierta";

/// One row per session: every waiter (on its first blocker) and every
/// session waited on, in the order they first show up.
fn assemble(edges: &[Edge], info: &HashMap<String, Info>) -> Vec<BlockedSession> {
    let mut order: Vec<&str> = Vec::new();
    for e in edges {
        for id in [&e.waiter, &e.blocker] {
            if !order.contains(&id.as_str()) {
                order.push(id);
            }
        }
    }
    order
        .into_iter()
        .map(|id| {
            let i = info.get(id).cloned().unwrap_or_default();
            match edges.iter().find(|e| e.waiter == id) {
                Some(e) => BlockedSession {
                    id: id.to_string(),
                    blocked_by: Some(e.blocker.clone()),
                    user: i.user,
                    client: i.client,
                    database: i.database,
                    wait: e.wait.clone().or(i.state),
                    waited_ms: e.waited_ms.or(i.wait_ms),
                    object: e.object.clone().or(i.object),
                    sql: i.sql,
                },
                None => BlockedSession {
                    id: id.to_string(),
                    blocked_by: None,
                    user: i.user,
                    client: i.client,
                    database: i.database,
                    wait: if i.idle_in_txn { Some(IDLE_IN_TXN.into()) } else { i.state },
                    waited_ms: i.txn_ms.or(i.wait_ms),
                    object: None,
                    sql: i.sql,
                },
            }
        })
        .collect()
}

/// PostgreSQL's lock-mode conflict table: whether a request for `want`
/// waits on a granted `held`. Unknown modes are taken as conflicting.
fn conflicts(want: &str, held: &str) -> bool {
    const MODES: [&str; 8] = [
        "AccessShareLock",
        "RowShareLock",
        "RowExclusiveLock",
        "ShareUpdateExclusiveLock",
        "ShareLock",
        "ShareRowExclusiveLock",
        "ExclusiveLock",
        "AccessExclusiveLock",
    ];
    // Row i: the modes (1-based bits) that mode i+1 conflicts with.
    const TABLE: [u16; 8] = [
        1 << 8,
        1 << 7 | 1 << 8,
        1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
        1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
        1 << 3 | 1 << 4 | 1 << 6 | 1 << 7 | 1 << 8,
        1 << 3 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
        1 << 2 | 1 << 3 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
        0b1_1111_1110,
    ];
    let pos = |m: &str| MODES.iter().position(|x| x.eq_ignore_ascii_case(m.trim()));
    match (pos(want), pos(held)) {
        (Some(w), Some(h)) => TABLE[w] & (1 << (h + 1)) != 0,
        _ => true,
    }
}

/// A lock request or grant, keyed by what it locks.
struct LockRow {
    id: String,
    key: String,
    mode: String,
    granted: bool,
    wait: Option<String>,
    object: Option<String>,
}

/// Pair each waiting request with the granted locks it conflicts with.
fn pair_locks(locks: &[LockRow]) -> Vec<Edge> {
    let mut edges: Vec<Edge> = Vec::new();
    for w in locks.iter().filter(|l| !l.granted) {
        for h in locks.iter().filter(|h| h.granted && h.key == w.key && h.id != w.id && conflicts(&w.mode, &h.mode)) {
            if !edges.iter().any(|e| e.waiter == w.id && e.blocker == h.id) {
                edges.push(Edge {
                    waiter: w.id.clone(),
                    blocker: h.id.clone(),
                    wait: w.wait.clone(),
                    object: w.object.clone(),
                    waited_ms: None,
                });
            }
        }
    }
    edges
}

fn ms(r: &SimpleQueryRow, name: &str) -> Option<u64> {
    cell(r, name).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn text(r: &SimpleQueryRow, name: &str) -> Option<String> {
    cell(r, name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn truthy(v: Option<String>) -> bool {
    matches!(v.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("t" | "true" | "1"))
}

pub(crate) async fn blocking(s: &PgSession) -> Result<Vec<BlockedSession>> {
    if let Some(why) = unsupported_reason(s.variant) {
        return Err(Error::Unsupported(why.into()));
    }
    match s.variant {
        Variant::Cockroach => cockroach(s).await,
        Variant::Redshift => redshift(s).await,
        Variant::Yellowbrick => yellowbrick(s).await,
        Variant::H2 => h2(s).await,
        _ => postgres(s).await,
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL and its lock manager

/// `pg_blocking_pids()` (9.6+) sees the whole wait queue; Greenplum's
/// only sees the coordinator's, so MPP pairs `pg_locks` per segment.
fn has_blocking_pids(s: &PgSession) -> bool {
    s.version >= 90600 && s.variant != Variant::OpenGauss && !s.variant.mpp()
}

async fn postgres(s: &PgSession) -> Result<Vec<BlockedSession>> {
    let edges = if s.variant == Variant::Yugabyte {
        yugabyte_edges(s).await?
    } else if has_blocking_pids(s) {
        let rows = s
            .text_within(
                "SELECT a.pid::text AS waiter, b.pid::text AS blocker, a.wait_event_type || ': ' || a.wait_event AS wait
                 FROM pg_stat_activity a, unnest(pg_blocking_pids(a.pid)) WITH ORDINALITY AS b(pid, n)
                 WHERE a.wait_event_type = 'Lock'
                 ORDER BY a.pid, b.n",
                QUERY_LIMIT,
            )
            .await?;
        rows.iter()
            .filter_map(|r| {
                Some(Edge { waiter: text(r, "waiter")?, blocker: text(r, "blocker")?, wait: text(r, "wait"), ..Default::default() })
            })
            .collect()
    } else {
        pg_lock_pairs(s).await?
    };
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let info = pg_info(s, &edges).await?;
    Ok(assemble(&edges, &info))
}

/// YugabyteDB keeps row locks in DocDB, not in PostgreSQL's lock manager:
/// `pg_blocking_pids()` comes back empty and `pg_locks` has no pid. Its
/// `ybdetails` name the waiting transaction and the ones it waits for
/// (`blocked_by`); `pg_stat_activity.yb_backend_xid` maps transactions to
/// backends. A single-statement write waits without a transaction id: its
/// backend is found in Active Session History, waiting on conflicting
/// transactions in the same tablet.
async fn yugabyte_edges(s: &PgSession) -> Result<Vec<Edge>> {
    const FROM_XID: &str = "SELECT l.*, w.pid AS wpid FROM l JOIN pg_stat_activity w ON w.yb_backend_xid::text = l.txn";
    const FROM_ASH: &str = "SELECT l.*, a.pid AS wpid FROM l
         JOIN (SELECT DISTINCT pid, wait_event_aux FROM yb_active_session_history
               WHERE sample_time > now() - interval '3 seconds'
                 AND wait_event = 'ConflictResolution_WaitOnConflictingTxns' AND wait_event_aux <> '') a
           ON l.txn IS NULL AND l.tablet LIKE a.wait_event_aux || '%'";
    let sql = |waiters: &str| {
        format!(
            "WITH l AS (SELECT l.locktype, l.relation, l.waitstart,
                               l.ybdetails->>'transactionid' AS txn, l.ybdetails->>'tablet_id' AS tablet,
                               -- Guarded: the planner may run the array functions
                               -- below on rows the WHERE drops.
                               CASE WHEN jsonb_typeof(l.ybdetails->'blocked_by') = 'array' THEN l.ybdetails->'blocked_by' ELSE '[]' END AS blocked_by,
                               CASE WHEN jsonb_typeof(l.ybdetails->'keyrangedetails'->'cols') = 'array'
                                    THEN l.ybdetails->'keyrangedetails'->'cols' ELSE '[]' END AS cols
                        FROM pg_locks l
                        WHERE NOT l.granted AND jsonb_typeof(l.ybdetails->'blocked_by') = 'array'),
                  lw AS ({waiters})
             SELECT lw.wpid::text AS waiter, h.pid::text AS blocker, 'Lock: ' || max(lw.locktype) AS wait,
                    max(lw.relation::regclass::text
                        || coalesce(' (clave ' || (SELECT string_agg(c, ', ')
                                                   FROM jsonb_array_elements_text(lw.cols) c) || ')', '')) AS object,
                    round(extract(epoch FROM now() - min(lw.waitstart)) * 1000) AS wait_ms
             FROM lw
             CROSS JOIN LATERAL jsonb_array_elements_text(lw.blocked_by) AS b(txn)
             JOIN pg_stat_activity h ON h.yb_backend_xid::text = b.txn
             WHERE h.pid <> lw.wpid
             GROUP BY lw.wpid, h.pid
             ORDER BY lw.wpid, h.pid"
        )
    };
    // Active Session History may be off: then only the waiters with a
    // transaction id show.
    let rows = match s.text_within(&sql(&format!("{FROM_XID} UNION ALL {FROM_ASH}")), QUERY_LIMIT).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("yugabyte: blocking without ASH: {e}");
            s.text_within(&sql(FROM_XID), QUERY_LIMIT).await?
        }
    };
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(Edge {
                waiter: text(r, "waiter")?,
                blocker: text(r, "blocker")?,
                wait: text(r, "wait"),
                object: text(r, "object"),
                waited_ms: ms(r, "wait_ms"),
            })
        })
        .collect())
}

/// Waits from `pg_locks`: the requests not granted and the grants on the
/// same lock they conflict with. On MPP each segment has its own locks
/// (and backends): the key takes the segment and the id is the
/// coordinator's backend of the same distributed session.
async fn pg_lock_pairs(s: &PgSession) -> Result<Vec<Edge>> {
    let mpp = s.variant.mpp();
    let key = format!(
        "concat_ws('/', l.locktype, l.database, l.relation, l.page, l.tuple, l.virtualxid, l.transactionid, l.classid, l.objid, l.objsubid{})",
        if mpp { ", l.gp_segment_id" } else { "" }
    );
    let id = if mpp {
        "coalesce((SELECT min(a.pid) FROM pg_stat_activity a WHERE a.sess_id = l.mppsessionid), l.pid)::text"
    } else {
        "l.pid::text"
    };
    let sql = format!(
        "WITH k AS (SELECT {id} AS id, {key} AS key, l.mode, l.granted, l.locktype FROM pg_locks l WHERE l.pid IS NOT NULL)
         SELECT id, key, mode, CASE WHEN granted THEN 't' ELSE 'f' END AS granted, 'Lock: ' || locktype AS wait
         FROM k WHERE key IN (SELECT key FROM k WHERE NOT granted)
         ORDER BY id"
    );
    let rows = s.text_within(&sql, QUERY_LIMIT).await?;
    let locks: Vec<LockRow> = rows
        .iter()
        .filter_map(|r| {
            Some(LockRow {
                id: text(r, "id")?,
                key: text(r, "key")?,
                mode: text(r, "mode").unwrap_or_default(),
                granted: truthy(text(r, "granted")),
                wait: text(r, "wait"),
                object: None,
            })
        })
        .collect();
    Ok(pair_locks(&locks))
}

/// What each session in `edges` is doing, from `pg_stat_activity`; for a
/// waiter, also the relation (and row) it waits on.
async fn pg_info(s: &PgSession, edges: &[Edge]) -> Result<HashMap<String, Info>> {
    let ids: Vec<i64> = ids(edges).iter().filter_map(|i| i.parse().ok()).collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let wait_start = if s.version >= 140000 && has_blocking_pids(s) {
        "coalesce((SELECT min(l.waitstart) FROM pg_locks l WHERE l.pid = a.pid AND NOT l.granted), a.query_start)"
    } else {
        "a.query_start"
    };
    let sql = format!(
        "SELECT a.pid::text AS id, a.usename AS usr,
                concat_ws(' · ', coalesce(host(a.client_addr), CASE WHEN a.client_port = -1 THEN 'socket local' END),
                          nullif(a.application_name, '')) AS client,
                a.datname AS db, a.state AS state,
                round(extract(epoch FROM now() - {wait_start}) * 1000) AS wait_ms,
                round(extract(epoch FROM now() - coalesce(a.xact_start, a.query_start)) * 1000) AS txn_ms,
                left(a.query, {MAX_TEXT}) AS sql,
                (SELECT CASE WHEN l.database = (SELECT d.oid FROM pg_database d WHERE d.datname = current_database())
                             THEN l.relation::regclass::text ELSE 'oid ' || l.relation::text END
                        || CASE WHEN l.locktype = 'tuple' THEN ' (fila ' || l.page || ',' || l.tuple || ')' ELSE '' END
                 FROM pg_locks l
                 WHERE l.pid = a.pid AND l.relation IS NOT NULL AND (NOT l.granted OR l.locktype = 'tuple')
                 ORDER BY l.granted LIMIT 1) AS object
         FROM pg_stat_activity a WHERE a.pid IN ({list})"
    );
    let rows = s.text_within(&sql, QUERY_LIMIT).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let state = text(r, "state");
            Some((
                text(r, "id")?,
                Info {
                    user: text(r, "usr"),
                    client: text(r, "client"),
                    database: text(r, "db"),
                    idle_in_txn: state.as_deref().is_some_and(|st| st.starts_with("idle in transaction")),
                    state,
                    wait_ms: ms(r, "wait_ms"),
                    txn_ms: ms(r, "txn_ms"),
                    object: text(r, "object"),
                    sql: text(r, "sql"),
                },
            ))
        })
        .collect())
}

/// Every session id in the edges, once.
fn ids(edges: &[Edge]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in edges {
        for id in [&e.waiter, &e.blocker] {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CockroachDB

/// Contended locks from `crdb_internal.cluster_locks` (a waiter's request
/// is `granted = false`, the holder's `true`, same key), transactions
/// mapped to their sessions through `cluster_sessions.kv_txn`.
async fn cockroach(s: &PgSession) -> Result<Vec<BlockedSession>> {
    // Since v25, crdb_internal is closed unless the session opts in.
    let _ = s.text_within("SET allow_unsafe_internals = true", QUERY_LIMIT).await;
    let rows = s
        .text_within(
            "WITH l AS (SELECT * FROM crdb_internal.cluster_locks WHERE contended AND txn_id IS NOT NULL),
                  w AS (SELECT DISTINCT ON (w.txn_id, h.txn_id) w.txn_id::STRING AS waiter, h.txn_id::STRING AS holder,
                               w.schema_name || '.' || w.table_name || ' (' || w.lock_key_pretty || ')' AS object,
                               'Lock: ' || coalesce(w.lock_strength, '') AS wait,
                               (extract(epoch FROM w.duration) * 1000)::INT8 AS wait_ms
                        FROM l AS w JOIN l AS h ON h.lock_key = w.lock_key AND h.granted AND h.txn_id <> w.txn_id
                        WHERE NOT w.granted)
             SELECT ws.session_id AS waiter, hs.session_id AS blocker, w.object, w.wait, w.wait_ms
             FROM w
             JOIN crdb_internal.cluster_sessions ws ON ws.kv_txn = w.waiter
             JOIN crdb_internal.cluster_sessions hs ON hs.kv_txn = w.holder
             ORDER BY ws.session_id",
            QUERY_LIMIT,
        )
        .await?;
    let edges: Vec<Edge> = rows
        .iter()
        .filter_map(|r| {
            Some(Edge {
                waiter: text(r, "waiter")?,
                blocker: text(r, "blocker")?,
                wait: text(r, "wait"),
                object: text(r, "object"),
                waited_ms: ms(r, "wait_ms"),
            })
        })
        .collect();
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids(&edges).iter().map(|i| format!("'{}'", i.replace('\'', "''"))).collect::<Vec<_>>().join(",");
    let rows = s
        .text_within(
            &format!(
                "SELECT s.session_id AS id, s.user_name AS usr,
                        concat_ws(' · ', nullif(s.client_address, ''), nullif(s.application_name, '')) AS client,
                        (SELECT l.database_name FROM crdb_internal.cluster_locks l WHERE l.txn_id::STRING = s.kv_txn LIMIT 1) AS db,
                        CASE WHEN s.active_queries = '' THEN 't' ELSE 'f' END AS idle,
                        (extract(epoch FROM now() - s.active_query_start) * 1000)::INT8 AS wait_ms,
                        (extract(epoch FROM now() - (SELECT t.start FROM crdb_internal.cluster_transactions t
                                                     WHERE t.id::STRING = s.kv_txn)::TIMESTAMPTZ) * 1000)::INT8 AS txn_ms,
                        left(coalesce(nullif(s.active_queries, ''), s.last_active_query), {MAX_TEXT}) AS sql
                 FROM crdb_internal.cluster_sessions s WHERE s.session_id IN ({list})"
            ),
            QUERY_LIMIT,
        )
        .await?;
    let info = rows
        .iter()
        .filter_map(|r| {
            let idle = truthy(text(r, "idle"));
            Some((
                text(r, "id")?,
                Info {
                    user: text(r, "usr"),
                    client: text(r, "client"),
                    database: text(r, "db"),
                    state: Some(if idle { IDLE_IN_TXN } else { "en ejecución" }.into()),
                    idle_in_txn: idle,
                    wait_ms: if idle { None } else { ms(r, "wait_ms") },
                    txn_ms: ms(r, "txn_ms"),
                    object: None,
                    sql: text(r, "sql"),
                },
            ))
        })
        .collect();
    Ok(assemble(&edges, &info))
}

// ---------------------------------------------------------------------------
// Redshift

/// `svv_transactions` lists every table lock, granted or waited for, with
/// its transaction; the running statement comes from `stv_recents`
/// (provisioned) or `sys_query_history` (Serverless).
async fn redshift(s: &PgSession) -> Result<Vec<BlockedSession>> {
    let rows = s
        .text_within(
            "SELECT pid::varchar AS id, txn_owner AS usr, txn_db AS db, relation::varchar AS relation, lock_mode AS mode,
                    CASE WHEN granted THEN 't' ELSE 'f' END AS granted,
                    datediff(ms, txn_start, getdate()) AS txn_ms
             FROM svv_transactions WHERE relation IS NOT NULL",
            QUERY_LIMIT,
        )
        .await?;
    let locks: Vec<LockRow> = rows
        .iter()
        .filter_map(|r| {
            let mode = text(r, "mode").unwrap_or_default();
            Some(LockRow {
                id: text(r, "id")?,
                key: text(r, "relation")?,
                wait: Some(format!("Lock: {mode}")),
                mode,
                granted: truthy(text(r, "granted")),
                object: text(r, "relation"),
            })
        })
        .collect();
    let mut edges = pair_locks(&locks);
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let mut info: HashMap<String, Info> = HashMap::new();
    for r in &rows {
        if let Some(id) = text(r, "id") {
            info.entry(id).or_insert_with(|| Info {
                user: text(r, "usr"),
                database: text(r, "db"),
                txn_ms: ms(r, "txn_ms"),
                idle_in_txn: true,
                ..Default::default()
            });
        }
    }
    // Statements running now; a session without one is idle in its
    // transaction.
    let pids = ids(&edges).iter().filter_map(|i| i.parse::<i64>().ok()).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let running = match s
        .text_within(
            &format!(
                "SELECT pid::varchar AS id, left(query, {MAX_TEXT}) AS sql, datediff(ms, starttime, getdate()) AS ms
                 FROM stv_recents WHERE status = 'Running' AND pid IN ({pids})"
            ),
            QUERY_LIMIT,
        )
        .await
    {
        Ok(r) => Ok(r),
        Err(_) => {
            s.text_within(
                &format!(
                    "SELECT session_id::varchar AS id, left(query_text, {MAX_TEXT}) AS sql, datediff(ms, start_time, getdate()) AS ms
                     FROM sys_query_history WHERE status = 'running' AND session_id IN ({pids})"
                ),
                QUERY_LIMIT,
            )
            .await
        }
    };
    if let Ok(running) = running {
        for r in &running {
            if let Some(i) = text(r, "id").and_then(|id| info.get_mut(&id)) {
                i.idle_in_txn = false;
                i.state = Some("en ejecución".into());
                i.sql = text(r, "sql");
                i.wait_ms = ms(r, "ms");
            }
        }
    }
    // Relation ids into names (pg_class is on the leader: a query of its own).
    let rels: Vec<String> = edges.iter().filter_map(|e| e.object.clone()).filter(|o| o.parse::<i64>().is_ok()).collect();
    if !rels.is_empty() {
        if let Ok(names) = s
            .text_within(
                &format!(
                    "SELECT c.oid::varchar AS oid, n.nspname || '.' || c.relname AS name
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid IN ({})",
                    rels.join(",")
                ),
                QUERY_LIMIT,
            )
            .await
        {
            let names: HashMap<String, String> = names.iter().filter_map(|r| Some((text(r, "oid")?, text(r, "name")?))).collect();
            for e in &mut edges {
                if let Some(n) = e.object.as_ref().and_then(|o| names.get(o)) {
                    e.object = Some(n.clone());
                }
            }
        }
    }
    Ok(assemble(&edges, &info))
}

// ---------------------------------------------------------------------------
// Yellowbrick

/// `sys.lock` says who blocks each waiting table lock
/// (`blocked_by_session_id`); `sys.session` and `sys.query` say the rest.
async fn yellowbrick(s: &PgSession) -> Result<Vec<BlockedSession>> {
    let rows = s
        .text_within(
            "SELECT DISTINCT l.session_id::varchar AS waiter, l.blocked_by_session_id::varchar AS blocker,
                    'Lock: ' || l.lock_type AS wait, l.table_id::varchar AS object
             FROM sys.lock l WHERE l.blocked_by_session_id IS NOT NULL AND NOT l.is_granted",
            QUERY_LIMIT,
        )
        .await?;
    let mut edges: Vec<Edge> = rows
        .iter()
        .filter_map(|r| {
            Some(Edge { waiter: text(r, "waiter")?, blocker: text(r, "blocker")?, wait: text(r, "wait"), object: text(r, "object"), waited_ms: None })
        })
        .collect();
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids(&edges).iter().filter_map(|i| i.parse::<i64>().ok()).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let mut info: HashMap<String, Info> = HashMap::new();
    if let Ok(rows) = s
        .text_within(
            &format!(
                "SELECT s.session_id::varchar AS id, u.name AS usr, d.name AS db, s.state AS state,
                        concat_ws(' · ', nullif(s.client_ip_address, ''), nullif(s.application_name, '')) AS client,
                        datediff(ms, s.last_statement, now()) AS idle_ms
                 FROM sys.session s LEFT JOIN sys.user u ON u.user_id = s.user_id
                      LEFT JOIN sys.database d ON d.database_id = s.database_id
                 WHERE s.session_id IN ({list})"
            ),
            QUERY_LIMIT,
        )
        .await
    {
        for r in &rows {
            if let Some(id) = text(r, "id") {
                let state = text(r, "state");
                info.insert(
                    id,
                    Info {
                        user: text(r, "usr"),
                        client: text(r, "client"),
                        database: text(r, "db"),
                        // A table lock held by an idle session: its
                        // transaction is still open.
                        idle_in_txn: state.as_deref() == Some("idle"),
                        state,
                        txn_ms: ms(r, "idle_ms"),
                        ..Default::default()
                    },
                );
            }
        }
    }
    if let Ok(rows) = s
        .text_within(
            &format!(
                "SELECT session_id::varchar AS id, left(query_text, {MAX_TEXT}) AS sql, total_ms
                 FROM sys.query WHERE session_id IN ({list})"
            ),
            QUERY_LIMIT,
        )
        .await
    {
        for r in &rows {
            if let Some(id) = text(r, "id") {
                let i = info.entry(id).or_default();
                i.sql = text(r, "sql");
                i.wait_ms = ms(r, "total_ms");
            }
        }
    }
    let tables: Vec<String> = edges.iter().filter_map(|e| e.object.clone()).filter(|o| o.parse::<i64>().is_ok()).collect();
    if !tables.is_empty() {
        if let Ok(names) =
            s.text_within(&format!("SELECT table_id::varchar AS id, name FROM sys.table WHERE table_id IN ({})", tables.join(",")), QUERY_LIMIT).await
        {
            let names: HashMap<String, String> = names.iter().filter_map(|r| Some((text(r, "id")?, text(r, "name")?))).collect();
            for e in &mut edges {
                if let Some(n) = e.object.as_ref().and_then(|o| names.get(o)) {
                    e.object = Some(n.clone());
                }
            }
        }
    }
    Ok(assemble(&edges, &info))
}

// ---------------------------------------------------------------------------
// H2

/// `information_schema.sessions` has the blocker of each session
/// (`blocker_id`); the object is the table both sessions lock.
async fn h2(s: &PgSession) -> Result<Vec<BlockedSession>> {
    let rows = s
        .text_within(
            &format!(
                "SELECT CAST(session_id AS VARCHAR) AS id, CAST(blocker_id AS VARCHAR) AS blocker, user_name AS usr,
                        client_addr AS client, session_state AS state,
                        CASE WHEN contains_uncommitted THEN 't' ELSE 'f' END AS uncommitted,
                        DATEDIFF('MILLISECOND', executing_statement_start, CURRENT_TIMESTAMP) AS wait_ms,
                        DATEDIFF('MILLISECOND', COALESCE(sleep_since, executing_statement_start), CURRENT_TIMESTAMP) AS idle_ms,
                        LEFT(executing_statement, {MAX_TEXT}) AS sql
                 FROM information_schema.sessions"
            ),
            QUERY_LIMIT,
        )
        .await?;
    let mut edges: Vec<Edge> = rows
        .iter()
        .filter_map(|r| Some(Edge { waiter: text(r, "id")?, blocker: text(r, "blocker")?, wait: Some("Lock".into()), ..Default::default() }))
        .collect();
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let info: HashMap<String, Info> = rows
        .iter()
        .filter_map(|r| {
            let state = text(r, "state");
            let sleeping = state.as_deref() == Some("SLEEP");
            Some((
                text(r, "id")?,
                Info {
                    user: text(r, "usr"),
                    client: text(r, "client"),
                    database: Some(s.database.clone()),
                    idle_in_txn: sleeping && truthy(text(r, "uncommitted")),
                    state,
                    wait_ms: ms(r, "wait_ms"),
                    txn_ms: if sleeping { ms(r, "idle_ms") } else { ms(r, "wait_ms") },
                    object: None,
                    sql: text(r, "sql"),
                },
            ))
        })
        .collect();
    // The locked table: one both the waiter and its blocker lock.
    if let Ok(locks) = s
        .text_within("SELECT CAST(session_id AS VARCHAR) AS id, table_schema || '.' || table_name AS t FROM information_schema.locks", QUERY_LIMIT)
        .await
    {
        let of = |id: &str| locks.iter().filter(|r| text(r, "id").as_deref() == Some(id)).filter_map(|r| text(r, "t")).collect::<Vec<_>>();
        for e in &mut edges {
            let mine = of(&e.waiter);
            e.object = of(&e.blocker).into_iter().find(|t| mine.contains(t));
        }
    }
    Ok(assemble(&edges, &info))
}

// ---------------------------------------------------------------------------
// Kill

pub(crate) async fn kill(s: &PgSession, id: &str) -> Result<()> {
    if let Some(why) = unsupported_reason(s.variant) {
        return Err(Error::Unsupported(why.into()));
    }
    let id = id.trim();
    let own = |own: Option<String>| -> Result<()> {
        if own.as_deref().map(str::trim) == Some(id) {
            return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede terminar desde acá".into()));
        }
        Ok(())
    };
    let first = |rows: Vec<SimpleQueryRow>| rows.first().and_then(|r| r.get(0).map(str::to_string));
    match s.variant {
        Variant::Cockroach => {
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Error::Query(format!("«{id}» no es un id de sesión de CockroachDB")));
            }
            own(first(s.text("SHOW session_id").await?))?;
            s.text(&format!("CANCEL SESSION '{id}'")).await.map(|_| ())
        }
        Variant::H2 => {
            let n: i64 = id.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de sesión de H2")))?;
            own(first(s.text("SELECT CAST(SESSION_ID() AS VARCHAR)").await?))?;
            let r = first(s.text(&format!("SELECT CASE WHEN ABORT_SESSION({n}) THEN 't' ELSE 'f' END")).await?);
            ended(r, id)
        }
        Variant::Yellowbrick => {
            let n: i64 = id.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de sesión de Yellowbrick")))?;
            let r = first(s.text(&format!("SELECT yb_terminate_session({n})::varchar")).await?);
            ended(r, id)
        }
        v => {
            // Backend pids are int4; openGauss's are thread ids (int8).
            let n: i64 = if v == Variant::OpenGauss {
                id.parse().ok()
            } else {
                id.parse::<i32>().ok().map(i64::from)
            }
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión (PID)")))?;
            own(first(s.text("SELECT pg_backend_pid()::varchar").await?))?;
            let sql = if v == Variant::OpenGauss {
                format!("SELECT pg_terminate_backend({n})::text")
            } else if v == Variant::Redshift {
                format!("SELECT pg_terminate_backend({n})")
            } else {
                format!("SELECT pg_terminate_backend({n}::int)::text")
            };
            ended(first(s.text(&sql).await?), id)
        }
    }
}

/// The result of a terminate function: true (or 1) when it ended it.
fn ended(r: Option<String>, id: &str) -> Result<()> {
    if truthy(r) {
        Ok(())
    } else {
        Err(Error::Query(format!(
            "no se pudo terminar la sesión {id}: no existe, ya terminó o tu usuario no tiene permiso para terminarla"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(w: &str, b: &str) -> Edge {
        Edge { waiter: w.into(), blocker: b.into(), wait: Some("Lock: transactionid".into()), ..Default::default() }
    }

    #[test]
    fn conflict_table() {
        assert!(conflicts("ShareLock", "ExclusiveLock"));
        assert!(conflicts("AccessShareLock", "AccessExclusiveLock"));
        assert!(!conflicts("AccessShareLock", "RowExclusiveLock"));
        assert!(!conflicts("RowExclusiveLock", "RowExclusiveLock"));
        assert!(conflicts("RowExclusiveLock", "ShareLock"));
        assert!(conflicts("ShareUpdateExclusiveLock", "ShareUpdateExclusiveLock"));
        assert!(!conflicts("ShareLock", "ShareLock"));
        assert!(conflicts("ShareRowExclusiveLock", "ShareRowExclusiveLock"));
        assert!(!conflicts("RowShareLock", "ShareLock"));
        assert!(conflicts("SIReadLock?", "AccessShareLock"), "unknown modes conflict");
        // Symmetric.
        let modes = ["AccessShareLock", "RowShareLock", "RowExclusiveLock", "ShareUpdateExclusiveLock", "ShareLock", "ShareRowExclusiveLock", "ExclusiveLock", "AccessExclusiveLock"];
        for a in modes {
            for b in modes {
                assert_eq!(conflicts(a, b), conflicts(b, a), "{a} {b}");
            }
        }
    }

    #[test]
    fn pairs_only_conflicting_grants() {
        let l = |id: &str, mode: &str, granted: bool| LockRow {
            id: id.into(),
            key: "relation/5/16384".into(),
            mode: mode.into(),
            granted,
            wait: Some("Lock: relation".into()),
            object: None,
        };
        let edges = pair_locks(&[
            l("1", "AccessExclusiveLock", true),
            l("2", "RowExclusiveLock", true),
            l("3", "ShareLock", false),
            l("3", "ShareLock", false),
        ]);
        let pairs: Vec<_> = edges.iter().map(|e| (e.waiter.as_str(), e.blocker.as_str())).collect();
        assert_eq!(pairs, [("3", "1"), ("3", "2")]);
        assert!(pair_locks(&[l("1", "AccessShareLock", true), l("2", "RowExclusiveLock", false)]).is_empty());
    }

    #[test]
    fn heads_and_waiters() {
        let mut info = HashMap::new();
        info.insert("10".to_string(), Info { idle_in_txn: true, state: Some("idle in transaction".into()), txn_ms: Some(9000), wait_ms: Some(10), ..Default::default() });
        info.insert("20".to_string(), Info { state: Some("active".into()), wait_ms: Some(1500), txn_ms: Some(1600), object: Some("t (fila 0,1)".into()), sql: Some("UPDATE t".into()), ..Default::default() });
        info.insert("30".to_string(), Info { state: Some("active".into()), wait_ms: Some(700), ..Default::default() });
        // 30 waits on 20 and on 10; 20 waits on 10.
        let chain = assemble(&[edge("20", "10"), edge("30", "20"), edge("30", "10")], &info);
        let ids: Vec<_> = chain.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["20", "10", "30"]);
        let head = &chain[1];
        assert_eq!(head.blocked_by, None);
        assert_eq!(head.wait.as_deref(), Some(IDLE_IN_TXN));
        assert_eq!(head.waited_ms, Some(9000));
        assert_eq!(chain[0].blocked_by.as_deref(), Some("10"));
        assert_eq!(chain[0].wait.as_deref(), Some("Lock: transactionid"));
        assert_eq!(chain[0].waited_ms, Some(1500));
        assert_eq!(chain[0].object.as_deref(), Some("t (fila 0,1)"));
        assert_eq!(chain[2].blocked_by.as_deref(), Some("20"));
        assert!(assemble(&[], &info).is_empty());
    }

    #[test]
    fn engines_without_locks_say_why() {
        for v in Variant::ALL {
            let unsupported = matches!(v, Variant::Materialize | Variant::RisingWave | Variant::CrateDb | Variant::Denodo);
            assert_eq!(supported(v), !unsupported, "{v:?}");
        }
        assert!(unsupported_reason(Variant::Postgres).is_none());
    }

    #[test]
    fn terminate_results() {
        assert!(ended(Some("t".into()), "1").is_ok());
        assert!(ended(Some("1".into()), "1").is_ok());
        assert!(ended(Some("f".into()), "1").is_err());
        assert!(ended(None, "1").is_err());
    }
}

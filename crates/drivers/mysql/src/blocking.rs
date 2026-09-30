//! Blocking chains (`Session::blocking`) and `KILL` (`Session::kill_session`)
//! for the engines that report lock waits:
//!
//! - MySQL 8 (and Aurora, Cloud SQL): InnoDB row-lock waits from
//!   `performance_schema.data_lock_waits`, metadata-lock waits from
//!   `sys.schema_table_lock_waits`, the last statement of an idle session
//!   from `events_statements_current`.
//! - MySQL 5.7 and MariaDB: `information_schema.INNODB_LOCK_WAITS`.
//! - TiDB: pessimistic lock waits from `information_schema.DATA_LOCK_WAITS`
//!   and `CLUSTER_TIDB_TRX`; sessions from `CLUSTER_PROCESSLIST`.
//!
//! The waits give the edges (who waits for whom); the process list fills in
//! user, host, database and statement for every session in a chain.

use crate::session::{at, MySqlSession};
use crate::Variant;
use dbine_driver::{BlockedSession, Error, Result};
use std::collections::HashMap;

/// InnoDB row-lock waits on MySQL 8.
const MYSQL8_ROW_WAITS: &str = "SELECT r.trx_mysql_thread_id, b.trx_mysql_thread_id,
       CONCAT('LOCK WAIT ', l.LOCK_TYPE, ' ', l.LOCK_MODE),
       TIMESTAMPDIFF(MICROSECOND, r.trx_wait_started, NOW(6)) DIV 1000,
       CONCAT(l.OBJECT_SCHEMA, '.', l.OBJECT_NAME, IFNULL(CONCAT(' · ', l.INDEX_NAME), ''),
              IFNULL(CONCAT(' · ', l.LOCK_DATA), '')),
       LEFT(r.trx_query, 4000)
  FROM performance_schema.data_lock_waits w
  JOIN performance_schema.data_locks l ON l.ENGINE_LOCK_ID = w.REQUESTING_ENGINE_LOCK_ID
  JOIN information_schema.INNODB_TRX r ON r.trx_id = w.REQUESTING_ENGINE_TRANSACTION_ID
  JOIN information_schema.INNODB_TRX b ON b.trx_id = w.BLOCKING_ENGINE_TRANSACTION_ID";

/// InnoDB row-lock waits on MariaDB and MySQL 5.7.
const LEGACY_ROW_WAITS: &str = "SELECT r.trx_mysql_thread_id, b.trx_mysql_thread_id,
       CONCAT('LOCK WAIT ', l.lock_type, ' ', l.lock_mode),
       TIMESTAMPDIFF(MICROSECOND, r.trx_wait_started, NOW(6)) DIV 1000,
       CONCAT(l.lock_table, IFNULL(CONCAT(' · ', l.lock_index), ''), IFNULL(CONCAT(' · ', l.lock_data), '')),
       LEFT(r.trx_query, 4000)
  FROM information_schema.INNODB_LOCK_WAITS w
  JOIN information_schema.INNODB_LOCKS l ON l.lock_id = w.requested_lock_id
  JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id
  JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id";

/// Metadata-lock waits (ALTER TABLE behind an open transaction…), MySQL 8.
const MYSQL8_MDL_WAITS: &str = "SELECT waiting_pid, blocking_pid,
       CONCAT('Metadata lock ', waiting_lock_type), waiting_query_secs * 1000,
       CONCAT(object_schema, '.', object_name), LEFT(waiting_query, 4000)
  FROM sys.schema_table_lock_waits";

/// TiDB's pessimistic lock waits, by session.
const TIDB_WAITS: &str = "SELECT r.SESSION_ID, b.SESSION_ID, CONCAT('Lock wait (', r.STATE, ')'),
       TIMESTAMPDIFF(MICROSECOND, r.WAITING_START_TIME, NOW(6)) DIV 1000,
       w.KEY_INFO, LEFT(IFNULL(r.CURRENT_SQL_DIGEST_TEXT, w.SQL_DIGEST_TEXT), 4000)
  FROM information_schema.DATA_LOCK_WAITS w
  JOIN information_schema.CLUSTER_TIDB_TRX r ON r.ID = w.TRX_ID
  JOIN information_schema.CLUSTER_TIDB_TRX b ON b.ID = w.CURRENT_HOLDING_TRX_ID";

/// One session waiting for another.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Edge {
    pub waiter: u64,
    pub blocker: u64,
    pub wait: Option<String>,
    pub waited_ms: Option<u64>,
    pub object: Option<String>,
    pub sql: Option<String>,
}

/// What the process list says about a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Proc {
    pub user: Option<String>,
    pub host: Option<String>,
    pub database: Option<String>,
    pub command: Option<String>,
    pub state: Option<String>,
    /// Time in the current command (idle time for a sleeping session), ms.
    pub time_ms: Option<u64>,
    /// The running statement.
    pub info: Option<String>,
    /// The last statement it ran (performance_schema, TiDB's digests).
    pub last_sql: Option<String>,
}

fn text(r: &mysql_async::Row, i: usize) -> Option<String> {
    at(r, i).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn ms(r: &mysql_async::Row, i: usize) -> Option<u64> {
    text(r, i).and_then(|s| s.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn id(r: &mysql_async::Row, i: usize) -> Option<u64> {
    text(r, i).and_then(|s| s.parse().ok())
}

/// TiDB's `KEY_INFO` (`{"db_name":"app","table_name":"t","handle_value":"1"…}`)
/// as `app.t · 1`; the raw text if it isn't that.
pub(crate) fn tidb_key(info: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(info) else {
        return info.to_string();
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty());
    let mut out = match (s("db_name"), s("table_name")) {
        (Some(d), Some(t)) => format!("{d}.{t}"),
        (None, Some(t)) => t.to_string(),
        _ => return info.to_string(),
    };
    if let Some(i) = s("index_name") {
        out.push_str(&format!(" · {i}"));
    }
    if let Some(h) = s("handle_value").or_else(|| s("index_values")) {
        out.push_str(&format!(" · {h}"));
    }
    out
}

/// The last non-empty text of TiDB's `tidb_decode_sql_digests` (a JSON
/// array of the transaction's statements).
pub(crate) fn last_digest(json: &str) -> Option<String> {
    let v: Vec<Option<String>> = serde_json::from_str(json).ok()?;
    v.into_iter().flatten().map(|s| s.trim().to_string()).rfind(|s| !s.is_empty())
}

/// Waiters (one row each, on their first blocker) and then the heads.
pub(crate) fn assemble(edges: &[Edge], procs: &HashMap<u64, Proc>) -> Vec<BlockedSession> {
    let mut out: Vec<BlockedSession> = Vec::new();
    let mut waiters: Vec<u64> = Vec::new();
    for e in edges {
        if waiters.contains(&e.waiter) {
            continue;
        }
        waiters.push(e.waiter);
        let p = procs.get(&e.waiter).cloned().unwrap_or_default();
        out.push(BlockedSession {
            id: e.waiter.to_string(),
            blocked_by: Some(e.blocker.to_string()),
            user: p.user,
            client: p.host,
            database: p.database,
            wait: e.wait.clone().or(p.state),
            waited_ms: e.waited_ms.or(p.time_ms),
            object: e.object.clone(),
            sql: p.info.or_else(|| e.sql.clone()).or(p.last_sql),
        });
    }
    let mut heads: Vec<u64> = Vec::new();
    for e in edges {
        if waiters.contains(&e.blocker) || heads.contains(&e.blocker) {
            continue;
        }
        heads.push(e.blocker);
        let p = procs.get(&e.blocker).cloned().unwrap_or_default();
        let idle = p.command.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("Sleep"));
        out.push(BlockedSession {
            id: e.blocker.to_string(),
            blocked_by: None,
            user: p.user,
            client: p.host,
            database: p.database,
            wait: if idle {
                Some("inactiva con transacción abierta".into())
            } else {
                p.state.filter(|s| !s.is_empty() && s != "0").or(p.command)
            },
            waited_ms: p.time_ms,
            object: None,
            sql: p.info.or(p.last_sql),
        });
    }
    out
}

/// A session id: an unsigned integer, nothing else.
pub(crate) fn session_id(id: &str) -> Result<u64> {
    id.trim().parse().map_err(|_| Error::Query(format!("«{id}» no es un id de conexión válido")))
}

impl MySqlSession {
    async fn edges(&mut self, sql: &str, tidb: bool) -> Result<Vec<Edge>> {
        let rows = self.rows(sql).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let (waiter, blocker) = (id(r, 0)?, id(r, 1)?);
                (waiter != blocker).then(|| Edge {
                    waiter,
                    blocker,
                    wait: text(r, 2),
                    waited_ms: ms(r, 3),
                    object: text(r, 4).map(|o| if tidb { tidb_key(&o) } else { o }),
                    sql: text(r, 5),
                })
            })
            .collect())
    }

    pub(crate) async fn blocking_chains(&mut self) -> Result<Vec<BlockedSession>> {
        let tidb = self.variant == Variant::TiDb;
        let mut edges = if tidb {
            self.edges(TIDB_WAITS, true).await?
        } else {
            match self.edges(MYSQL8_ROW_WAITS, false).await {
                Ok(e) => {
                    // Metadata locks: sys schema and its instruments may be missing.
                    let mut e = e;
                    match self.edges(MYSQL8_MDL_WAITS, false).await {
                        Ok(mdl) => e.extend(mdl),
                        Err(err) => tracing::debug!("mysql blocking: metadata locks: {err}"),
                    }
                    e
                }
                Err(e8) => self.edges(LEGACY_ROW_WAITS, false).await.map_err(|e| {
                    tracing::debug!("mysql blocking: data_lock_waits: {e8}");
                    Error::Query(format!(
                        "No se pudieron leer las esperas de bloqueo: {e}. Hace falta el privilegio PROCESS."
                    ))
                })?,
            }
        };
        edges.dedup();
        if edges.is_empty() {
            return Ok(Vec::new());
        }
        let mut ids: Vec<u64> = edges.iter().flat_map(|e| [e.waiter, e.blocker]).collect();
        ids.sort_unstable();
        ids.dedup();
        // Numbers parsed above: safe to interpolate.
        let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let view = if tidb { "information_schema.CLUSTER_PROCESSLIST" } else { "information_schema.PROCESSLIST" };
        let procs_sql = format!(
            "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, LEFT(INFO, 4000) FROM {view} WHERE ID IN ({list})"
        );
        let mut procs: HashMap<u64, Proc> = HashMap::new();
        for r in self.optional_rows(&procs_sql).await {
            let Some(pid) = id(&r, 0) else { continue };
            procs.insert(
                pid,
                Proc {
                    user: text(&r, 1),
                    host: text(&r, 2),
                    database: text(&r, 3),
                    command: text(&r, 4),
                    state: text(&r, 6),
                    time_ms: ms(&r, 5).map(|s| s * 1000),
                    info: text(&r, 7),
                    last_sql: None,
                },
            );
        }
        // The last statement of each session, for an idle head.
        let last = if tidb {
            format!(
                "SELECT SESSION_ID, tidb_decode_sql_digests(ALL_SQL_DIGESTS, 4000)
                   FROM information_schema.CLUSTER_TIDB_TRX WHERE SESSION_ID IN ({list})"
            )
        } else {
            // MariaDB only has it with performance_schema = ON.
            format!(
                "SELECT t.PROCESSLIST_ID, LEFT(e.SQL_TEXT, 4000)
                   FROM performance_schema.threads t
                   JOIN performance_schema.events_statements_current e ON e.THREAD_ID = t.THREAD_ID
                  WHERE t.PROCESSLIST_ID IN ({list})"
            )
        };
        {
            for r in self.optional_rows(&last).await {
                let (Some(pid), Some(sql)) = (id(&r, 0), text(&r, 1)) else { continue };
                let sql = if tidb { last_digest(&sql) } else { Some(sql) };
                if let Some(p) = procs.get_mut(&pid) {
                    p.last_sql = sql;
                }
            }
        }
        Ok(assemble(&edges, &procs))
    }

    pub(crate) async fn kill_connection(&mut self, id: &str) -> Result<()> {
        let id = session_id(id)?;
        let sql = if self.variant == Variant::TiDb { format!("KILL TIDB {id}") } else { format!("KILL {id}") };
        self.rows(&sql).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_numbers() {
        assert_eq!(session_id(" 42 ").unwrap(), 42);
        assert!(session_id("1; DROP TABLE x").is_err());
        assert!(session_id("-1").is_err());
        assert!(session_id("").is_err());
    }

    #[test]
    fn tidb_keys_read_as_tables() {
        assert_eq!(
            tidb_key(r#"{"db_id":2,"db_name":"app","table_id":9,"table_name":"t","handle_type":"int","handle_value":"1"}"#),
            "app.t · 1"
        );
        assert_eq!(tidb_key("7480000000"), "7480000000");
        assert_eq!(last_digest(r#"["begin","update `t` set `v` = ? where `id` = ?",null]"#).unwrap(), "update `t` set `v` = ? where `id` = ?");
    }

    #[test]
    fn chains_list_waiters_then_heads() {
        let edges = vec![
            Edge { waiter: 3, blocker: 2, wait: Some("LOCK WAIT".into()), waited_ms: Some(900), ..Default::default() },
            Edge { waiter: 2, blocker: 1, ..Default::default() },
            Edge { waiter: 3, blocker: 1, ..Default::default() },
        ];
        let mut procs = HashMap::new();
        procs.insert(1, Proc { command: Some("Sleep".into()), time_ms: Some(5000), last_sql: Some("UPDATE t".into()), ..Default::default() });
        procs.insert(3, Proc { info: Some("UPDATE t SET v = 2".into()), ..Default::default() });
        let chain = assemble(&edges, &procs);
        assert_eq!(chain.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["3", "2", "1"]);
        assert_eq!(chain[0].blocked_by.as_deref(), Some("2"));
        assert_eq!(chain[0].sql.as_deref(), Some("UPDATE t SET v = 2"));
        assert_eq!(chain[2].blocked_by, None);
        assert_eq!(chain[2].wait.as_deref(), Some("inactiva con transacción abierta"));
        assert_eq!(chain[2].sql.as_deref(), Some("UPDATE t"));
    }
}

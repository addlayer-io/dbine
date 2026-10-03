//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another session's statement ([`dbine_driver::Session::cancel_query`])
//! per variant:
//!
//! - MySQL 5.7+ (Aurora, Cloud SQL): `performance_schema.threads` with the
//!   current statement (elapsed time, CPU from 8.0.28, rows examined), the
//!   client's `program_name` and InnoDB's open transactions; without
//!   performance_schema, `information_schema.PROCESSLIST`. `KILL QUERY`.
//! - MariaDB: `information_schema.PROCESSLIST` (`TIME_MS`, `EXAMINED_ROWS`)
//!   and InnoDB's open transactions. `KILL QUERY`.
//! - TiDB: `CLUSTER_PROCESSLIST` (every TiDB instance) and
//!   `CLUSTER_TIDB_TRX`. `KILL TIDB QUERY`.
//! - OceanBase, SingleStore, StarRocks, Doris, VeloDB: `SHOW FULL
//!   PROCESSLIST`. `KILL QUERY`.
//! - Databend: `system.processes` (sessions by UUID). `KILL QUERY '<id>'`.
//! - Manticore and GreptimeDB only report running queries (`SHOW QUERIES`,
//!   `information_schema.process_list`), and `KILL` ends the query, not a
//!   session: they cancel, but have no session to end.
//!
//! The first query a server answers is remembered, so a server without
//! performance_schema doesn't pay for the failed one on every poll. Who
//! waits for whom comes from the blocking queries, run only when some
//! session is waiting on a lock.

use crate::blocking::{last_digest, session_id};
use crate::session::{named, MySqlSession};
use crate::Variant;
use dbine_driver::{Error, Result, ServerProcess};
use std::collections::HashMap;

/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most: a server with thousands of connections still answers fast.
const MAX_ROWS: usize = 2000;
/// Mark of the GreptimeDB listing query, to find its own row.
const OWN_MARK: &str = "dbine_processes";

/// Engines whose `KILL` ends a session (`Session::kill_session`).
pub(crate) fn can_kill(v: Variant) -> bool {
    !matches!(v, Variant::Manticore | Variant::GreptimeDb)
}

/// performance_schema with the current statement; `{cpu}` is CPU_TIME
/// (MySQL 8.0.28+) or NULL.
const MYSQL_PS: &str = "SELECT /*+ MAX_EXECUTION_TIME(5000) */
       t.PROCESSLIST_ID AS pid, t.PROCESSLIST_USER AS puser, t.PROCESSLIST_HOST AS phost, t.PROCESSLIST_DB AS pdb,
       t.PROCESSLIST_COMMAND AS pcmd, t.PROCESSLIST_STATE AS pstate,
       CASE WHEN e.END_EVENT_ID IS NULL AND e.TIMER_WAIT IS NOT NULL THEN e.TIMER_WAIT DIV 1000000000
            ELSE t.PROCESSLIST_TIME * 1000 END AS ptime_ms,
       LEFT(t.PROCESSLIST_INFO, {MAX_TEXT}) AS pinfo, a.ATTR_VALUE AS pprogram,
       {cpu} AS pcpu_ms,
       CASE WHEN e.END_EVENT_ID IS NULL THEN e.ROWS_EXAMINED END AS preads,
       (x.trx_id IS NOT NULL) AS pin_trx, (x.trx_state = 'LOCK WAIT') AS plock_wait,
       CASE WHEN x.trx_id IS NOT NULL THEN LEFT(e.SQL_TEXT, {MAX_TEXT}) END AS plast_sql,
       (t.CONNECTION_TYPE IS NULL) AS psystem, (t.PROCESSLIST_ID = CONNECTION_ID()) AS pown
  FROM performance_schema.threads t
  LEFT JOIN performance_schema.events_statements_current e ON e.THREAD_ID = t.THREAD_ID AND e.NESTING_EVENT_ID IS NULL
  LEFT JOIN performance_schema.session_connect_attrs a ON a.PROCESSLIST_ID = t.PROCESSLIST_ID AND a.ATTR_NAME = 'program_name'
  LEFT JOIN information_schema.INNODB_TRX x ON x.trx_mysql_thread_id = t.PROCESSLIST_ID
 WHERE t.PROCESSLIST_ID IS NOT NULL
 ORDER BY (t.PROCESSLIST_COMMAND = 'Sleep'), t.PROCESSLIST_ID LIMIT {MAX_ROWS}";

/// information_schema.PROCESSLIST and InnoDB's transactions; `{time}` and
/// `{reads}` differ between MySQL and MariaDB.
const PROCESSLIST: &str = "SELECT /*+ MAX_EXECUTION_TIME(5000) */
       p.ID AS pid, p.USER AS puser, p.HOST AS phost, p.DB AS pdb, p.COMMAND AS pcmd, p.STATE AS pstate,
       {time} AS ptime_ms, LEFT(p.INFO, {MAX_TEXT}) AS pinfo, {reads} AS preads,
       (x.trx_id IS NOT NULL) AS pin_trx, (x.trx_state = 'LOCK WAIT') AS plock_wait, (p.ID = CONNECTION_ID()) AS pown
  FROM information_schema.PROCESSLIST p
  LEFT JOIN information_schema.INNODB_TRX x ON x.trx_mysql_thread_id = p.ID
 ORDER BY (p.COMMAND = 'Sleep'), p.ID LIMIT {MAX_ROWS}";

/// Every TiDB instance's sessions and their transactions.
const TIDB: &str = "SELECT /*+ MAX_EXECUTION_TIME(5000) */
       p.ID AS pid, p.USER AS puser, p.HOST AS phost, p.DB AS pdb, p.COMMAND AS pcmd, p.STATE AS pstate,
       p.TIME * 1000 AS ptime_ms, LEFT(p.INFO, {MAX_TEXT}) AS pinfo,
       (t.ID IS NOT NULL) AS pin_trx, (t.STATE = 'LockWaiting') AS plock_wait,
       CASE WHEN t.ID IS NOT NULL AND p.COMMAND = 'Sleep' THEN tidb_decode_sql_digests(t.ALL_SQL_DIGESTS, {MAX_TEXT}) END AS plast_sql,
       (p.ID = CONNECTION_ID()) AS pown
  FROM information_schema.CLUSTER_PROCESSLIST p
  LEFT JOIN information_schema.CLUSTER_TIDB_TRX t ON t.SESSION_ID = p.ID
 ORDER BY (p.COMMAND = 'Sleep'), p.ID LIMIT {MAX_ROWS}";

/// TiDB before CLUSTER_TIDB_TRX (5.x).
const TIDB_PLAIN: &str = "SELECT ID AS pid, USER AS puser, HOST AS phost, DB AS pdb, COMMAND AS pcmd, STATE AS pstate,
       TIME * 1000 AS ptime_ms, LEFT(INFO, {MAX_TEXT}) AS pinfo, (ID = CONNECTION_ID()) AS pown
  FROM information_schema.CLUSTER_PROCESSLIST
 ORDER BY (COMMAND = 'Sleep'), ID LIMIT {MAX_ROWS}";

const DATABEND: &str = "SELECT id AS pid, `user` AS puser, host AS phost, `database` AS pdb, command AS pcmd,
       status AS pstate, time * 1000 AS ptime_ms, extra_info AS pinfo, scan_progress_read_rows AS preads,
       (id = connection_id()) AS pown
  FROM system.processes LIMIT {MAX_ROWS}";

const GREPTIME: &str = "SELECT id AS pid, schemas AS pdb, client AS phost, query AS pinfo,
       CAST(elapsed_time AS BIGINT) AS ptime_ms, 1 AS {OWN_MARK}
  FROM information_schema.process_list LIMIT {MAX_ROWS}";

const SHOW: &str = "SHOW FULL PROCESSLIST";

/// The queries to try, in order, for a variant.
fn queries(v: Variant) -> Vec<String> {
    let fill = |sql: &str| sql.replace("{MAX_TEXT}", &MAX_TEXT.to_string()).replace("{MAX_ROWS}", &MAX_ROWS.to_string());
    match v {
        Variant::MySql => vec![
            fill(MYSQL_PS).replace("{cpu}", "CASE WHEN e.END_EVENT_ID IS NULL THEN e.CPU_TIME DIV 1000000000 END"),
            fill(MYSQL_PS).replace("{cpu}", "NULL"),
            fill(PROCESSLIST).replace("{time}", "p.TIME * 1000").replace("{reads}", "NULL"),
            SHOW.into(),
        ],
        Variant::MariaDb => {
            vec![fill(PROCESSLIST).replace("{time}", "ROUND(p.TIME_MS)").replace("{reads}", "p.EXAMINED_ROWS"), SHOW.into()]
        }
        Variant::TiDb => vec![fill(TIDB), fill(TIDB_PLAIN), SHOW.into()],
        Variant::Databend => vec![fill(DATABEND)],
        Variant::Manticore => vec!["SHOW QUERIES".into()],
        Variant::GreptimeDb => vec![fill(GREPTIME).replace("{OWN_MARK}", OWN_MARK)],
        _ => vec![SHOW.into()],
    }
}

/// One row of whichever query answered, by column name.
#[derive(Debug, Clone, Default, PartialEq)]
struct Raw {
    id: String,
    user: Option<String>,
    host: Option<String>,
    database: Option<String>,
    command: Option<String>,
    state: Option<String>,
    info: Option<String>,
    elapsed_ms: Option<u64>,
    program: Option<String>,
    cpu_ms: Option<u64>,
    reads: Option<u64>,
    /// It has an open transaction.
    in_trx: bool,
    /// Its transaction waits for a row lock.
    lock_wait: bool,
    /// The last statement it ran (for an idle session in a transaction).
    last_sql: Option<String>,
    system: Option<bool>,
    own: Option<bool>,
}

fn text(r: &mysql_async::Row, names: &[&str]) -> Option<String> {
    named(r, names).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn number(r: &mysql_async::Row, names: &[&str]) -> Option<u64> {
    text(r, names).and_then(|s| s.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn flag(r: &mysql_async::Row, names: &[&str]) -> Option<bool> {
    text(r, names).map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true"))
}

fn raw(r: &mysql_async::Row, tidb: bool) -> Raw {
    let elapsed = number(r, &["ptime_ms"])
        .or_else(|| number(r, &["Time"]).map(|s| s * 1000))
        .or_else(|| text(r, &["time"]).and_then(|t| duration_ms(&t)));
    let last = text(r, &["plast_sql"]);
    let info = text(r, &["pinfo", "Info", "query"]);
    // GreptimeDB: its own row is the listing query, marked.
    let marked = named(r, &[OWN_MARK]).map(|_| info.as_deref().is_some_and(|q| q.contains(OWN_MARK)));
    Raw {
        id: text(r, &["pid", "Id", "ID"]).unwrap_or_default(),
        user: text(r, &["puser", "User"]),
        host: text(r, &["phost", "Host"]),
        database: text(r, &["pdb", "db"]),
        command: text(r, &["pcmd", "Command"]),
        state: text(r, &["pstate", "State"]),
        info,
        elapsed_ms: elapsed,
        program: text(r, &["pprogram"]),
        cpu_ms: number(r, &["pcpu_ms"]),
        reads: number(r, &["preads"]),
        in_trx: flag(r, &["pin_trx"]).unwrap_or(false),
        lock_wait: flag(r, &["plock_wait"]).unwrap_or(false),
        last_sql: if tidb { last.and_then(|j| last_digest(&j)) } else { last },
        system: flag(r, &["psystem"]),
        own: flag(r, &["pown"]).or(marked),
    }
}

/// Manticore's `1h 2m 3s`, `7ms ago` or `250us` as milliseconds.
fn duration_ms(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches("ago").trim();
    let (mut total, mut num, mut unit, mut any) = (0.0, String::new(), String::new(), false);
    let mut flush = |num: &mut String, unit: &mut String| -> Option<()> {
        if num.is_empty() {
            return Some(());
        }
        let n: f64 = num.parse().ok()?;
        let factor = match unit.as_str() {
            "us" | "µs" => 0.001,
            "ms" => 1.0,
            "s" | "" => 1000.0,
            "m" | "min" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            _ => return None,
        };
        total += n * factor;
        any = true;
        num.clear();
        unit.clear();
        Some(())
    };
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.is_empty() {
                flush(&mut num, &mut unit)?;
            }
            num.push(c);
        } else if c.is_whitespace() {
            continue;
        } else {
            unit.push(c);
        }
    }
    flush(&mut num, &mut unit)?;
    any.then_some(total as u64)
}

/// The first keyword of a statement (`SELECT`, `UPDATE`…), past comments.
fn first_word(sql: &str) -> Option<String> {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else if let Some(rest) = s.strip_prefix('(') {
            s = rest.trim_start();
        } else {
            break;
        }
    }
    let w: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!w.is_empty()).then(|| w.to_ascii_uppercase())
}

/// Server threads, not clients: the event scheduler, replication, MariaDB's
/// "system user" workers.
fn system_thread(r: &Raw) -> bool {
    let user = r.user.as_deref().unwrap_or("");
    let cmd = r.command.as_deref().unwrap_or("");
    user.eq_ignore_ascii_case("system user")
        || user.eq_ignore_ascii_case("event_scheduler")
        || ["Daemon", "Binlog Dump", "Binlog Dump GTID", "Slave_IO", "Slave_SQL", "Slave_worker"].iter().any(|c| cmd.eq_ignore_ascii_case(c))
}

/// A row as the Monitor shows it. `me` is this connection's id; `waits`
/// maps a waiting session to (its blocker, what it waits on).
fn process(r: Raw, me: &str, waits: &HashMap<String, (String, Option<String>)>) -> ServerProcess {
    let system = r.system.unwrap_or(false) || system_thread(&r);
    let idle = r.command.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("Sleep"));
    let active = !idle && !system;
    let own = r.own.unwrap_or(r.id == me);
    let wait = waits.get(&r.id);
    let state = r.state.clone().filter(|s| s != "0");
    let status = if idle && r.in_trx { Some("inactiva con transacción abierta".to_string()) } else { state.clone().or(r.command.clone()) };
    let command = if active { r.info.as_deref().and_then(first_word).or(r.command.clone()) } else { r.command.clone() };
    let sql = if active {
        r.info.clone()
    } else if idle && r.in_trx {
        r.last_sql.clone().or(r.info.clone())
    } else {
        None
    };
    ServerProcess {
        status,
        active,
        system,
        own,
        user: r.user,
        host: r.host,
        program: r.program,
        database: r.database,
        command,
        elapsed_ms: r.elapsed_ms,
        cpu_ms: r.cpu_ms,
        reads: r.reads,
        wait: wait
            .and_then(|(_, w)| w.clone())
            .or_else(|| r.lock_wait.then(|| "LOCK WAIT".to_string()))
            .or_else(|| state.filter(|s| s.starts_with("Waiting for") && s.contains("lock"))),
        blocked_by: wait.map(|(b, _)| b.clone()),
        sql: sql.map(|q| q.chars().take(MAX_TEXT).collect()),
        id: r.id,
        ..Default::default()
    }
}

pub(crate) async fn processes(s: &mut MySqlSession) -> Result<Vec<ServerProcess>> {
    let tidb = s.variant == Variant::TiDb;
    let qs = queries(s.variant);
    let start = s.processes_query.min(qs.len() - 1);
    let mut rows = None;
    let mut first_err = None;
    for (i, sql) in qs.iter().enumerate().skip(start) {
        match s.rows(sql).await {
            Ok(r) => {
                s.processes_query = i;
                rows = Some(r);
                break;
            }
            Err(e) => {
                tracing::debug!("{:?} processes: {e}", s.variant);
                first_err.get_or_insert(e);
            }
        }
    }
    let Some(rows) = rows else {
        return Err(first_err.unwrap_or_else(|| Error::Query("no se pudo leer la lista de procesos".into())));
    };
    let mut raws: Vec<Raw> = rows.iter().take(MAX_ROWS).map(|r| raw(r, tidb)).filter(|r| !r.id.is_empty()).collect();
    raws.dedup_by(|a, b| a.id == b.id);

    // The last statement of idle sessions holding a transaction, where the
    // list didn't bring it (MariaDB, with performance_schema on).
    let missing: Vec<u64> = raws
        .iter()
        .filter(|r| r.in_trx && r.last_sql.is_none() && r.command.as_deref() == Some("Sleep"))
        .filter_map(|r| r.id.parse().ok())
        .collect();
    if !missing.is_empty() && s.variant.is_mysql_server() {
        let list = missing.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT t.PROCESSLIST_ID, LEFT(e.SQL_TEXT, {MAX_TEXT})
               FROM performance_schema.threads t
               JOIN performance_schema.events_statements_current e ON e.THREAD_ID = t.THREAD_ID
              WHERE t.PROCESSLIST_ID IN ({list})"
        );
        for r in s.optional_rows(&sql).await {
            let (Some(id), Some(q)) = (crate::session::at(&r, 0), crate::session::at(&r, 1)) else { continue };
            if let Some(p) = raws.iter_mut().find(|p| p.id == id) {
                p.last_sql = Some(q);
            }
        }
    }

    // Who blocks whom, only when someone waits on a lock.
    let mut waits = HashMap::new();
    let waiting = raws.iter().any(|r| r.lock_wait || r.state.as_deref().is_some_and(|st| st.starts_with("Waiting for") && st.contains("lock")));
    if waiting && s.variant.has_lock_waits() {
        match s.wait_edges().await {
            Ok(edges) => {
                for e in edges {
                    waits.entry(e.waiter.to_string()).or_insert((e.blocker.to_string(), e.wait));
                }
            }
            Err(e) => tracing::debug!("{:?} processes: lock waits: {e}", s.variant),
        }
    }
    let me = s.conn.id().to_string();
    Ok(raws.into_iter().map(|r| process(r, &me, &waits)).collect())
}

/// A Databend session id: a UUID (letters, digits and dashes).
pub(crate) fn databend_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(id)
    } else {
        Err(Error::Query(format!("«{id}» no es un id de sesión de Databend")))
    }
}

/// A GreptimeDB process id: `<frontend address>/<number>`.
fn greptime_id(id: &str) -> Result<&str> {
    let id = id.trim();
    let ok = id.rsplit_once('/').is_some_and(|(addr, n)| {
        !addr.is_empty()
            && addr.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_' | '[' | ']'))
            && !n.is_empty()
            && n.chars().all(|c| c.is_ascii_digit())
    });
    if ok {
        Ok(id)
    } else {
        Err(Error::Query(format!("«{id}» no es un id de proceso de GreptimeDB")))
    }
}

pub(crate) async fn cancel(s: &mut MySqlSession, id: &str) -> Result<()> {
    let own = || Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into());
    let sql = match s.variant {
        // Its own listing query is over by now: nothing of DBine's to hit.
        Variant::GreptimeDb => format!("KILL '{}'", greptime_id(id)?),
        Variant::Databend => {
            let id = databend_id(id)?;
            let me = s.rows("SELECT connection_id()").await?.first().and_then(|r| crate::session::at(r, 0));
            if me.as_deref() == Some(id) {
                return Err(own());
            }
            format!("KILL QUERY '{id}'")
        }
        v => {
            let n = session_id(id)?;
            let me = s.rows("SELECT CONNECTION_ID()").await?.first().and_then(|r| crate::session::at(r, 0));
            if me.and_then(|m| m.parse::<u64>().ok()) == Some(n) {
                return Err(own());
            }
            match v {
                Variant::TiDb => format!("KILL TIDB QUERY {n}"),
                Variant::Manticore => format!("KILL {n}"),
                _ => format!("KILL QUERY {n}"),
            }
        }
    };
    s.rows(&sql).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_ms() {
        assert_eq!(duration_ms("7ms ago"), Some(7));
        assert_eq!(duration_ms("2us"), Some(0));
        assert_eq!(duration_ms("1m 30s"), Some(90_000));
        assert_eq!(duration_ms("3s ago"), Some(3000));
        assert_eq!(duration_ms("soon"), None);
    }

    #[test]
    fn first_words() {
        assert_eq!(first_word("  select 1").as_deref(), Some("SELECT"));
        assert_eq!(first_word("/*+ HINT */ (SELECT 1)").as_deref(), Some("SELECT"));
        assert_eq!(first_word("  "), None);
    }

    #[test]
    fn ids_are_checked() {
        assert!(databend_id("86a90dd3-8a08-4c5f-ae77-f0576fe381a9").is_ok());
        assert!(databend_id("x'; DROP TABLE t; --").is_err());
        assert!(greptime_id("192.168.1.2:4001/7").is_ok());
        assert!(greptime_id("[::1]:4001/12").is_ok());
        assert!(greptime_id("a/b").is_err());
        assert!(greptime_id("x'/1").is_err());
    }

    #[test]
    fn rows_become_processes() {
        let waits = HashMap::from([("12".to_string(), ("9".to_string(), Some("LOCK WAIT RECORD X".to_string())))]);
        let running = Raw {
            id: "12".into(),
            command: Some("Query".into()),
            state: Some("updating".into()),
            info: Some("update t set v = 1".into()),
            lock_wait: true,
            in_trx: true,
            ..Default::default()
        };
        let p = process(running, "5", &waits);
        assert!(p.active && !p.own && !p.system);
        assert_eq!(p.command.as_deref(), Some("UPDATE"));
        assert_eq!(p.blocked_by.as_deref(), Some("9"));
        assert_eq!(p.wait.as_deref(), Some("LOCK WAIT RECORD X"));
        assert_eq!(p.sql.as_deref(), Some("update t set v = 1"));

        let idle = Raw { id: "9".into(), command: Some("Sleep".into()), in_trx: true, last_sql: Some("update t".into()), ..Default::default() };
        let p = process(idle, "5", &waits);
        assert!(!p.active);
        assert_eq!(p.status.as_deref(), Some("inactiva con transacción abierta"));
        assert_eq!(p.sql.as_deref(), Some("update t"));

        let sleeping = Raw { id: "5".into(), command: Some("Sleep".into()), info: Some("old".into()), ..Default::default() };
        let p = process(sleeping, "5", &waits);
        assert!(p.own && p.sql.is_none() && p.status.as_deref() == Some("Sleep"));

        let daemon = Raw { id: "1".into(), user: Some("event_scheduler".into()), command: Some("Daemon".into()), ..Default::default() };
        let p = process(daemon, "5", &waits);
        assert!(p.system && !p.active);
    }

    #[test]
    fn only_engines_with_sessions_end_them() {
        assert!(can_kill(Variant::MySql) && can_kill(Variant::Doris) && can_kill(Variant::Databend));
        assert!(!can_kill(Variant::Manticore) && !can_kill(Variant::GreptimeDb));
        for v in Variant::ALL {
            assert!(!queries(v.base()).is_empty());
        }
    }
}

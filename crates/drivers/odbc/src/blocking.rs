//! Blocking chains (`Session::blocking`) and ending sessions
//! (`Session::kill_session`) for the presets whose system views say who
//! waits for whom:
//!
//! - Db2 LUW: `SYSIBMADM.MON_LOCKWAITS`, with `MON_GET_CONNECTION` /
//!   `MON_GET_UNIT_OF_WORK` for the client and the state; ends a session
//!   with `ADMIN_CMD('FORCE APPLICATION (<handle>)')`.
//! - Sybase ASE: `master..sysprocesses.blocked`, the statement from
//!   `monProcessSQLText` (MDA) when enabled; `KILL <spid>`.
//! - SQL Anywhere: `sa_conn_info().BlockedOn`; `DROP CONNECTION <n>`.
//! - Informix and GBase 8s: `sysmaster:syslocks` (owner and first waiter),
//!   no statement text; ending a session needs the sysadmin database's
//!   `task('onmode', 'z', …)`, which only runs connected to sysadmin.
//!
//! Everything goes through [`Source`], so it's tested with canned sets.

use crate::design::{eng, Eng};
use crate::monitor::{parse, Source};
use crate::presets::Preset;
use dbine_driver::{BlockedSession, Error, Result};

const IDLE: &str = "inactiva con transacción abierta";

/// The preset reports blocking chains.
pub fn supports_blocking(p: &Preset) -> bool {
    matches!(eng(p), Eng::Db2 | Eng::Ase | Eng::Sqla | Eng::Informix)
}

/// The preset can end another session from SQL on this connection.
pub fn supports_kill(p: &Preset) -> bool {
    matches!(eng(p), Eng::Db2 | Eng::Ase | Eng::Sqla)
}

/// A session in (or maybe in) a chain.
#[derive(Debug, Clone, Default, PartialEq)]
struct Node {
    id: u64,
    blocked_by: Option<u64>,
    user: Option<String>,
    client: Option<String>,
    database: Option<String>,
    wait: Option<String>,
    waited_ms: Option<u64>,
    object: Option<String>,
    sql: Option<String>,
    /// Holds a transaction without running anything.
    idle: bool,
}

fn int(s: Option<&str>) -> Option<u64> {
    let t = s?.trim();
    (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten()
}

fn ms_from_secs(s: Option<&str>) -> Option<u64> {
    s.and_then(parse).map(|v| (v.max(0.0) * 1000.0) as u64)
}

fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

fn qualified(schema: Option<&str>, name: Option<&str>) -> Option<String> {
    match (schema, name) {
        (Some(s), Some(n)) => Some(format!("{s}.{n}")),
        (s, n) => owned(n.or(s)),
    }
}

/// Waiting sessions first, then the heads (sessions others wait for that
/// wait for no one); everything else is dropped.
fn chain(nodes: Vec<Node>) -> Vec<BlockedSession> {
    let blockers: Vec<u64> = nodes.iter().filter_map(|n| n.blocked_by).collect();
    let to_session = |n: Node| BlockedSession {
        id: n.id.to_string(),
        blocked_by: n.blocked_by.map(|b| b.to_string()),
        user: n.user,
        client: n.client,
        database: n.database,
        wait: if n.blocked_by.is_none() && n.idle { Some(IDLE.into()) } else { n.wait },
        waited_ms: n.waited_ms,
        object: if n.blocked_by.is_some() { n.object } else { None },
        sql: n.sql,
    };
    let mut seen: Vec<u64> = Vec::new();
    let mut out = Vec::new();
    for n in nodes.iter().filter(|n| n.blocked_by.is_some_and(|b| b != n.id)) {
        if !seen.contains(&n.id) {
            seen.push(n.id);
            out.push(to_session(n.clone()));
        }
    }
    for b in blockers {
        if seen.contains(&b) {
            continue;
        }
        seen.push(b);
        let head = nodes.iter().find(|n| n.id == b && n.blocked_by.is_none()).cloned();
        out.push(to_session(head.unwrap_or(Node { id: b, ..Default::default() })));
    }
    out
}

fn ids_list(nodes: &[Node]) -> String {
    let mut ids: Vec<u64> = nodes.iter().flat_map(|n| [Some(n.id), n.blocked_by]).flatten().collect();
    ids.sort_unstable();
    ids.dedup();
    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

pub fn collect(p: &Preset, src: &mut dyn Source) -> Result<Vec<BlockedSession>> {
    let nodes = match eng(p) {
        Eng::Db2 => db2(src)?,
        Eng::Ase => ase(src)?,
        Eng::Sqla => sqla(src)?,
        Eng::Informix => informix(src)?,
        _ => return Err(Error::Unsupported("este motor no informa bloqueos entre sesiones".into())),
    };
    Ok(chain(nodes))
}

fn denied(what: &str, e: Error) -> Error {
    Error::Query(format!("No se pudieron leer los bloqueos ({what}): {e}"))
}

fn db2(src: &mut dyn Source) -> Result<Vec<Node>> {
    let w = src.query("SELECT * FROM SYSIBMADM.MON_LOCKWAITS").map_err(|e| denied("SYSIBMADM.MON_LOCKWAITS", e))?;
    let mut nodes = Vec::new();
    for r in 0..w.rows.len() {
        let (Some(req), Some(hld)) = (int(w.get(r, "REQ_APPLICATION_HANDLE")), int(w.get(r, "HLD_APPLICATION_HANDLE")))
        else {
            continue;
        };
        let wait = match (w.get(r, "LOCK_MODE_REQUESTED"), w.get(r, "LOCK_OBJECT_TYPE")) {
            (Some(m), Some(t)) => Some(format!("LOCK WAIT {m} ({t})")),
            (m, t) => m.or(t).map(|x| format!("LOCK WAIT {x}")),
        };
        nodes.push(Node {
            id: req,
            blocked_by: Some(hld),
            user: owned(w.get(r, "REQ_USERID")),
            client: owned(w.get(r, "REQ_APPLICATION_NAME")),
            wait,
            waited_ms: ms_from_secs(w.get(r, "LOCK_WAIT_ELAPSED_TIME")),
            object: qualified(w.get(r, "TABSCHEMA"), w.get(r, "TABNAME")),
            sql: owned(w.get(r, "REQ_STMT_TEXT")),
            ..Default::default()
        });
        if !nodes.iter().any(|n| n.id == hld && n.blocked_by.is_none()) {
            nodes.push(Node {
                id: hld,
                user: owned(w.get(r, "HLD_USERID")),
                client: owned(w.get(r, "HLD_APPLICATION_NAME")),
                sql: owned(w.get(r, "HLD_CURRENT_STMT_TEXT")),
                ..Default::default()
            });
        }
    }
    if nodes.is_empty() {
        return Ok(nodes);
    }
    // Client host, database and unit-of-work state (UOWWAIT: idle with an
    // open transaction). Optional: without them the chain still shows.
    let sql = format!(
        "SELECT C.APPLICATION_HANDLE, C.SESSION_AUTH_ID, C.CLIENT_HOSTNAME, C.APPLICATION_NAME, CURRENT SERVER AS DB,
                U.WORKLOAD_OCCURRENCE_STATE AS STATE,
                TIMESTAMPDIFF(2, CHAR(CURRENT TIMESTAMP - U.UOW_START_TIME)) AS UOW_SECS
           FROM TABLE(MON_GET_CONNECTION(NULL, -1)) AS C
           LEFT JOIN TABLE(MON_GET_UNIT_OF_WORK(NULL, -1)) AS U ON U.APPLICATION_HANDLE = C.APPLICATION_HANDLE
          WHERE C.APPLICATION_HANDLE IN ({})",
        ids_list(&nodes)
    );
    if let Ok(c) = src.query(&sql) {
        for r in 0..c.rows.len() {
            let Some(id) = int(c.get(r, "APPLICATION_HANDLE")) else { continue };
            let state = c.get(r, "STATE");
            for n in nodes.iter_mut().filter(|n| n.id == id) {
                n.user = n.user.take().or_else(|| owned(c.get(r, "SESSION_AUTH_ID")));
                let app = n.client.take().or_else(|| owned(c.get(r, "APPLICATION_NAME")));
                n.client = match (c.get(r, "CLIENT_HOSTNAME"), app) {
                    (Some(h), Some(a)) => Some(format!("{h} · {a}")),
                    (h, a) => a.or_else(|| owned(h)),
                };
                n.database = owned(c.get(r, "DB"));
                if n.blocked_by.is_none() {
                    n.idle = state.is_some_and(|s| s.eq_ignore_ascii_case("UOWWAIT"));
                    n.wait = owned(state);
                    n.waited_ms = ms_from_secs(c.get(r, "UOW_SECS"));
                }
            }
        }
    }
    Ok(nodes)
}

fn ase(src: &mut dyn Source) -> Result<Vec<Node>> {
    let p = src
        .query(
            "SELECT spid, blocked, suser_name(suid) AS usr, hostname, program_name, db_name(dbid) AS db,
                    status, cmd, time_blocked, tran_name
               FROM master..sysprocesses
              WHERE blocked > 0 OR spid IN (SELECT blocked FROM master..sysprocesses WHERE blocked > 0)",
        )
        .map_err(|e| denied("master..sysprocesses", e))?;
    let mut nodes: Vec<Node> = (0..p.rows.len())
        .filter_map(|r| {
            let id = int(p.get(r, "spid"))?;
            let blocked_by = int(p.get(r, "blocked")).filter(|b| *b > 0);
            let status = p.get(r, "status");
            let client = match (p.get(r, "hostname"), p.get(r, "program_name")) {
                (Some(h), Some(a)) => Some(format!("{h} · {a}")),
                (h, a) => owned(h.or(a)),
            };
            Some(Node {
                id,
                blocked_by,
                user: owned(p.get(r, "usr")),
                client,
                database: owned(p.get(r, "db")),
                wait: owned(status.or(p.get(r, "cmd"))),
                waited_ms: blocked_by.and(ms_from_secs(p.get(r, "time_blocked"))),
                idle: status.is_some_and(|s| s.to_ascii_lowercase().contains("recv sleep")),
                ..Default::default()
            })
        })
        .collect();
    if nodes.is_empty() {
        return Ok(nodes);
    }
    // The running batch's text: MDA tables, only with monitoring enabled.
    let sql = format!(
        "SELECT SPID, SQLText FROM master..monProcessSQLText WHERE SPID IN ({})
          ORDER BY SPID, BatchID, LineNumber, SequenceInLine",
        ids_list(&nodes)
    );
    if let Ok(t) = src.query(&sql) {
        for r in 0..t.rows.len() {
            let (Some(id), Some(text)) = (int(t.get(r, "SPID")), t.rows[r].get(1).cloned().flatten()) else { continue };
            for n in nodes.iter_mut().filter(|n| n.id == id) {
                n.sql.get_or_insert_with(String::new).push_str(&text);
            }
        }
    }
    Ok(nodes)
}

fn sqla(src: &mut dyn Source) -> Result<Vec<Node>> {
    let c = src
        .query(
            "SELECT Number, Userid, NodeAddr, Name, DB_NAME(DBNumber) AS db, BlockedOn, LockTable, LockRowID,
                    ReqStatus, UncommitOps,
                    CAST(LEFT(CONNECTION_PROPERTY('LastStatement', Number), 2000) AS LONG VARCHAR) AS stmt
               FROM sa_conn_info()",
        )
        .map_err(|e| denied("sa_conn_info", e))?;
    Ok((0..c.rows.len())
        .filter_map(|r| {
            let id = int(c.get(r, "Number"))?;
            let blocked_by = int(c.get(r, "BlockedOn")).filter(|b| *b > 0);
            let status = c.get(r, "ReqStatus");
            let object = match (c.get(r, "LockTable"), c.get(r, "LockRowID").filter(|v| *v != "0")) {
                (Some(t), Some(row)) => Some(format!("{t} · {row}")),
                (t, _) => owned(t),
            };
            Some(Node {
                id,
                blocked_by,
                user: owned(c.get(r, "Userid")),
                client: owned(c.get(r, "NodeAddr").or(c.get(r, "Name"))),
                database: owned(c.get(r, "db")),
                wait: owned(status),
                object,
                sql: owned(c.get(r, "stmt")),
                idle: status.is_some_and(|s| s.eq_ignore_ascii_case("Idle")),
                ..Default::default()
            })
        })
        .collect())
}

fn informix(src: &mut dyn Source) -> Result<Vec<Node>> {
    let l = src
        .query("SELECT owner, waiter, dbsname, tabname, type, rowidlk FROM sysmaster:syslocks WHERE waiter IS NOT NULL")
        .map_err(|e| denied("sysmaster:syslocks", e))?;
    let mut nodes = Vec::new();
    for r in 0..l.rows.len() {
        let (Some(owner), Some(waiter)) = (int(l.get(r, "owner")), int(l.get(r, "waiter"))) else { continue };
        let mut object = match (l.get(r, "dbsname"), l.get(r, "tabname")) {
            (Some(d), Some(t)) => Some(format!("{d}:{t}")),
            (d, t) => owned(t.or(d)),
        };
        if let (Some(o), Some(row)) = (object.as_mut(), l.get(r, "rowidlk").filter(|v| *v != "0")) {
            o.push_str(&format!(" · {row}"));
        }
        nodes.push(Node {
            id: waiter,
            blocked_by: Some(owner),
            wait: l.get(r, "type").map(|t| format!("LOCK WAIT {t}")),
            object,
            ..Default::default()
        });
        nodes.push(Node { id: owner, ..Default::default() });
    }
    if nodes.is_empty() {
        return Ok(nodes);
    }
    let sql = format!("SELECT sid, username, hostname FROM sysmaster:syssessions WHERE sid IN ({})", ids_list(&nodes));
    if let Ok(s) = src.query(&sql) {
        for r in 0..s.rows.len() {
            let Some(id) = int(s.get(r, "sid")) else { continue };
            for n in nodes.iter_mut().filter(|n| n.id == id) {
                n.user = owned(s.get(r, "username"));
                n.client = owned(s.get(r, "hostname"));
            }
        }
    }
    Ok(nodes)
}

/// The statement that ends session `id`, after checking it's a number.
pub fn kill_sql(p: &Preset, id: &str) -> Result<String> {
    let n = int(Some(id)).ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión válido")))?;
    Ok(match eng(p) {
        Eng::Db2 => format!("CALL SYSPROC.ADMIN_CMD('FORCE APPLICATION ({n})')"),
        Eng::Ase => format!("KILL {n}"),
        Eng::Sqla => format!("DROP CONNECTION {n}"),
        _ => return Err(Error::Unsupported("este motor no permite terminar sesiones desde DBine".into())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Set;
    use crate::presets::PRESETS;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    /// Answers the first query containing a needle; any other fails.
    struct Fake(Vec<(&'static str, Set)>);

    impl Source for Fake {
        fn query(&mut self, sql: &str) -> Result<Set> {
            self.0
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .map(|(_, s)| s.clone())
                .ok_or_else(|| Error::Query("objeto inexistente".into()))
        }
    }

    #[test]
    fn capabilities_follow_the_engine() {
        for id in ["db2", "sybase", "sqlanywhere"] {
            assert!(supports_blocking(preset(id)) && supports_kill(preset(id)), "{id}");
        }
        for id in ["informix", "gbase8s"] {
            assert!(supports_blocking(preset(id)) && !supports_kill(preset(id)), "{id}");
        }
        for id in ["odbc", "db2i", "db2zos", "teradata", "vertica"] {
            assert!(!supports_blocking(preset(id)) && !supports_kill(preset(id)), "{id}");
        }
    }

    #[test]
    fn kill_ids_are_numbers() {
        assert_eq!(kill_sql(preset("db2"), " 77 ").unwrap(), "CALL SYSPROC.ADMIN_CMD('FORCE APPLICATION (77)')");
        assert_eq!(kill_sql(preset("sybase"), "12").unwrap(), "KILL 12");
        assert_eq!(kill_sql(preset("sqlanywhere"), "5").unwrap(), "DROP CONNECTION 5");
        for bad in ["", "1; DROP TABLE x", "1)') --", "-1"] {
            assert!(kill_sql(preset("db2"), bad).is_err(), "{bad}");
        }
        assert!(matches!(kill_sql(preset("informix"), "3"), Err(Error::Unsupported(_))));
    }

    #[test]
    fn db2_lock_waits() {
        let waits = Set::new(
            &["REQ_APPLICATION_HANDLE", "HLD_APPLICATION_HANDLE", "LOCK_OBJECT_TYPE", "LOCK_MODE_REQUESTED", "TABSCHEMA", "TABNAME", "LOCK_WAIT_ELAPSED_TIME", "REQ_STMT_TEXT", "HLD_CURRENT_STMT_TEXT"],
            vec![vec![Some("21"), Some("20"), Some("ROW"), Some("X"), Some("APP"), Some("T"), Some("3"), Some("UPDATE T SET V = 2"), Some("UPDATE T SET V = 1")]],
        );
        let conns = Set::new(
            &["APPLICATION_HANDLE", "SESSION_AUTH_ID", "CLIENT_HOSTNAME", "APPLICATION_NAME", "DB", "STATE", "UOW_SECS"],
            vec![
                vec![Some("20"), Some("BOB"), Some("pc1"), Some("app"), Some("SAMPLE"), Some("UOWWAIT"), Some("9")],
                vec![Some("21"), Some("ANA"), Some("pc2"), Some("app"), Some("SAMPLE"), Some("LOCKWAIT"), Some("4")],
            ],
        );
        let mut src = Fake(vec![("MON_LOCKWAITS", waits), ("MON_GET_CONNECTION", conns)]);
        let c = collect(preset("db2"), &mut src).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].id.as_str(), c[0].blocked_by.as_deref()), ("21", Some("20")));
        assert_eq!(c[0].waited_ms, Some(3000));
        assert_eq!(c[0].object.as_deref(), Some("APP.T"));
        assert_eq!(c[0].wait.as_deref(), Some("LOCK WAIT X (ROW)"));
        assert_eq!(c[0].client.as_deref(), Some("pc2 · app"));
        assert_eq!(c[1].id, "20");
        assert_eq!(c[1].wait.as_deref(), Some(IDLE));
        assert_eq!(c[1].waited_ms, Some(9000));
        assert_eq!(c[1].sql.as_deref(), Some("UPDATE T SET V = 1"));
    }

    #[test]
    fn ase_sysprocesses() {
        let procs = Set::new(
            &["spid", "blocked", "usr", "hostname", "program_name", "db", "status", "cmd", "time_blocked", "tran_name"],
            vec![
                vec![Some("15"), Some("0"), Some("sa"), Some("pc1"), Some("isql"), Some("app"), Some("recv sleep"), Some("AWAITING COMMAND"), None, Some("$user_transaction")],
                vec![Some("16"), Some("15"), Some("sa"), Some("pc2"), Some("isql"), Some("app"), Some("lock sleep"), Some("UPDATE"), Some("7"), None],
            ],
        );
        let text = Set::new(&["SPID", "SQLText"], vec![vec![Some("16"), Some("update t set v = 2 ")], vec![Some("16"), Some("where id = 1")]]);
        let mut src = Fake(vec![("sysprocesses", procs), ("monProcessSQLText", text)]);
        let c = collect(preset("sybase"), &mut src).unwrap();
        assert_eq!(c.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["16", "15"]);
        assert_eq!(c[0].waited_ms, Some(7000));
        assert_eq!(c[0].sql.as_deref(), Some("update t set v = 2 where id = 1"));
        assert_eq!(c[1].wait.as_deref(), Some(IDLE));
    }

    #[test]
    fn sqla_and_informix() {
        let conns = Set::new(
            &["Number", "Userid", "NodeAddr", "Name", "db", "BlockedOn", "LockTable", "LockRowID", "ReqStatus", "UncommitOps", "stmt"],
            vec![
                vec![Some("1"), Some("DBA"), Some("10.0.0.1"), None, Some("demo"), Some("0"), None, None, Some("Idle"), Some("1"), Some("update t set v = 1")],
                vec![Some("2"), Some("DBA"), Some("10.0.0.2"), None, Some("demo"), Some("1"), Some("DBA.t"), Some("42"), Some("BlockedLock"), Some("0"), None],
                vec![Some("3"), Some("DBA"), None, None, Some("demo"), Some("0"), None, None, Some("Idle"), Some("0"), None],
            ],
        );
        let c = collect(preset("sqlanywhere"), &mut Fake(vec![("sa_conn_info", conns)])).unwrap();
        assert_eq!(c.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["2", "1"]);
        assert_eq!(c[0].object.as_deref(), Some("DBA.t · 42"));
        assert_eq!(c[1].wait.as_deref(), Some(IDLE));

        let locks = Set::new(
            &["owner", "waiter", "dbsname", "tabname", "type", "rowidlk"],
            vec![vec![Some("40"), Some("41"), Some("stores"), Some("customer"), Some("X"), Some("257")]],
        );
        let sess = Set::new(&["sid", "username", "hostname"], vec![vec![Some("41"), Some("informix"), Some("pc")]]);
        let c = collect(preset("informix"), &mut Fake(vec![("syslocks", locks), ("syssessions", sess)])).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].object.as_deref(), Some("stores:customer · 257"));
        assert_eq!(c[0].user.as_deref(), Some("informix"));
        assert_eq!(c[1].blocked_by, None);
    }

    #[test]
    fn nothing_blocked_is_empty() {
        let empty = Set::new(&["REQ_APPLICATION_HANDLE", "HLD_APPLICATION_HANDLE"], vec![]);
        assert!(collect(preset("db2"), &mut Fake(vec![("MON_LOCKWAITS", empty)])).unwrap().is_empty());
        assert!(collect(preset("db2"), &mut Fake(vec![])).is_err());
        assert!(matches!(collect(preset("teradata"), &mut Fake(vec![])), Err(Error::Unsupported(_))));
    }
}

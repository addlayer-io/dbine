//! What the user may do (`Session::permissions`).
//!
//! - **Neo4j Enterprise:** `SHOW USER PRIVILEGES` (the current user's, with
//!   its roles', which any user may read). A privilege counts when it's
//!   granted on every graph (`*`) or on the explorer's database and, for
//!   transactions, for every user (`USER(*)`); a DENY on the same scope
//!   wins. A grant or deny on part of it (some users, `HOME`/`DEFAULT`)
//!   leaves the action unknown.
//!   - profiler: SHOW TRANSACTION (without it `SHOW TRANSACTIONS` only
//!     shows the user's own). Kill: TERMINATE TRANSACTION. Both also
//!     through TRANSACTION MANAGEMENT or ALL DATABASE PRIVILEGES.
//!   - create / drop: CREATE / DROP DATABASE, DATABASE MANAGEMENT or ALL
//!     DBMS PRIVILEGES.
//!   - security: USER MANAGEMENT, CREATE USER or ALL DBMS PRIVILEGES; with
//!     only other user, role or privilege management it's unknown.
//! - **Neo4j Community:** no roles or privileges, every user may do
//!   everything; it has a single database, so creating and dropping one
//!   stays unknown (the server refuses it for any user).
//! - **Memgraph:** without users (`SHOW CURRENT USER` is null) or without
//!   an Enterprise license every user may do everything. Enterprise: `SHOW
//!   PRIVILEGES FOR <me>` (it needs AUTH; without it, unknown): DURABILITY
//!   for snapshots (backup and restore), TRANSACTION_MANAGEMENT for the
//!   profiler, MULTI_DATABASE_EDIT for creating and dropping databases,
//!   AUTH for security.
//! - **Neptune:** AWS IAM decides: unknown.
//!
//! A check the server refuses leaves the fields unknown; only a broken
//! connection is an error.

use crate::{as_text, Flavor, GraphSession};
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::{Map, Value};

/// One row of `SHOW USER PRIVILEGES`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Privilege {
    pub granted: bool,
    pub action: String,
    pub graph: String,
    pub segment: String,
}

/// How a privilege row covers what an action needs.
#[derive(Clone, Copy, PartialEq)]
enum Cover {
    None,
    Part,
    Full,
}

/// `database`: the explorer's (`None`: all of them).
fn graph_cover(graph: &str, database: Option<&str>, dbms: bool) -> Cover {
    if graph == "*" {
        Cover::Full
    } else if dbms {
        // DBMS privileges are listed with graph `*` only.
        Cover::None
    } else if matches!(graph, "HOME" | "DEFAULT") {
        Cover::Part
    } else {
        match database {
            Some(db) if db.eq_ignore_ascii_case(graph) => Cover::Full,
            Some(_) => Cover::None,
            // Granted on some database, the action is on all of them.
            None => Cover::Part,
        }
    }
}

fn segment_cover(segment: &str) -> Cover {
    match segment {
        "database" | "USER(*)" | "" => Cover::Full,
        s if s.starts_with("USER(") => Cover::Part,
        _ => Cover::Full,
    }
}

/// The access to an action any of `actions` gives.
fn neo4j_access(privs: &[Privilege], actions: &[&str], database: Option<&str>, dbms: bool, missing: &str) -> Access {
    let cover = |p: &Privilege| {
        if !actions.contains(&p.action.as_str()) {
            return Cover::None;
        }
        match (graph_cover(&p.graph, database, dbms), segment_cover(&p.segment)) {
            (Cover::None, _) | (_, Cover::None) => Cover::None,
            (Cover::Full, Cover::Full) => Cover::Full,
            _ => Cover::Part,
        }
    };
    let (mut granted, mut granted_part, mut denied_part) = (false, false, false);
    for p in privs {
        match (p.granted, cover(p)) {
            (_, Cover::None) => {}
            (false, Cover::Full) => return Access::check(false, format!("{missing} (denegado)")),
            (false, Cover::Part) => denied_part = true,
            (true, Cover::Full) => granted = true,
            (true, Cover::Part) => granted_part = true,
        }
    }
    if granted && !denied_part {
        Access::Allowed
    } else if granted || granted_part {
        Access::Unknown
    } else {
        Access::check(false, missing)
    }
}

pub(crate) fn neo4j_decide(privs: &[Privilege], database: Option<&str>) -> Permissions {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    const TX: &[&str] = &["transaction_management", "database_actions"];
    let with = |a: &'static str| -> Vec<&'static str> { std::iter::once(a).chain(TX.iter().copied()).collect() };
    // Anything that changes users, roles or privileges.
    let manages = |a: &str| {
        a == "dbms_actions" || (!a.starts_with("show_") && (a.contains("user") || a.contains("role") || a.contains("privilege") || a.contains("password")))
    };
    let security = match neo4j_access(privs, &["user_management", "create_user", "dbms_actions"], None, true, "USER MANAGEMENT") {
        Access::Denied { .. } if privs.iter().any(|p| p.granted && manages(&p.action)) => Access::Unknown,
        other => other,
    };
    Permissions {
        profiler: neo4j_access(privs, &with("show_transaction"), database, false, "SHOW TRANSACTION"),
        kill_session: neo4j_access(privs, &with("terminate_transaction"), database, false, "TERMINATE TRANSACTION"),
        create_database: neo4j_access(privs, &["create_database", "database_management", "dbms_actions"], None, true, "CREATE DATABASE"),
        drop_database: match database {
            Some(_) => neo4j_access(privs, &["drop_database", "database_management", "dbms_actions"], None, true, "DROP DATABASE"),
            None => Access::Unknown,
        },
        manage_security: security,
        ..Default::default()
    }
}

/// Neo4j Community: a single database and no privileges.
fn neo4j_community() -> Permissions {
    Permissions { profiler: Access::Allowed, kill_session: Access::Allowed, manage_security: Access::Allowed, ..Default::default() }
}

/// Memgraph without authorization (no users, or Community): every user may
/// do everything but creating and dropping databases, which Community
/// doesn't have.
fn memgraph_open(enterprise_dbs: bool) -> Permissions {
    let dbs = if enterprise_dbs { Access::Allowed } else { Access::Unknown };
    Permissions {
        backup: Access::Allowed,
        restore: Access::Allowed,
        profiler: Access::Allowed,
        create_database: dbs.clone(),
        drop_database: dbs,
        manage_security: Access::Allowed,
        ..Default::default()
    }
}

/// Memgraph Enterprise: `(privilege, effective)` of `SHOW PRIVILEGES FOR`
/// (`GRANT` or `DENY`; fine-grained label rows left out).
pub(crate) fn memgraph_decide(rows: &[(String, String)], database: Option<&str>) -> Permissions {
    let has = |name: &str| rows.iter().any(|(p, e)| p.eq_ignore_ascii_case(name) && e.eq_ignore_ascii_case("GRANT"));
    let snapshots = Access::check(has("DURABILITY"), "DURABILITY");
    let dbs = Access::check(has("MULTI_DATABASE_EDIT"), "MULTI_DATABASE_EDIT");
    Permissions {
        backup: snapshots.clone(),
        restore: snapshots,
        profiler: Access::check(has("TRANSACTION_MANAGEMENT"), "TRANSACTION_MANAGEMENT"),
        create_database: dbs.clone(),
        drop_database: if database.is_some_and(|d| !d.trim().is_empty()) { dbs } else { Access::Unknown },
        manage_security: Access::check(has("AUTH"), "AUTH"),
        ..Default::default()
    }
}

fn text(r: &Map<String, Value>, k: &str) -> String {
    r.get(k).map(as_text).unwrap_or_default()
}

impl GraphSession {
    /// Rows by column name; `None` when the server refused the statement.
    async fn permission_rows(&mut self, q: &str, db: Option<&str>) -> Result<std::result::Result<Vec<Map<String, Value>>, String>> {
        match self.query_on(q, db).await {
            Ok((cols, rows)) => Ok(Ok(rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect())),
            Err(e @ Error::Connect(_)) => Err(e),
            Err(e) => {
                tracing::debug!("graph: permissions check refused: {e}");
                Ok(Err(e.to_string()))
            }
        }
    }
}

async fn neo4j(s: &mut GraphSession, database: Option<&str>) -> Result<Permissions> {
    let Ok(ed) = s.permission_rows("CALL dbms.components() YIELD edition RETURN edition", Some("system")).await? else {
        return Ok(Permissions::default());
    };
    if ed.first().is_some_and(|r| text(r, "edition").eq_ignore_ascii_case("community")) {
        return Ok(neo4j_community());
    }
    let q = "SHOW USER PRIVILEGES YIELD access, action, graph, segment";
    let Ok(rows) = s.permission_rows(q, Some("system")).await? else {
        return Ok(Permissions::default());
    };
    let privs: Vec<Privilege> = rows
        .iter()
        .map(|r| Privilege {
            granted: text(r, "access").eq_ignore_ascii_case("GRANTED"),
            action: text(r, "action"),
            graph: text(r, "graph"),
            segment: text(r, "segment"),
        })
        .collect();
    Ok(neo4j_decide(&privs, database))
}

async fn memgraph(s: &mut GraphSession, database: Option<&str>) -> Result<Permissions> {
    let Ok(me) = s.permission_rows("SHOW CURRENT USER", None).await? else {
        return Ok(Permissions::default());
    };
    let Some(user) = me.first().map(|r| text(r, "user")).filter(|u| !u.is_empty()) else {
        // No users: authentication is off. Multi-tenancy may still be
        // Enterprise only: unknown.
        return Ok(memgraph_open(false));
    };
    let q = format!("SHOW PRIVILEGES FOR {}", crate::cypher::ident(&user));
    match s.permission_rows(&q, None).await? {
        Ok(rows) => {
            let rows: Vec<(String, String)> = rows
                .iter()
                .map(|r| (text(r, "privilege"), text(r, "effective")))
                .filter(|(p, _)| !p.starts_with("LABEL ") && !p.starts_with("EDGE_TYPE "))
                .collect();
            Ok(memgraph_decide(&rows, database))
        }
        // Community: privileges aren't enforced.
        Err(m) if m.contains("enterprise") => Ok(memgraph_open(false)),
        Err(_) => Ok(Permissions::default()),
    }
}

pub(crate) async fn check(s: &mut GraphSession, database: Option<&str>) -> Result<Permissions> {
    match s.flavor {
        Flavor::Neo4j => neo4j(s, database).await,
        Flavor::Memgraph => memgraph(s, database).await,
        Flavor::Neptune => Ok(Permissions::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn p(granted: bool, action: &str, graph: &str, segment: &str) -> Privilege {
        Privilege { granted, action: action.into(), graph: graph.into(), segment: segment.into() }
    }

    /// What Neo4j 5 lists for PUBLIC.
    fn public() -> Vec<Privilege> {
        vec![p(true, "access", "HOME", "database"), p(true, "execute", "*", "PROCEDURE(*)"), p(true, "load", "*", "ALL DATA")]
    }

    #[test]
    fn the_admin_role() {
        let mut privs = public();
        privs.extend([p(true, "transaction_management", "*", "USER(*)"), p(true, "dbms_actions", "*", "database"), p(true, "access", "*", "database")]);
        let x = neo4j_decide(&privs, Some("neo4j"));
        assert_eq!((&x.profiler, &x.kill_session, &x.create_database, &x.drop_database, &x.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&x.backup, &x.restore), (&Access::Unknown, &Access::Unknown));
        assert_eq!(neo4j_decide(&privs, None).drop_database, Access::Unknown);
    }

    #[test]
    fn a_reader_is_denied() {
        let mut privs = public();
        privs.extend([p(true, "match", "*", "NODE(*)"), p(true, "access", "*", "database"), p(true, "show_user", "*", "database")]);
        let x = neo4j_decide(&privs, Some("neo4j"));
        assert!(denied(&x.profiler, "SHOW TRANSACTION"));
        assert!(denied(&x.kill_session, "TERMINATE TRANSACTION"));
        assert!(denied(&x.create_database, "CREATE DATABASE"));
        assert!(denied(&x.drop_database, "DROP DATABASE"));
        assert!(denied(&x.manage_security, "USER MANAGEMENT"));
    }

    #[test]
    fn scope_decides() {
        let privs = vec![
            p(true, "show_transaction", "neo4j", "USER(*)"),
            p(true, "terminate_transaction", "*", "USER(bob)"),
            p(true, "create_database", "*", "database"),
        ];
        let x = neo4j_decide(&privs, Some("neo4j"));
        assert_eq!(x.profiler, Access::Allowed);
        // Only bob's transactions: some can be ended.
        assert_eq!(x.kill_session, Access::Unknown);
        assert_eq!(x.create_database, Access::Allowed);
        assert!(denied(&x.drop_database, "DROP DATABASE"));
        // Another database: not granted there.
        assert!(denied(&neo4j_decide(&privs, Some("other")).profiler, "SHOW TRANSACTION"));
        // All databases, granted on one: unknown.
        assert_eq!(neo4j_decide(&privs, None).profiler, Access::Unknown);
        // The home database may be this one.
        let home = vec![p(true, "show_transaction", "HOME", "USER(*)")];
        assert_eq!(neo4j_decide(&home, Some("neo4j")).profiler, Access::Unknown);
    }

    #[test]
    fn denies_win() {
        let privs = vec![p(true, "transaction_management", "*", "USER(*)"), p(false, "show_transaction", "*", "USER(*)")];
        let x = neo4j_decide(&privs, Some("neo4j"));
        assert!(denied(&x.profiler, "SHOW TRANSACTION (denegado)"));
        assert_eq!(x.kill_session, Access::Allowed);
        let privs = vec![p(true, "transaction_management", "*", "USER(*)"), p(false, "terminate_transaction", "*", "USER(ana)")];
        assert_eq!(neo4j_decide(&privs, Some("neo4j")).kill_session, Access::Unknown);
    }

    #[test]
    fn partial_security_is_unknown() {
        let x = neo4j_decide(&[p(true, "role_management", "*", "database")], None);
        assert_eq!(x.manage_security, Access::Unknown);
        let x = neo4j_decide(&[p(true, "create_user", "*", "database")], None);
        assert_eq!(x.manage_security, Access::Allowed);
    }

    #[test]
    fn community_editions() {
        let x = neo4j_community();
        assert_eq!((&x.profiler, &x.kill_session, &x.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&x.create_database, &x.drop_database), (&Access::Unknown, &Access::Unknown));
        let x = memgraph_open(false);
        assert_eq!((&x.backup, &x.restore, &x.profiler, &x.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(x.create_database, Access::Unknown);
    }

    #[test]
    fn memgraph_enterprise() {
        let rows = vec![("DURABILITY".to_string(), "GRANT".to_string()), ("AUTH".into(), "DENY".into()), ("MATCH".into(), "GRANT".into())];
        let x = memgraph_decide(&rows, Some("memgraph"));
        assert_eq!((&x.backup, &x.restore), (&Access::Allowed, &Access::Allowed));
        assert!(denied(&x.profiler, "TRANSACTION_MANAGEMENT"));
        assert!(denied(&x.manage_security, "AUTH"));
        assert!(denied(&x.drop_database, "MULTI_DATABASE_EDIT"));
        assert_eq!(memgraph_decide(&rows, None).drop_database, Access::Unknown);
    }
}

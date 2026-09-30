//! What the login may do (`Session::permissions`). Dremio offers creating
//! and dropping spaces (the databases), the profiler (the job history) and
//! users, roles and grants.
//!
//! - **Dremio OSS (Community)**: every user is an administrator, so spaces
//!   and the whole job history are allowed. It has no SQL for users, roles
//!   or grants (an Enterprise feature, not a missing privilege), so
//!   `manage_security` stays unknown. It's recognized by the lack of
//!   `sys.users`.
//! - **Enterprise**: the login's system privileges from `sys.privileges`,
//!   through its roles (`sys.membership`, plus `PUBLIC`). Members of `ADMIN`
//!   may do everything; otherwise creating a space needs `CREATE SPACE`, the
//!   jobs of others `VIEW JOB HISTORY`, and users and roles `CREATE USER` /
//!   `CREATE ROLE` / `MANAGE GRANTS`. Dropping a space needs ownership,
//!   which a grant listing may not show: allowed with `OWNERSHIP` on it,
//!   unknown otherwise.
//!
//! Every check is a `SELECT`.

use crate::{text, DremioSession};
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};

type Row = Map<String, Value>;

fn field(r: &Row, k: &str) -> String {
    r.get(k).map(text).unwrap_or_default()
}

/// Privilege names as the grammar writes them (`CREATE_SPACE` → `CREATE SPACE`).
fn norm(p: &str) -> String {
    p.trim().to_uppercase().replace('_', " ")
}

pub(crate) fn community() -> Permissions {
    Permissions { create_database: Access::Allowed, drop_database: Access::Allowed, profiler: Access::Allowed, ..Default::default() }
}

/// Enterprise: from the privilege and membership rows.
pub(crate) fn enterprise(user: &str, privileges: &[Row], members: &[Row], database: Option<&str>) -> Permissions {
    // The login and every role it reaches (PUBLIC included).
    let mut grantees: HashSet<(String, &'static str)> = HashSet::new();
    let mut queue = VecDeque::from([(user.to_string(), "USER"), ("PUBLIC".to_string(), "ROLE")]);
    while let Some((n, ty)) = queue.pop_front() {
        if grantees.len() > 256 || !grantees.insert((n.clone(), ty)) {
            continue;
        }
        for m in members.iter().filter(|m| field(m, "member_name") == n && field(m, "member_type").eq_ignore_ascii_case(ty)) {
            queue.push_back((field(m, "role_name"), "ROLE"));
        }
    }
    if grantees.iter().any(|(n, ty)| *ty == "ROLE" && n.eq_ignore_ascii_case("ADMIN")) {
        return Permissions { manage_security: Access::Allowed, ..community() };
    }
    let mine = |r: &&Row| {
        let ty = field(r, "grantee_type").to_uppercase();
        let ty = if ty == "ROLE" { "ROLE" } else { "USER" };
        grantees.contains(&(field(r, "grantee_id"), ty))
    };
    let system: HashSet<String> = privileges
        .iter()
        .filter(mine)
        .filter(|r| matches!(field(r, "object_type").to_uppercase().as_str(), "SYSTEM" | ""))
        .map(|r| norm(&field(r, "privilege")))
        .collect();
    let has = |p: &str| system.contains(p) || system.contains("ALL");
    let owns = database.is_some_and(|db| {
        privileges.iter().filter(mine).any(|r| {
            field(r, "object_type").eq_ignore_ascii_case("SPACE")
                && field(r, "object_id").trim_matches('"') == db
                && matches!(norm(&field(r, "privilege")).as_str(), "OWNERSHIP" | "ALL")
        })
    });
    Permissions {
        create_database: Access::check(has("CREATE SPACE"), "CREATE SPACE"),
        drop_database: if owns { Access::Allowed } else { Access::Unknown },
        profiler: Access::check(has("VIEW JOB HISTORY"), "VIEW JOB HISTORY"),
        manage_security: Access::check(
            has("CREATE USER") || has("CREATE ROLE") || has("MANAGE GRANTS"),
            "CREATE USER, CREATE ROLE o MANAGE GRANTS (o el rol ADMIN)",
        ),
        ..Default::default()
    }
}

/// A failed read is "unknown", except a dead connection.
fn soft<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(_) => Ok(None),
    }
}

pub(crate) async fn check(s: &DremioSession, database: Option<&str>) -> Result<Permissions> {
    let Some(sys_users) = soft(
        s.strings("SELECT TABLE_NAME FROM INFORMATION_SCHEMA.\"TABLES\" WHERE TABLE_SCHEMA = 'sys' AND TABLE_NAME = 'users'").await,
    )?
    else {
        return Ok(Permissions::default());
    };
    if sys_users.is_empty() {
        return Ok(community());
    }
    let user = soft(s.strings("SELECT query_user()").await)?
        .and_then(|rows| rows.into_iter().next()?.into_iter().next())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| s.conn.user.clone());
    if user.is_empty() {
        return Ok(Permissions::default());
    }
    let Some(privileges) = soft(s.records("SELECT grantee_type, grantee_id, privilege, object_type, object_id FROM sys.privileges").await)? else {
        return Ok(Permissions::default());
    };
    let Some(members) = soft(s.records("SELECT role_name, member_name, member_type FROM sys.membership").await)? else {
        return Ok(Permissions::default());
    };
    Ok(enterprise(&user, &privileges, &members, database))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(v: Value) -> Vec<Row> {
        v.as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
    }

    #[test]
    fn admin_role_through_a_role() {
        let members = rows(json!([
            {"role_name": "ops", "member_name": "ana", "member_type": "USER"},
            {"role_name": "ADMIN", "member_name": "ops", "member_type": "ROLE"},
        ]));
        assert_eq!(enterprise("ana", &[], &members, Some("ventas")), Permissions { manage_security: Access::Allowed, ..community() });
    }

    #[test]
    fn system_privileges_of_the_user_its_roles_and_public() {
        let members = rows(json!([{"role_name": "analistas", "member_name": "ana", "member_type": "USER"}]));
        let privileges = rows(json!([
            {"grantee_type": "role", "grantee_id": "analistas", "privilege": "VIEW_JOB_HISTORY", "object_type": "SYSTEM", "object_id": ""},
            {"grantee_type": "role", "grantee_id": "PUBLIC", "privilege": "CREATE_SPACE", "object_type": "SYSTEM", "object_id": ""},
            {"grantee_type": "user", "grantee_id": "ana", "privilege": "OWNERSHIP", "object_type": "SPACE", "object_id": "ventas"},
            {"grantee_type": "user", "grantee_id": "otro", "privilege": "CREATE_USER", "object_type": "SYSTEM", "object_id": ""},
        ]));
        let p = enterprise("ana", &privileges, &members, Some("ventas"));
        assert_eq!(p.profiler, Access::Allowed);
        assert_eq!(p.create_database, Access::Allowed);
        assert_eq!(p.drop_database, Access::Allowed);
        assert!(p.manage_security.is_denied());
        assert_eq!(p.restore, Access::Unknown);

        let p = enterprise("luis", &privileges, &members, Some("ventas"));
        assert_eq!(p.profiler, Access::Denied { missing: "VIEW JOB HISTORY".into() });
        assert_eq!(p.create_database, Access::Allowed, "through PUBLIC");
        assert_eq!(p.drop_database, Access::Unknown);
    }

    #[test]
    fn community_is_all_admins() {
        let p = community();
        assert_eq!((p.create_database, p.drop_database, p.profiler), (Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!(p.manage_security, Access::Unknown);
    }
}

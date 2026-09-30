//! What the user may do (`Session::permissions`), from its own row of
//! `SHOW USERS` (super, sysinfo, createdb). Only users with SYSINFO 1 may
//! run it; a user the server refuses it to has SYSINFO 0, and so isn't a
//! superuser (`root` and the superusers have it).
//!
//! - profiler: `performance_schema.perf_queries`. A superuser or a SYSINFO
//!   user sees every client's queries; without SYSINFO the view still
//!   answers, possibly with the user's own only: unknown.
//! - create, drop and security: allowed to a superuser. For anyone else
//!   unknown, never denied: TDengine 3.3.6 (OSS) let a user without SUPER
//!   and with CREATEDB 0 create and drop databases, create users and
//!   grant, so neither flag proves a refusal.
//!
//! TDengine has no backups or ending sessions over REST. A check the server
//! refuses otherwise leaves everything unknown; only a broken connection is
//! an error.

use crate::{text, TdSession};
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::{Map, Value};

/// The user as `SHOW USERS` describes it; `None` fields weren't answered.
#[derive(Debug, Clone, Default)]
pub(crate) struct Me {
    pub superuser: Option<bool>,
    pub sysinfo: Option<bool>,
}

fn flag(r: &Map<String, Value>, k: &str) -> Option<bool> {
    match r.get(k)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|v| v != 0),
        Value::String(s) => Some(s == "1" || s.eq_ignore_ascii_case("true")),
        _ => None,
    }
}

pub(crate) fn decide(me: &Me, database: Option<&str>) -> Permissions {
    let database = database.is_some_and(|d| !d.trim().is_empty());
    let Some(superuser) = me.superuser else { return Permissions::default() };
    if superuser {
        return Permissions {
            profiler: Access::Allowed,
            create_database: Access::Allowed,
            drop_database: if database { Access::Allowed } else { Access::Unknown },
            manage_security: Access::Allowed,
            ..Default::default()
        };
    }
    Permissions { profiler: if me.sysinfo == Some(true) { Access::Allowed } else { Access::Unknown }, ..Default::default() }
}

/// The refusal a SYSINFO 0 user gets.
fn permission_denied(e: &Error) -> bool {
    e.to_string().to_lowercase().contains("permission denied")
}

pub(crate) async fn check(s: &TdSession, database: Option<&str>) -> Result<Permissions> {
    let me = match s.records("SHOW USERS").await {
        Ok(rows) => {
            let user = &s.conn.user;
            let row = rows.iter().find(|r| r.get("name").map(text).as_deref() == Some(user.as_str()));
            match row {
                Some(r) => Me { superuser: flag(r, "super"), sysinfo: flag(r, "sysinfo") },
                None => Me::default(),
            }
        }
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(e) if permission_denied(&e) => Me { superuser: Some(false), sysinfo: Some(false) },
        Err(e) => {
            tracing::debug!("tdengine: permissions check refused: {e}");
            Me::default()
        }
    };
    Ok(decide(&me, database))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_may_do_everything() {
        let me = Me { superuser: Some(true), sysinfo: Some(true) };
        let p = decide(&me, Some("db"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(decide(&me, None).drop_database, Access::Unknown);
    }

    #[test]
    fn plain_users_are_never_denied() {
        let p = decide(&Me { superuser: Some(false), sysinfo: Some(true) }, Some("db"));
        assert_eq!(p, Permissions { profiler: Access::Allowed, ..Default::default() });
        // SYSINFO 0: SHOW USERS refused.
        assert_eq!(decide(&Me { superuser: Some(false), sysinfo: Some(false) }, Some("db")), Permissions::default());
    }

    #[test]
    fn nothing_known() {
        assert_eq!(decide(&Me::default(), Some("db")), Permissions::default());
        assert!(permission_denied(&Error::Query("Permission denied or target object not exist".into())));
    }

    #[test]
    fn flags_as_the_server_sends_them() {
        let r: Map<String, Value> = serde_json::from_value(serde_json::json!({ "super": 1, "sysinfo": "0", "createdb": true })).unwrap();
        assert_eq!((flag(&r, "super"), flag(&r, "sysinfo"), flag(&r, "createdb"), flag(&r, "x")), (Some(true), Some(false), Some(true), None));
    }
}

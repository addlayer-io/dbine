//! What the login may do (`Session::permissions`), from `GET /_session`
//! (the user's name and roles) and the database's `_security`.
//!
//! - create / drop a database: only server admins (the `_admin` role; on
//!   a server without admins, CouchDB 2's "admin party", everyone has it).
//! - security: server admins; a database admin (by name or role in
//!   `_security.admins`) may change that database's `_security`. Anyone
//!   else is unknown, not denied: `_users` may let users sign up.
//!
//! CouchDB has no backups, profiler nor sessions to end. DBine's read-only
//! mode isn't looked at: this reports what the server grants the user, and
//! the read-only mode blocks the writes on its own. A check the server
//! refuses leaves its action unknown; only an unreachable server is an
//! error.

use crate::{seg, CouchSession};
use dbine_driver::{Access, Error, Permissions, Result};
use reqwest::Method;
use serde_json::Value;

/// The logged-in user.
#[derive(Debug, Clone, Default)]
pub(crate) struct User {
    pub name: Option<String>,
    pub roles: Vec<String>,
}

impl User {
    fn server_admin(&self) -> bool {
        self.roles.iter().any(|r| r == "_admin")
    }
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

pub(crate) fn user(session: &Value) -> Option<User> {
    let ctx = session.get("userCtx")?;
    Some(User { name: ctx.get("name").and_then(Value::as_str).map(str::to_string), roles: strs(ctx.get("roles")) })
}

/// The user is in `_security.admins`, by name or role.
pub(crate) fn database_admin(security: &Value, u: &User) -> bool {
    let admins = security.get("admins");
    let names = strs(admins.and_then(|a| a.get("names")));
    let roles = strs(admins.and_then(|a| a.get("roles")));
    u.name.as_ref().is_some_and(|n| names.contains(n)) || u.roles.iter().any(|r| roles.contains(r))
}

/// `user`: `None` when `_session` didn't answer; `db_admin`: whether the
/// user administers the explorer's database (`None`: not known).
pub(crate) fn decide(user: Option<&User>, db_admin: Option<bool>, database: bool) -> Permissions {
    let mut p = Permissions::default();
    if let Some(u) = user {
        let admin = Access::check(u.server_admin(), "_admin (administrador del servidor)");
        p.create_database = admin.clone();
        p.drop_database = if database { admin } else { Access::Unknown };
        p.manage_security = if u.server_admin() || db_admin == Some(true) { Access::Allowed } else { Access::Unknown };
    }
    p
}

/// `None` when the server refused; an unreachable server is the error.
async fn get(s: &CouchSession, path: &str) -> Result<Option<Value>> {
    match s.call(Method::GET, path, None).await {
        Ok(v) => Ok(Some(v)),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            tracing::debug!("couchdb: permissions check refused: {path}: {e}");
            Ok(None)
        }
    }
}

pub(crate) async fn check(s: &CouchSession, database: Option<&str>) -> Result<Permissions> {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    let u = get(s, "/_session").await?.as_ref().and_then(user);
    let db_admin = match (&u, database) {
        (Some(u), Some(db)) if !u.server_admin() => {
            get(s, &format!("/{}/_security", seg(db))).await?.map(|sec| database_admin(&sec, u))
        }
        _ => None,
    };
    Ok(decide(u.as_ref(), db_admin, database.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    #[test]
    fn a_server_admin_may_do_everything_offered() {
        let u = user(&json!({ "ok": true, "userCtx": { "name": "admin", "roles": ["_admin"] } })).unwrap();
        let p = decide(Some(&u), None, true);
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.profiler, &p.kill_session), (&Access::Unknown, &Access::Unknown, &Access::Unknown));
        assert_eq!(decide(Some(&u), None, false).drop_database, Access::Unknown);
    }

    #[test]
    fn a_plain_user_and_a_database_admin() {
        let u = user(&json!({ "userCtx": { "name": "ana", "roles": ["ventas"] } })).unwrap();
        let p = decide(Some(&u), Some(false), true);
        assert!(denied(&p.create_database, "_admin") && denied(&p.drop_database, "_admin"));
        // It may still sign users up: not proven.
        assert_eq!(p.manage_security, Access::Unknown);
        assert!(database_admin(&json!({ "admins": { "names": [], "roles": ["ventas"] } }), &u));
        assert!(database_admin(&json!({ "admins": { "names": ["ana"] } }), &u));
        assert!(!database_admin(&json!({ "members": { "names": ["ana"] } }), &u));
        assert!(!database_admin(&json!({}), &u));
        assert_eq!(decide(Some(&u), Some(true), true).manage_security, Access::Allowed);
    }

    #[test]
    fn unknown_session_and_anonymous() {
        assert_eq!(decide(None, None, true), Permissions::default());
        let anon = user(&json!({ "userCtx": { "name": null, "roles": [] } })).unwrap();
        assert!(anon.name.is_none() && decide(Some(&anon), None, true).create_database.is_denied());
    }
}

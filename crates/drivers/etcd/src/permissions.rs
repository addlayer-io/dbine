//! What the login may do (`Session::permissions`). etcd enforces users and
//! roles only after `auth enable`; then the `root` role is its
//! administrator: the snapshot (the Maintenance API's `Snapshot` is
//! admin-only) and every call of the Auth API that changes users, roles or
//! grants need it. A user may read its own roles (`UserGet` on itself).
//! Both calls only read.

use crate::EtcdSession;
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::{json, Value};

const ROOT: &str = "root";

/// From the auth status and, with auth on, the login's roles (`None`: they
/// couldn't be read).
pub(crate) fn decide(auth_enabled: bool, roles: Option<&[String]>) -> Permissions {
    let admin = if !auth_enabled {
        Access::Allowed
    } else {
        match roles {
            Some(r) => Access::check(r.iter().any(|r| r == ROOT), "rol root"),
            None => Access::Unknown,
        }
    };
    Permissions { backup: admin.clone(), manage_security: admin, ..Default::default() }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// Only a dead connection is an error; a check that fails leaves `Unknown`.
pub(crate) async fn check(s: &EtcdSession) -> Result<Permissions> {
    // `AuthStatus` is itself admin-only once auth is on: "permission
    // denied" means auth is on and the login isn't root.
    let enabled = match s.call("/v3/auth/status", json!({})).await {
        Ok(v) => v.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(e) if e.to_string().contains("permission denied") => true,
        Err(_) => return Ok(Permissions::default()),
    };
    if !enabled {
        return Ok(decide(false, None));
    }
    // With auth on and no user (a client certificate), the name isn't known here.
    let roles = match s.conn.user.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => s.call("/v3/auth/user/get", json!({ "name": u })).await.ok().map(|r| strings(r.get("roles"))),
        None => None,
    };
    Ok(decide(true, roles.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_decides_with_auth_on() {
        assert_eq!(decide(false, None).backup, Access::Allowed);
        assert_eq!(decide(false, None).manage_security, Access::Allowed);
        let p = decide(true, Some(&["lectores".into()]));
        assert_eq!(p.backup, Access::Denied { missing: "rol root".into() });
        assert_eq!(p.manage_security, Access::Denied { missing: "rol root".into() });
        assert_eq!(decide(true, Some(&["a".into(), "root".into()])).backup, Access::Allowed);
        assert_eq!(decide(true, None).backup, Access::Unknown);
        // Nothing else is offered: restore, profiler, kill, databases.
        let p = decide(false, None);
        assert_eq!((p.restore, p.profiler, p.kill_session, p.create_database, p.drop_database), Default::default());
    }
}

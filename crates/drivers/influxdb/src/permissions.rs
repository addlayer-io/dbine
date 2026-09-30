//! What the login or token may do (`Session::permissions`), per API.
//!
//! - **1.x (InfluxQL):** users are admins or not, and only admins run
//!   `SHOW QUERIES` (the profiler), `CREATE` / `DROP DATABASE` and manage
//!   users. `SHOW USERS` is admin-only too: when it answers, the user is an
//!   admin (or authentication is off); when the server refuses it for
//!   lacking admin, the user isn't.
//! - **2.x (Flux):** tokens, and the server doesn't say which one is ours
//!   (`/api/v2/authorizations` hides the token strings). A token that may
//!   read authorizations sees its own among them, so when every active one
//!   that may read authorizations agrees about `write:buckets` (on the whole
//!   org, or on the bucket for dropping it), that's the answer; otherwise,
//!   and when the list comes back empty (the token can't read them),
//!   unknown. There's no profiler or security in 2.x.
//! - **3.x (SQL):** admin tokens may do everything; Enterprise's resource
//!   tokens only read and write data. `system.tokens` of `_internal` is
//!   admin-only: when it answers, the token is an admin (or authentication
//!   is off); refused as unauthorized (403, not a 401 "not
//!   authenticated"), it's a resource token, which can't create or drop
//!   databases. The profiler (`system.queries`) may still be readable by
//!   one: unknown then.
//!
//! No API has backups or ending sessions. Only a broken connection is an
//! error.

use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::Value as J;

const V1_ADMIN: &str = "ALL PRIVILEGES (usuario administrador)";
const V3_ADMIN: &str = "un token de administrador";

/// A check's outcome: the answer, or the refusal's error.
fn classify<T>(r: Result<T>) -> Result<std::result::Result<T, Error>> {
    match r {
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            tracing::debug!("influxdb: permissions check refused: {e}");
            Ok(Err(e))
        }
        Ok(v) => Ok(Ok(v)),
    }
}

// -- 1.x ----------------------------------------------------------------------

/// `admin`: `Some(true)` SHOW USERS answered; `Some(false)` refused for
/// lacking admin.
pub(crate) fn v1_decide(admin: Option<bool>, database: Option<&str>) -> Permissions {
    let Some(admin) = admin else { return Permissions::default() };
    let a = Access::check(admin, V1_ADMIN);
    Permissions {
        profiler: a.clone(),
        create_database: a.clone(),
        drop_database: if database.is_some_and(|d| !d.trim().is_empty()) { a.clone() } else { Access::Unknown },
        manage_security: a,
        ..Default::default()
    }
}

/// The server's refusal says the user isn't an admin.
fn needs_admin(e: &Error) -> bool {
    let m = e.to_string().to_lowercase();
    m.contains("requires admin")
}

pub(crate) async fn v1(s: &mut crate::v1::InfluxQlSession, database: Option<&str>) -> Result<Permissions> {
    let admin = match classify(s.query("SHOW USERS").await)? {
        Ok(_) => Some(true),
        Err(e) if needs_admin(&e) => Some(false),
        Err(_) => None,
    };
    Ok(v1_decide(admin, database))
}

// -- 2.x ----------------------------------------------------------------------

/// `write:buckets` over the org (`bucket` None: creating one) or the
/// bucket, from one permission. `org`: the connection's org, by name or id.
fn writes_buckets(p: &J, org: &str, bucket: Option<&str>) -> bool {
    let r = &p["resource"];
    let s = |k: &str| r.get(k).and_then(J::as_str).filter(|v| !v.is_empty());
    let in_org = match (s("orgID"), s("org")) {
        (None, None) => true,
        (id, name) => id == Some(org) || name == Some(org),
    };
    p["action"] == "write"
        && r["type"] == "buckets"
        && in_org
        && match (s("id"), bucket) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(_), Some(b)) => s("name") == Some(b),
        }
}

/// Every candidate agrees: that's the answer.
fn agree(v: &[bool]) -> Option<bool> {
    match v.first() {
        Some(first) if v.iter().all(|x| x == first) => Some(*first),
        _ => None,
    }
}

/// `authorizations`: the `authorizations` array of `/api/v2/authorizations`.
pub(crate) fn v2_decide(authorizations: &[J], org: &str, bucket: Option<&str>) -> Permissions {
    let perms = |a: &J| a["permissions"].as_array().cloned().unwrap_or_default();
    // Ours is among those that can read authorizations.
    let candidates: Vec<Vec<J>> = authorizations
        .iter()
        .filter(|a| a["status"].as_str().is_none_or(|s| s == "active"))
        .map(perms)
        .filter(|ps| ps.iter().any(|p| p["action"] == "read" && p["resource"]["type"] == "authorizations"))
        .collect();
    let decide = |bucket: Option<&str>| {
        let each: Vec<bool> = candidates.iter().map(|ps| ps.iter().any(|p| writes_buckets(p, org, bucket))).collect();
        agree(&each).map_or(Access::Unknown, |ok| Access::check(ok, "write:buckets"))
    };
    let bucket = bucket.map(str::trim).filter(|b| !b.is_empty());
    Permissions {
        create_database: decide(None),
        drop_database: match bucket {
            Some(b) => decide(Some(b)),
            None => Access::Unknown,
        },
        ..Default::default()
    }
}

/// `list`: `/api/v2/authorizations`' answer.
pub(crate) fn v2(list: Result<J>, org: &str, bucket: Option<&str>) -> Result<Permissions> {
    let Ok(v) = classify(list)? else { return Ok(Permissions::default()) };
    let auths = v["authorizations"].as_array().cloned().unwrap_or_default();
    Ok(v2_decide(&auths, org, bucket))
}

// -- 3.x ----------------------------------------------------------------------

/// `admin`: `Some(true)` system.tokens answered, `Some(false)` refused as
/// unauthorized.
pub(crate) fn v3_decide(admin: Option<bool>, database: Option<&str>) -> Permissions {
    match admin {
        None => Permissions::default(),
        Some(true) => Permissions {
            profiler: Access::Allowed,
            create_database: Access::Allowed,
            drop_database: if database.is_some_and(|d| !d.trim().is_empty()) { Access::Allowed } else { Access::Unknown },
            ..Default::default()
        },
        Some(false) => Permissions {
            create_database: Access::check(false, V3_ADMIN),
            drop_database: if database.is_some_and(|d| !d.trim().is_empty()) { Access::check(false, V3_ADMIN) } else { Access::Unknown },
            ..Default::default()
        },
    }
}

/// `tokens`: the answer to `SELECT … FROM system.tokens` on `_internal`.
pub(crate) fn v3<T>(tokens: Result<T>, database: Option<&str>) -> Result<Permissions> {
    let admin = match classify(tokens)? {
        Ok(_) => Some(true),
        // 401 is a token the server doesn't know (not ours to judge).
        Err(Error::AuthFailed(m)) if !m.to_lowercase().contains("not authenticated") => Some(false),
        Err(_) => None,
    };
    Ok(v3_decide(admin, database))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    #[test]
    fn v1_admins_and_the_rest() {
        let p = v1_decide(Some(true), Some("telegraf"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        let p = v1_decide(Some(false), None);
        assert!(denied(&p.profiler, "ALL PRIVILEGES") && denied(&p.create_database, "administrador") && denied(&p.manage_security, "ALL PRIVILEGES"));
        assert_eq!(p.drop_database, Access::Unknown);
        assert_eq!(v1_decide(None, Some("x")), Permissions::default());
        assert!(needs_admin(&Error::AuthFailed(
            "error authorizing query: ana not authorized to execute statement 'SHOW USERS', requires admin privilege".into()
        )));
        assert!(!needs_admin(&Error::Query("timeout".into())));
    }

    fn auth(read_auths: bool, perms: Vec<J>) -> J {
        let mut ps = perms;
        if read_auths {
            ps.push(json!({ "action": "read", "resource": { "type": "authorizations" } }));
        }
        json!({ "status": "active", "permissions": ps })
    }

    #[test]
    fn v2_operator_and_org_tokens() {
        let all = auth(true, vec![json!({ "action": "write", "resource": { "type": "buckets" } })]);
        let p = v2_decide(&[all], "dbine", Some("test"));
        assert_eq!((&p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed));
        let org = auth(true, vec![json!({ "action": "write", "resource": { "type": "buckets", "orgID": "0a1", "org": "dbine" } })]);
        assert_eq!(v2_decide(&[org.clone()], "dbine", None).create_database, Access::Allowed);
        assert_eq!(v2_decide(&[org.clone()], "0a1", None).create_database, Access::Allowed);
        assert!(denied(&v2_decide(&[org], "other", None).create_database, "write:buckets"));
    }

    #[test]
    fn v2_bucket_tokens_and_disagreement() {
        let one = auth(true, vec![json!({ "action": "write", "resource": { "type": "buckets", "id": "b1", "name": "test", "orgID": "0a1" } })]);
        let p = v2_decide(&[one.clone()], "0a1", Some("test"));
        assert!(denied(&p.create_database, "write:buckets"));
        assert_eq!(p.drop_database, Access::Allowed);
        assert!(denied(&v2_decide(&[one.clone()], "0a1", Some("other")).drop_database, "write:buckets"));
        // Two candidates that disagree: can't tell which is ours.
        let all = auth(true, vec![json!({ "action": "write", "resource": { "type": "buckets" } })]);
        assert_eq!(v2_decide(&[one.clone(), all], "0a1", None).create_database, Access::Unknown);
        // Tokens that can't read authorizations aren't ours.
        let reader = auth(false, vec![]);
        assert_eq!(v2_decide(&[one, reader], "0a1", Some("test")).drop_database, Access::Allowed);
        // Nothing visible: unknown.
        assert_eq!(v2_decide(&[], "0a1", Some("test")), Permissions::default());
        assert_eq!(v2(Ok(json!({ "authorizations": [] })), "0a1", None).unwrap(), Permissions::default());
        assert_eq!(v2(Err(Error::AuthFailed("no".into())), "0a1", None).unwrap(), Permissions::default());
        assert!(v2(Err(Error::Connect("down".into())), "0a1", None).is_err());
    }

    #[test]
    fn v3_admin_and_resource_tokens() {
        let p = v3(Ok(()), Some("db")).unwrap();
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        let p = v3::<()>(Err(Error::AuthFailed("unauthorized".into())), Some("db")).unwrap();
        assert!(denied(&p.create_database, "token de administrador") && denied(&p.drop_database, "token de administrador"));
        assert_eq!(p.profiler, Access::Unknown);
        assert_eq!(v3::<()>(Err(Error::Query("table not found".into())), None).unwrap(), Permissions::default());
        let p = v3::<()>(Err(Error::AuthFailed("the request was not authenticated".into())), None).unwrap();
        assert_eq!(p, Permissions::default());
    }
}

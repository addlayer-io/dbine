//! What the login may do (`Session::permissions`), from the cluster
//! manager's `POST /pools/default/checkPermissions`, which answers for the
//! current user through all its roles and groups:
//!
//! - profiler: `cluster.n1ql.meta!read` (reading `system:completed_requests`;
//!   lowering the threshold is optional).
//! - create a bucket: `cluster.buckets!create`; drop it:
//!   `cluster.bucket[<bucket>]!delete`.
//! - create a scope ("Nuevo esquema…") in the bucket:
//!   `cluster.bucket[<bucket>].collections!write`.
//! - security: `cluster.admin.security!write` or
//!   `cluster.admin.security.local!write` (local users and their roles).
//!
//! Couchbase has no backups DBine runs and no sessions to end. DBine's
//! read-only mode isn't looked at: this reports what the server grants the
//! user, and the read-only mode blocks the writes on its own. A check the
//! server refuses leaves everything unknown; only an unreachable server is
//! an error.

use crate::CbSession;
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::Value;

const META: &str = "cluster.n1ql.meta!read";
const CREATE: &str = "cluster.buckets!create";
const SECURITY: &str = "cluster.admin.security!write";
const SECURITY_LOCAL: &str = "cluster.admin.security.local!write";

fn drop_permission(bucket: &str) -> String {
    format!("cluster.bucket[{bucket}]!delete")
}

fn scope_permission(bucket: &str) -> String {
    format!("cluster.bucket[{bucket}].collections!write")
}

/// The permissions asked for, comma-separated.
pub(crate) fn request(database: Option<&str>) -> String {
    let mut v = vec![META.to_string(), CREATE.into(), SECURITY.into(), SECURITY_LOCAL.into()];
    v.extend(database.map(drop_permission));
    v.extend(database.map(scope_permission));
    v.join(",")
}

/// `answer`: `checkPermissions`' `{permission: bool}` (`None`: refused).
pub(crate) fn decide(answer: Option<&Value>, database: Option<&str>) -> Permissions {
    let one = |k: &str| answer.and_then(|a| a.get(k)).and_then(Value::as_bool);
    let access = |v: Option<bool>, missing: &str| v.map_or(Access::Unknown, |ok| Access::check(ok, missing));
    let security = match (one(SECURITY), one(SECURITY_LOCAL)) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    };
    Permissions {
        profiler: access(one(META), "query_system_catalog (cluster.n1ql.meta!read)"),
        create_database: access(one(CREATE), "cluster.buckets!create (cluster_admin)"),
        drop_database: match database {
            Some(b) => {
                let k = drop_permission(b);
                access(one(&k), &format!("{k} (cluster_admin)"))
            }
            None => Access::Unknown,
        },
        create_schema: match database {
            Some(b) => {
                let k = scope_permission(b);
                access(one(&k), &format!("{k} (bucket_admin)"))
            }
            None => Access::Unknown,
        },
        manage_security: access(security, "cluster.admin.security.local!write (security_admin_local)"),
        ..Default::default()
    }
}

pub(crate) async fn check(s: &CbSession, database: Option<&str>) -> Result<Permissions> {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    let rb = s.conn.http.post(format!("{}/pools/default/checkPermissions", s.conn.mgmt)).body(request(database));
    let answer = match s.conn.mgmt_send(rb).await {
        Ok(text) => serde_json::from_str::<Value>(&text).ok(),
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(e) => {
            tracing::debug!("couchbase: permissions check refused: {e}");
            None
        }
    };
    Ok(decide(answer.as_ref(), database))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn answer(v: bool) -> Value {
        json!({ META: v, CREATE: v, SECURITY: v, SECURITY_LOCAL: v, "cluster.bucket[ventas]!delete": v, "cluster.bucket[ventas].collections!write": v })
    }

    #[test]
    fn asks_for_the_bucket_it_would_drop() {
        assert_eq!(request(None), "cluster.n1ql.meta!read,cluster.buckets!create,cluster.admin.security!write,cluster.admin.security.local!write");
        assert!(request(Some("ventas")).ends_with(",cluster.bucket[ventas]!delete,cluster.bucket[ventas].collections!write"));
    }

    #[test]
    fn an_admin_may_do_everything_offered() {
        let p = decide(Some(&answer(true)), Some("ventas"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.kill_session), (&Access::Unknown, &Access::Unknown));
        assert_eq!(p.create_schema, Access::Allowed);
        assert_eq!(decide(Some(&answer(true)), None).drop_database, Access::Unknown);
        assert_eq!(decide(Some(&answer(true)), None).create_schema, Access::Unknown);
    }

    #[test]
    fn a_bucket_user_is_denied_by_permission() {
        let p = decide(Some(&answer(false)), Some("ventas"));
        assert!(denied(&p.profiler, "cluster.n1ql.meta!read"));
        assert!(denied(&p.create_database, "cluster.buckets!create"));
        assert!(denied(&p.drop_database, "cluster.bucket[ventas]!delete"));
        assert!(denied(&p.create_schema, "cluster.bucket[ventas].collections!write"));
        assert!(denied(&p.manage_security, "security.local!write"));
        // Only local users: still allowed.
        let a = json!({ SECURITY: false, SECURITY_LOCAL: true });
        assert_eq!(decide(Some(&a), None).manage_security, Access::Allowed);
    }

    #[test]
    fn refused_is_unknown() {
        assert_eq!(decide(None, Some("ventas")), Permissions::default());
    }
}

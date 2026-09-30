//! What the user may do (`Session::permissions`), from `LIST PRIVILEGES OF
//! USER <me>`, which any user may run about itself and which includes what
//! its roles give (`root` lists nothing: it has every privilege). The
//! privileges these actions need are global ones:
//!
//! - profiler: MAINTAIN (`SHOW QUERIES`).
//! - create / drop: MANAGE_DATABASE.
//! - security: MANAGE_USER. With MANAGE_ROLE only, or some privilege it may
//!   pass on (grant option), part of it still works: unknown.
//!
//! IoTDB has no backups or ending sessions over REST. A check the server
//! refuses leaves everything unknown; only a broken connection is an
//! error.

use crate::IotDbSession;
use dbine_driver::{Access, Error, Grant, Permissions, Result};

pub(crate) fn decide(grants: &[Grant], database: Option<&str>) -> Permissions {
    let has = |p: &str| grants.iter().any(|g| g.privilege.eq_ignore_ascii_case(p) || g.privilege.eq_ignore_ascii_case("ALL"));
    let databases = Access::check(has("MANAGE_DATABASE"), "MANAGE_DATABASE");
    let security = if has("MANAGE_USER") {
        Access::Allowed
    } else if has("MANAGE_ROLE") || grants.iter().any(|g| g.grantable) {
        Access::Unknown
    } else {
        Access::check(false, "MANAGE_USER")
    };
    Permissions {
        profiler: Access::check(has("MAINTAIN"), "MAINTAIN"),
        create_database: databases.clone(),
        drop_database: if database.is_some_and(|d| !d.trim().is_empty()) { databases } else { Access::Unknown },
        manage_security: security,
        ..Default::default()
    }
}

pub(crate) async fn check(s: &IotDbSession, database: Option<&str>) -> Result<Permissions> {
    match crate::security::grants(s, &s.username).await {
        Ok(grants) => Ok(decide(&grants, database)),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            tracing::debug!("iotdb: permissions check refused: {e}");
            Ok(Permissions::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn g(privilege: &str, grantable: bool) -> Grant {
        Grant { privilege: privilege.into(), grantable, ..Default::default() }
    }

    #[test]
    fn root_may_do_everything() {
        let p = decide(&[g("ALL", false)], Some("root.sg"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.kill_session), (&Access::Unknown, &Access::Unknown));
        assert_eq!(decide(&[g("ALL", false)], None).drop_database, Access::Unknown);
    }

    #[test]
    fn a_reader_is_denied() {
        let p = decide(&[g("READ_DATA", false)], Some("root.sg"));
        assert!(denied(&p.profiler, "MAINTAIN"));
        assert!(denied(&p.create_database, "MANAGE_DATABASE") && denied(&p.drop_database, "MANAGE_DATABASE"));
        assert!(denied(&p.manage_security, "MANAGE_USER"));
    }

    #[test]
    fn partial_security_is_unknown() {
        let p = decide(&[g("MANAGE_ROLE", false)], None);
        assert_eq!(p.manage_security, Access::Unknown);
        let p = decide(&[g("READ_DATA", true)], None);
        assert_eq!(p.manage_security, Access::Unknown);
        let p = decide(&[g("MANAGE_DATABASE", false), g("MAINTAIN", false), g("MANAGE_USER", false)], Some("root.sg"));
        assert_eq!((&p.profiler, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    }
}

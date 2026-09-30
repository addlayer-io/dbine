//! What the login may do (`Session::permissions`) in Aurora DSQL. Of the
//! actions that need more than reading data, DSQL only has users and roles
//! (no native backups, profiler, ending sessions nor databases of its own:
//! the only one is `postgres`). Managing them takes the `admin` role or a
//! role with `CREATEROLE`, read from `pg_roles`.

use crate::err;
use dbine_driver::{Access, Error, Permissions, Result};
use tokio_postgres::{Client, SimpleQueryMessage};

pub const SQL: &str = "SELECT current_user AS me, rolsuper, rolcreaterole FROM pg_roles WHERE rolname = current_user";

/// The login's name and role attributes as `pg_roles` gives them.
pub fn map(me: &str, superuser: bool, create_role: bool) -> Permissions {
    Permissions {
        // `admin` signs in with DbConnectAdmin and owns the cluster.
        manage_security: Access::check(me == "admin" || superuser || create_role, "CREATEROLE"),
        ..Default::default()
    }
}

pub async fn check(c: &Client) -> Result<Permissions> {
    let msgs = match c.simple_query(SQL).await.map_err(err) {
        Ok(m) => m,
        // A dead connection is the session's error; anything else only
        // leaves the actions unknown.
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(e) => {
            tracing::debug!("dsql permissions: {e}");
            return Ok(Permissions::default());
        }
    };
    let yes = |v: Option<&str>| matches!(v, Some("t" | "true"));
    Ok(msgs
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(map(
                r.get("me").unwrap_or_default(),
                yes(r.get("rolsuper")),
                yes(r.get("rolcreaterole")),
            )),
            _ => None,
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_or_createrole_manage_roles() {
        assert_eq!(map("admin", false, false).manage_security, Access::Allowed);
        assert_eq!(map("app", false, true).manage_security, Access::Allowed);
        assert_eq!(map("app", true, false).manage_security, Access::Allowed);
        let p = map("lector", false, false);
        assert_eq!(p.manage_security, Access::Denied { missing: "CREATEROLE".into() });
        // Nothing else is DSQL's to check.
        assert_eq!(Permissions { manage_security: Access::Unknown, ..p }, Permissions::default());
    }
}

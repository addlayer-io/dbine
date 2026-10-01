//! What the login may do (`Session::permissions`) in Aurora DSQL. Of the
//! actions that need more than reading data, DSQL only has users and roles
//! (no native backups, profiler, ending sessions nor databases of its own:
//! the only one is `postgres`). Managing them takes the `admin` role or a
//! role with `CREATEROLE`, read from `pg_roles`; creating a schema, the
//! `admin` role or `CREATE` on the database (`has_database_privilege`).

use crate::err;
use dbine_driver::{Access, Error, Permissions, Result};
use tokio_postgres::{Client, SimpleQueryMessage};

pub const SQL: &str = "SELECT current_user AS me, rolsuper, rolcreaterole FROM pg_roles WHERE rolname = current_user";
/// Apart: a cluster that refuses it mustn't cost the check above.
pub const CREATE_SQL: &str = "SELECT has_database_privilege(current_database(), 'CREATE') AS can_create";

/// The login's name and role attributes as `pg_roles` gives them, and
/// whether it may create schemas (`None`: unknown).
pub fn map(me: &str, superuser: bool, create_role: bool, can_create: Option<bool>) -> Permissions {
    // `admin` signs in with DbConnectAdmin and owns the cluster.
    let admin = me == "admin" || superuser;
    Permissions {
        manage_security: Access::check(admin || create_role, "CREATEROLE"),
        create_schema: match can_create {
            _ if admin => Access::Allowed,
            Some(yes) => Access::check(yes, "CREATE sobre la base postgres"),
            None => Access::Unknown,
        },
        ..Default::default()
    }
}

/// The first row's `col` as a boolean, `None` when refused or missing.
async fn flag(c: &Client, sql: &str, col: &str) -> Result<Option<bool>> {
    match c.simple_query(sql).await.map_err(err) {
        Ok(msgs) => Ok(msgs.iter().find_map(|m| match m {
            SimpleQueryMessage::Row(r) => r.get(col).map(|v| matches!(v, "t" | "true")),
            _ => None,
        })),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            tracing::debug!("dsql permissions ({col}): {e}");
            Ok(None)
        }
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
    let can_create = flag(c, CREATE_SQL, "can_create").await?;
    Ok(msgs
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(map(
                r.get("me").unwrap_or_default(),
                yes(r.get("rolsuper")),
                yes(r.get("rolcreaterole")),
                can_create,
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
        assert_eq!(map("admin", false, false, None).manage_security, Access::Allowed);
        assert_eq!(map("app", false, true, None).manage_security, Access::Allowed);
        assert_eq!(map("app", true, false, None).manage_security, Access::Allowed);
        let p = map("lector", false, false, None);
        assert_eq!(p.manage_security, Access::Denied { missing: "CREATEROLE".into() });
        // Nothing else is DSQL's to check.
        assert_eq!(Permissions { manage_security: Access::Unknown, ..p }, Permissions::default());
    }

    #[test]
    fn schemas_need_create_on_the_database() {
        assert_eq!(map("admin", false, false, Some(false)).create_schema, Access::Allowed);
        assert_eq!(map("app", false, false, Some(true)).create_schema, Access::Allowed);
        assert_eq!(map("app", false, false, Some(false)).create_schema, Access::Denied { missing: "CREATE sobre la base postgres".into() });
        assert_eq!(map("app", false, true, None).create_schema, Access::Unknown);
    }
}

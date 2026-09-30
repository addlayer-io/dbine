//! What the login may do (`Session::permissions`), from one query on the
//! roles active in the session (`IS_ROLE_IN_SESSION`, which follows the
//! role hierarchy and the secondary roles) and the database's owner.
//!
//! Snowflake has no function that answers "may this role use account
//! privilege X": a custom role may hold CREATE DATABASE, MANAGE GRANTS or
//! MONITOR, and seeing it takes SHOW GRANTS down the whole hierarchy or
//! ACCOUNT_USAGE (hours late). So the system roles say what's allowed and
//! the rest stays unknown, never denied:
//!
//! - ACCOUNTADMIN: backups, restores, the profiler (every warehouse's
//!   history), ending transactions of other users, creating databases and
//!   managing users.
//! - SYSADMIN: creating databases (a restore always creates one).
//! - USERADMIN (and SECURITYADMIN above it): users, roles and grants.
//! - Dropping the explorer's database needs OWNERSHIP of it: allowed or
//!   denied by whether its owner role is in the session.
//!
//! A check the server refuses leaves everything unknown; only a broken
//! connection is an error.

use crate::SnowflakeSession;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Access, Error, Permissions, Result};

const ROLES: &str = "IFF(IS_ROLE_IN_SESSION('ACCOUNTADMIN'), 1, 0), IFF(IS_ROLE_IN_SESSION('SYSADMIN'), 1, 0), \
                     IFF(IS_ROLE_IN_SESSION('USERADMIN'), 1, 0)";

/// The query's answer.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Roles {
    pub accountadmin: bool,
    pub sysadmin: bool,
    pub useradmin: bool,
    /// The explorer's database's owner role is in the session (`None`: no
    /// database, or its owner couldn't be read).
    pub owner: Option<bool>,
}

pub(crate) fn map(r: Roles) -> Permissions {
    let allowed_if = |yes: bool| if yes { Access::Allowed } else { Access::Unknown };
    Permissions {
        backup: allowed_if(r.accountadmin),
        restore: allowed_if(r.accountadmin || r.sysadmin),
        profiler: allowed_if(r.accountadmin),
        kill_session: allowed_if(r.accountadmin),
        create_database: allowed_if(r.accountadmin || r.sysadmin),
        drop_database: r.owner.map_or(Access::Unknown, |own| Access::check(own, "OWNERSHIP sobre la base")),
        manage_security: allowed_if(r.accountadmin || r.useradmin),
    }
}

fn flag(row: &[Option<String>], i: usize) -> Option<bool> {
    row.get(i).cloned().flatten().map(|v| v.trim() == "1")
}

/// The first row of `sql`; `None` when the server refused it.
async fn first_row(s: &SnowflakeSession, sql: &str, args: &[&str]) -> Result<Option<Vec<Option<String>>>> {
    match s.text_rows(sql, args).await {
        Ok(rows) => Ok(Some(rows.into_iter().next().unwrap_or_default())),
        Err(Error::Query(e)) => {
            tracing::debug!("snowflake: permissions check refused: {e}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

pub(crate) async fn check(s: &SnowflakeSession, database: Option<&str>) -> Result<Permissions> {
    let db = database.map(str::trim).filter(|d| !d.is_empty());
    let mut row = None;
    if let Some(db) = db {
        // The owner of a database the role can't see is NULL: unknown.
        let sql = format!(
            "SELECT {ROLES}, (SELECT IFF(IS_ROLE_IN_SESSION(database_owner), 1, 0) FROM {}.INFORMATION_SCHEMA.DATABASES \
             WHERE database_name = ?)",
            qualified_name(Quote::Double, None, db)
        );
        row = first_row(s, &sql, &[db]).await?;
    }
    // Without a database, or when it can't be read: the roles alone.
    let row = match row {
        Some(r) => r,
        None => match first_row(s, &format!("SELECT {ROLES}"), &[]).await? {
            Some(r) => r,
            None => return Ok(Permissions::default()),
        },
    };
    Ok(map(Roles {
        accountadmin: flag(&row, 0) == Some(true),
        sysadmin: flag(&row, 1) == Some(true),
        useradmin: flag(&row, 2) == Some(true),
        owner: flag(&row, 3),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accountadmin_owning_the_database_may_do_everything() {
        let r = Roles { accountadmin: true, sysadmin: true, useradmin: true, owner: Some(true) };
        assert_eq!(map(r), Permissions::all());
    }

    #[test]
    fn other_roles_are_never_denied_account_privileges() {
        let p = map(Roles { owner: Some(false), ..Default::default() });
        assert_eq!(p.drop_database, Access::Denied { missing: "OWNERSHIP sobre la base".into() });
        for a in [p.backup, p.restore, p.profiler, p.kill_session, p.create_database, p.manage_security] {
            assert_eq!(a, Access::Unknown);
        }
    }

    #[test]
    fn sysadmin_and_useradmin() {
        let p = map(Roles { sysadmin: true, useradmin: true, ..Default::default() });
        assert_eq!((p.create_database, p.restore, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!((p.backup, p.kill_session, p.drop_database), (Access::Unknown, Access::Unknown, Access::Unknown));
    }

    #[test]
    fn flags_from_text() {
        let row = vec![Some("1".to_string()), Some("0".to_string()), None];
        assert_eq!((flag(&row, 0), flag(&row, 1), flag(&row, 2), flag(&row, 5)), (Some(true), Some(false), None, None));
    }
}

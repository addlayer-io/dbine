//! What the login may do (`Session::permissions`), from three read-only
//! queries (each may fail on its own; what it answers then stays unknown):
//!
//! - who: `CURRENT_USER`, the database's owner (`MON$DATABASE.MON$OWNER`)
//!   and `RDB$ROLE_IN_USE('RDB$ADMIN')` (Firebird 3+). SYSDBA or the
//!   RDB$ADMIN role make an administrator.
//! - the security database: the user's `SEC$ADMIN` flag (GRANT ADMIN ROLE:
//!   creates databases and manages users) and `SEC$DB_CREATORS` (GRANT
//!   CREATE DATABASE, to the user or to a role in use).
//! - Firebird 4+ system privileges (`RDB$SYSTEM_PRIVILEGE`):
//!   MONITOR_ANY_ATTACHMENT, CREATE_DATABASE, DROP_DATABASE, USER_MANAGEMENT.
//!
//! Profiler = see the other users' attachments in MON$ (administrator,
//! owner or MONITOR_ANY_ATTACHMENT); create = administrator, SEC$ADMIN or
//! CREATE DATABASE (denied only on Firebird 3: from 4 on a role of the
//! security database can grant CREATE_DATABASE unseen); drop = administrator, owner or DROP_DATABASE; security
//! = administrator, owner (roles and grants in its database), SEC$ADMIN
//! (users) or USER_MANAGEMENT. Firebird has no native backup DBine runs
//! and no "end a session" action: those stay unknown.

use crate::{int, text, FirebirdSession};
use dbine_driver::{Access, Permissions, Result};

const WHO: &str = "SELECT TRIM(CURRENT_USER), TRIM(MON$OWNER), IIF(RDB$ROLE_IN_USE('RDB$ADMIN'), 1, 0) FROM MON$DATABASE";

const SECURITY_DB: &str = "SELECT
  (SELECT MAX(IIF(u.SEC$ADMIN, 1, 0)) FROM SEC$USERS u WHERE u.SEC$USER_NAME = CURRENT_USER),
  IIF(EXISTS(SELECT 1 FROM SEC$DB_CREATORS c
              WHERE (c.SEC$USER_TYPE = 8 AND c.SEC$USER = CURRENT_USER)
                 OR (c.SEC$USER_TYPE = 13 AND RDB$ROLE_IN_USE(TRIM(c.SEC$USER)))), 1, 0)
  FROM RDB$DATABASE";

const SYSTEM_PRIVILEGES: &str = "SELECT IIF(RDB$SYSTEM_PRIVILEGE(MONITOR_ANY_ATTACHMENT), 1, 0),
       IIF(RDB$SYSTEM_PRIVILEGE(CREATE_DATABASE), 1, 0),
       IIF(RDB$SYSTEM_PRIVILEGE(DROP_DATABASE), 1, 0),
       IIF(RDB$SYSTEM_PRIVILEGE(USER_MANAGEMENT), 1, 0)
  FROM RDB$DATABASE";

/// Firebird 4+ system privileges of the attachment.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct System {
    pub monitor: bool,
    pub create: bool,
    pub drop: bool,
    pub users: bool,
}

/// What the three queries answered.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts {
    pub user: String,
    pub owner: String,
    pub rdb_admin: bool,
    /// `None`: the security database couldn't be read (or doesn't list the user).
    pub sec_admin: Option<bool>,
    pub creator: Option<bool>,
    /// `None`: Firebird 3 (no system privileges) or the check failed.
    pub system: Option<System>,
}

/// The facts to what the UI allows. `drop_target`: the explorer named the
/// connection's database (drop refers to it).
pub(crate) fn decide(f: &Facts, drop_target: bool) -> Permissions {
    let admin = f.user == "SYSDBA" || f.rdb_admin;
    let owner = !f.user.is_empty() && f.user == f.owner;
    let sys = f.system.unwrap_or_default();
    let profiler = if admin || owner || sys.monitor {
        Access::Allowed
    } else if f.system.is_some() {
        Access::check(false, "MONITOR_ANY_ATTACHMENT")
    } else {
        Access::check(false, "SYSDBA o rol RDB$ADMIN")
    };
    let create_database = if admin || f.sec_admin == Some(true) || f.creator == Some(true) || sys.create {
        Access::Allowed
    } else if f.system.is_none() && f.sec_admin.is_some() && f.creator.is_some() {
        // Firebird 3: administrators and GRANT CREATE DATABASE are the only
        // ways. Firebird 4+ can also grant the CREATE_DATABASE system
        // privilege through a role of the security database, which this
        // database can't see: left unknown there.
        Access::check(false, "CREATE DATABASE")
    } else {
        Access::Unknown
    };
    let drop_database = if !drop_target {
        Access::Unknown
    } else if admin || owner || sys.drop {
        Access::Allowed
    } else if f.system.is_some() {
        Access::check(false, "DROP_DATABASE (o ser el dueño de la base)")
    } else {
        Access::check(false, "ser el dueño de la base o rol RDB$ADMIN")
    };
    let manage_security = if admin || owner || f.sec_admin == Some(true) || sys.users {
        Access::Allowed
    } else if f.sec_admin.is_some() {
        Access::check(false, if f.system.is_some() { "USER_MANAGEMENT o rol RDB$ADMIN" } else { "rol RDB$ADMIN" })
    } else {
        Access::Unknown
    };
    Permissions { profiler, create_database, drop_database, manage_security, ..Default::default() }
}

/// The first row of `sql`, or `None` when the server refused it.
async fn first_row(s: &FirebirdSession, sql: &'static str) -> Option<Vec<rsfbclient_core::Column>> {
    match s.rows(sql, vec![]).await {
        Ok(rows) => rows.into_iter().next(),
        Err(e) => {
            tracing::debug!("firebird: permissions check refused: {e}");
            None
        }
    }
}

pub(crate) async fn check(s: &FirebirdSession, database: Option<&str>) -> Result<Permissions> {
    let Some(who) = first_row(s, WHO).await else {
        // Firebird 3+ always answers this: only a dead attachment fails here too.
        s.rows("SELECT 1 FROM RDB$DATABASE", vec![]).await?;
        return Ok(Permissions::default());
    };
    let flag = |r: &[rsfbclient_core::Column], i: usize| r.get(i).and_then(int).map(|v| v == 1);
    let sec = first_row(s, SECURITY_DB).await;
    let system = first_row(s, SYSTEM_PRIVILEGES).await.map(|r| System {
        monitor: flag(&r, 0) == Some(true),
        create: flag(&r, 1) == Some(true),
        drop: flag(&r, 2) == Some(true),
        users: flag(&r, 3) == Some(true),
    });
    let facts = Facts {
        user: who.first().and_then(text).unwrap_or_default(),
        owner: who.get(1).and_then(text).unwrap_or_default(),
        rdb_admin: flag(&who, 2) == Some(true),
        sec_admin: sec.as_deref().and_then(|r| flag(r, 0)),
        creator: sec.as_deref().and_then(|r| flag(r, 1)),
        system,
    };
    let drop_target = database.map(str::trim).is_some_and(|d| !d.is_empty() && d == s.database.trim());
    Ok(decide(&facts, drop_target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn plain(system: Option<System>) -> Facts {
        Facts { user: "ANA".into(), owner: "DBINE".into(), sec_admin: Some(false), creator: Some(false), system, ..Default::default() }
    }

    #[test]
    fn sysdba_and_rdb_admin_may_do_everything_offered() {
        for f in [Facts { user: "SYSDBA".into(), ..Default::default() }, Facts { user: "ANA".into(), rdb_admin: true, ..Default::default() }] {
            let p = decide(&f, true);
            assert_eq!((p.profiler, p.create_database, p.drop_database, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
            assert_eq!((p.backup, p.restore, p.kill_session), (Access::Unknown, Access::Unknown, Access::Unknown));
            assert_eq!(decide(&f, false).drop_database, Access::Unknown);
        }
    }

    #[test]
    fn a_plain_user_is_denied_in_firebird_4_terms() {
        let p = decide(&plain(Some(System::default())), true);
        assert!(denied(&p.profiler, "MONITOR_ANY_ATTACHMENT"));
        // A security-database role could grant CREATE_DATABASE unseen.
        assert_eq!(p.create_database, Access::Unknown);
        assert!(denied(&p.drop_database, "DROP_DATABASE"));
        assert!(denied(&p.manage_security, "USER_MANAGEMENT"));
    }

    #[test]
    fn firebird_3_names_the_admin_role() {
        let p = decide(&plain(None), true);
        assert!(denied(&p.profiler, "RDB$ADMIN"));
        assert!(denied(&p.create_database, "CREATE DATABASE"));
        assert!(denied(&p.drop_database, "dueño"));
        assert!(denied(&p.manage_security, "RDB$ADMIN"));
    }

    #[test]
    fn owner_monitors_drops_and_manages_its_database() {
        let f = Facts { user: "DBINE".into(), ..plain(Some(System::default())) };
        let p = decide(&f, true);
        assert_eq!((p.profiler, p.drop_database, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!(p.create_database, Access::Unknown);
    }

    #[test]
    fn grants_and_security_admins() {
        let f = Facts { creator: Some(true), ..plain(Some(System::default())) };
        assert_eq!(decide(&f, true).create_database, Access::Allowed);
        let f = Facts { sec_admin: Some(true), ..plain(Some(System::default())) };
        let p = decide(&f, true);
        assert_eq!((p.create_database, p.manage_security), (Access::Allowed, Access::Allowed));
        assert!(p.profiler.is_denied());
        let f = plain(Some(System { monitor: true, ..Default::default() }));
        assert_eq!(decide(&f, true).profiler, Access::Allowed);
    }

    #[test]
    fn an_unreadable_security_database_stays_unknown() {
        let f = Facts { sec_admin: None, creator: None, ..plain(Some(System::default())) };
        let p = decide(&f, true);
        assert_eq!((p.create_database, p.manage_security), (Access::Unknown, Access::Unknown));
        assert!(p.profiler.is_denied());
    }
}

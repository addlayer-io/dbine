//! What the login may do (`Session::permissions`), for the presets where
//! one read answers it:
//!
//! - **Db2 for LUW**: the authorities of the session's user, direct or
//!   through groups, roles and PUBLIC, from
//!   `SYSPROC.AUTH_LIST_AUTHORITIES_FOR_AUTHID`. `BACKUP DATABASE` and
//!   `FORCE APPLICATION` (both through ADMIN_CMD) need SYSADM, SYSCTRL or
//!   SYSMAINT; roles and database-wide grants need SECADM or ACCESSCTRL;
//!   `CREATE SCHEMA` with any name and owner, DBADM.
//! - **SAP ASE**: the active system roles (`show_role()`) and whether the
//!   login owns the database (`master..sysdatabases.suid`). `DUMP` / `LOAD
//!   DATABASE` need sa_role, oper_role or being its owner; `KILL` sa_role;
//!   `DROP DATABASE` sa_role or the owner; logins and roles sso_role.
//!   `CREATE DATABASE` may also be granted to a login, which no simple read
//!   shows: allowed with sa_role, unknown otherwise.
//!
//! The other presets stay unknown (docs/soporte-por-motor.md). A failed
//! read leaves its fields unknown: an ODBC error doesn't tell a dead
//! connection from a missing view.

use crate::OdbcSession;
use dbine_driver::{Access, Permissions, Result};

const DB2_ADMIN: &str = "SYSADM, SYSCTRL o SYSMAINT";

/// Db2: authorities held from `AUTH_LIST_AUTHORITIES_FOR_AUTHID` rows
/// (`AUTHORITY` then its `Y` / `N` / `*` columns).
pub(crate) fn db2_held(rows: &[Vec<Option<String>>]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.iter().skip(1).any(|c| c.as_deref().map(str::trim) == Some("Y")))
        .filter_map(|r| r.first().cloned().flatten().map(|a| a.trim().to_uppercase()))
        .collect()
}

pub(crate) fn db2(held: &[String]) -> Permissions {
    let has = |a: &str| held.iter().any(|h| h == a);
    let admin = Access::check(has("SYSADM") || has("SYSCTRL") || has("SYSMAINT"), DB2_ADMIN);
    Permissions {
        backup: admin.clone(),
        kill_session: admin,
        manage_security: Access::check(has("SECADM") || has("ACCESSCTRL"), "SECADM o ACCESSCTRL"),
        // Without DBADM a user may still create the schema named after
        // itself: not a denial.
        create_schema: if has("DBADM") { Access::Allowed } else { Access::Unknown },
        ..Default::default()
    }
}

/// ASE: from the active roles (`show_role()`, space-separated) and whether
/// the login owns the database (`None`: unknown).
pub(crate) fn ase(roles: &str, owner: Option<bool>) -> Permissions {
    let has = |r: &str| roles.split_whitespace().any(|x| x.eq_ignore_ascii_case(r));
    let (sa, sso, oper) = (has("sa_role"), has("sso_role"), has("oper_role"));
    let dump = match (sa || oper, owner) {
        (true, _) | (_, Some(true)) => Access::Allowed,
        (false, Some(false)) => Access::Denied { missing: "sa_role, oper_role o ser el dueño de la base".into() },
        (false, None) => Access::Unknown,
    };
    let drop = match (sa, owner) {
        (true, _) | (_, Some(true)) => Access::Allowed,
        (false, Some(false)) => Access::Denied { missing: "sa_role o ser el dueño de la base".into() },
        (false, None) => Access::Unknown,
    };
    Permissions {
        backup: dump.clone(),
        restore: dump,
        kill_session: Access::check(sa, "sa_role"),
        create_database: if sa { Access::Allowed } else { Access::Unknown },
        drop_database: drop,
        manage_security: Access::check(sso, "sso_role"),
        ..Default::default()
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn first(rows: &[Vec<Option<String>>]) -> Option<String> {
    rows.first()?.first()?.clone()
}

pub(crate) async fn check(s: &OdbcSession, database: Option<&str>) -> Result<Permissions> {
    Ok(match s.preset.id {
        "db2" => {
            let sql = "SELECT AUTHORITY, D_USER, D_GROUP, D_PUBLIC, ROLE_USER, ROLE_GROUP, ROLE_PUBLIC, D_ROLE \
                       FROM TABLE (SYSPROC.AUTH_LIST_AUTHORITIES_FOR_AUTHID (SESSION_USER, 'U')) AS T";
            match s.query(sql.into(), Vec::new()).await {
                Ok(rows) if !rows.is_empty() => db2(&db2_held(&rows)),
                _ => Permissions::default(),
            }
        }
        "sybase" => {
            let Ok(roles) = s.query("select show_role()".into(), Vec::new()).await else { return Ok(Permissions::default()) };
            let roles = first(&roles).unwrap_or_default();
            let db = database.filter(|d| !d.trim().is_empty()).unwrap_or(&s.database);
            let owner = if db.is_empty() {
                None
            } else {
                let sql = format!(
                    "select case when suid = suser_id() then 1 else 0 end from master..sysdatabases where name = {}",
                    lit(db)
                );
                s.query(sql, Vec::new()).await.ok().and_then(|r| first(&r)).map(|v| v.trim() == "1")
            };
            ase(&roles, owner)
        }
        _ => Permissions::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(a: &str, cols: &[&str]) -> Vec<Option<String>> {
        std::iter::once(Some(a.to_string())).chain(cols.iter().map(|c| Some(c.to_string()))).collect()
    }

    #[test]
    fn db2_authorities_through_any_path() {
        let rows = vec![
            row("SYSADM", &["N", "N", "N", "*", "*", "*", "N"]),
            row("SYSMAINT", &["N", "Y", "N", "*", "*", "*", "N"]),
            row("SECADM", &["N", "N", "N", "N", "N", "N", "N"]),
            row("ACCESSCTRL", &["N", "N", "N", "N", "N", "N", "Y"]),
        ];
        let held = db2_held(&rows);
        assert_eq!(held, ["SYSMAINT", "ACCESSCTRL"]);
        let p = db2(&held);
        assert_eq!((p.backup.clone(), p.kill_session.clone(), p.manage_security.clone()), (Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!(p.restore, Access::Unknown);

        let p = db2(&["DBADM".to_string()]);
        assert_eq!(p.backup, Access::Denied { missing: DB2_ADMIN.into() });
        assert_eq!(p.create_schema, Access::Allowed);
        assert_eq!(db2(&[]).create_schema, Access::Unknown);
        assert_eq!(p.manage_security, Access::Denied { missing: "SECADM o ACCESSCTRL".into() });
    }

    #[test]
    fn ase_roles_and_ownership() {
        let p = ase("sa_role sso_role oper_role", Some(false));
        assert_eq!(p.backup, Access::Allowed);
        assert_eq!(p.drop_database, Access::Allowed);
        assert_eq!(p.manage_security, Access::Allowed);
        assert_eq!(p.create_database, Access::Allowed);

        let owner = ase("", Some(true));
        assert_eq!((owner.backup.clone(), owner.restore.clone(), owner.drop_database.clone()), (Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!(owner.kill_session, Access::Denied { missing: "sa_role".into() });
        assert_eq!(owner.create_database, Access::Unknown);
        assert_eq!(owner.manage_security, Access::Denied { missing: "sso_role".into() });

        let nobody = ase("", Some(false));
        assert!(nobody.backup.is_denied() && nobody.restore.is_denied() && nobody.drop_database.is_denied());
        let oper = ase("OPER_ROLE", Some(false));
        assert_eq!((oper.backup, oper.drop_database.is_denied()), (Access::Allowed, true));
        assert_eq!(ase("", None).backup, Access::Unknown);
    }
}

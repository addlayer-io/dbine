//! What the login may do (`Session::permissions`), from one query on
//! `SESSION_PRIVS` and `SESSION_ROLES` (the privileges and roles enabled in
//! this session, through other roles too) and the direct grants on the V$
//! views the profiler reads.
//!
//! The "database" is a schema:
//! - backup / restore (Data Pump, see `backup`): of the session's own
//!   schema, CREATE TABLE (Data Pump's master table); of another one,
//!   DATAPUMP_EXP_FULL_DATABASE / DATAPUMP_IMP_FULL_DATABASE (or the old
//!   EXP_ / IMP_FULL_DATABASE). READ and WRITE on the directory aren't
//!   checked: the directory is chosen in the form.
//! - profiler: reading V$SESSION and V$SQLSTATS (SELECT_CATALOG_ROLE,
//!   SELECT ANY DICTIONARY or grants on both views).
//! - kill: ALTER SYSTEM. Create / drop (schema-only accounts): CREATE USER /
//!   DROP USER; `create_schema` too is CREATE USER (a schema is a user, so
//!   DBine has no "Nuevo esquema…" for Oracle: see `Driver::schema_spec`). Security: CREATE USER, ALTER USER, CREATE ROLE or GRANT ANY
//!   PRIVILEGE / ROLE.
//!
//! A check the server refuses leaves everything unknown; only a broken
//! connection is an error.

use crate::{db_code, err};
use dbine_driver::{Access, Permissions, Result};
use oracledb::Connection;

const SQL: &str = "SELECT SYS_CONTEXT('USERENV', 'SESSION_USER'),
       (SELECT LISTAGG(privilege, ',') WITHIN GROUP (ORDER BY privilege) FROM session_privs
         WHERE privilege IN ('SELECT ANY DICTIONARY', 'ALTER SYSTEM', 'CREATE USER', 'ALTER USER', 'DROP USER', 'CREATE ROLE',
                             'GRANT ANY PRIVILEGE', 'GRANT ANY ROLE', 'CREATE TABLE', 'CREATE ANY TABLE')),
       (SELECT LISTAGG(role, ',') WITHIN GROUP (ORDER BY role) FROM session_roles
         WHERE role IN ('SELECT_CATALOG_ROLE', 'DATAPUMP_EXP_FULL_DATABASE', 'DATAPUMP_IMP_FULL_DATABASE',
                        'EXP_FULL_DATABASE', 'IMP_FULL_DATABASE')),
       (SELECT TO_CHAR(COUNT(DISTINCT table_name)) FROM all_tab_privs
         WHERE table_schema = 'SYS' AND table_name IN ('V_$SESSION', 'V_$SQLSTATS') AND privilege IN ('SELECT', 'READ'))
  FROM dual";

/// The query's answer.
#[derive(Debug, Default)]
pub(crate) struct Grants {
    pub user: String,
    pub privileges: Vec<String>,
    pub roles: Vec<String>,
    /// How many of V_$SESSION / V_$SQLSTATS the session may read by grant.
    pub v_views: usize,
}

impl Grants {
    fn has(&self, p: &str) -> bool {
        self.privileges.iter().any(|x| x == p) || self.roles.iter().any(|x| x == p)
    }

    fn any(&self, ps: &[&str]) -> bool {
        ps.iter().any(|p| self.has(p))
    }
}

/// `database`: the explorer's schema (`None`: the server as a whole).
pub(crate) fn map(g: &Grants, database: Option<&str>) -> Permissions {
    let schema = database.map(str::trim).filter(|d| !d.is_empty());
    let own = schema.map(|s| s == g.user);
    let data_pump = |full: &[&str], full_name: &str| match own {
        None => Access::Unknown,
        Some(true) => Access::check(g.any(&["CREATE TABLE", "CREATE ANY TABLE"]), "CREATE TABLE"),
        Some(false) => Access::check(g.any(full), full_name),
    };
    Permissions {
        backup: data_pump(&["DATAPUMP_EXP_FULL_DATABASE", "EXP_FULL_DATABASE"], "DATAPUMP_EXP_FULL_DATABASE"),
        restore: data_pump(&["DATAPUMP_IMP_FULL_DATABASE", "IMP_FULL_DATABASE"], "DATAPUMP_IMP_FULL_DATABASE"),
        profiler: Access::check(
            g.any(&["SELECT ANY DICTIONARY", "SELECT_CATALOG_ROLE"]) || g.v_views >= 2,
            "SELECT_CATALOG_ROLE o SELECT ANY DICTIONARY (leer V$SESSION y V$SQLSTATS)",
        ),
        kill_session: Access::check(g.has("ALTER SYSTEM"), "ALTER SYSTEM"),
        create_database: Access::check(g.has("CREATE USER"), "CREATE USER"),
        drop_database: if schema.is_some() { Access::check(g.has("DROP USER"), "DROP USER") } else { Access::Unknown },
        manage_security: Access::check(
            g.any(&["CREATE USER", "ALTER USER", "CREATE ROLE", "GRANT ANY PRIVILEGE", "GRANT ANY ROLE"]),
            "CREATE USER, ALTER USER o GRANT ANY PRIVILEGE",
        ),
        // A schema is a user: creating one is CREATE USER.
        create_schema: Access::check(g.has("CREATE USER"), "CREATE USER"),
    }
}

fn list(v: Option<String>) -> Vec<String> {
    v.map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// Runs the check; `None` when the server refused it (an ORA- error).
pub(crate) fn grants(c: &Connection) -> Result<Option<Grants>> {
    let row = c
        .statement(SQL)
        .map(|b| b.exclude_from_cache())
        .and_then(|b| b.build())
        .and_then(|stmt| stmt.query_row(&[]))
        .and_then(|r| (0..4).map(|i| r.get::<Option<String>>(i)).collect::<std::result::Result<Vec<_>, _>>());
    let mut row = match row {
        Ok(r) => r.into_iter(),
        Err(e) if db_code(&e).is_some() => {
            tracing::debug!("oracle: permissions check refused: {e}");
            return Ok(None);
        }
        Err(e) => return Err(err(e)),
    };
    let mut next = || row.next().flatten();
    Ok(Some(Grants {
        user: next().unwrap_or_default(),
        privileges: list(next()),
        roles: list(next()),
        v_views: next().and_then(|n| n.parse().ok()).unwrap_or(0),
    }))
}

pub(crate) fn check(c: &Connection, database: Option<&str>) -> Result<Permissions> {
    Ok(grants(c)?.map(|g| map(&g, database)).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(privileges: &[&str], roles: &[&str], v_views: usize) -> Grants {
        Grants {
            user: "ANA".into(),
            privileges: privileges.iter().map(|s| s.to_string()).collect(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            v_views,
        }
    }

    #[test]
    fn a_dba_may_do_everything() {
        let dba = g(
            &["ALTER SYSTEM", "CREATE USER", "DROP USER", "ALTER USER", "SELECT ANY DICTIONARY", "CREATE TABLE"],
            &["DATAPUMP_EXP_FULL_DATABASE", "DATAPUMP_IMP_FULL_DATABASE", "SELECT_CATALOG_ROLE"],
            0,
        );
        assert_eq!(map(&dba, Some("HR")), Permissions::all());
        assert_eq!(map(&dba, None).backup, Access::Unknown);
        assert_eq!(map(&dba, None).drop_database, Access::Unknown);
    }

    #[test]
    fn a_plain_user_exports_only_its_own_schema() {
        let u = g(&["CREATE TABLE"], &[], 0);
        let own = map(&u, Some("ANA"));
        assert_eq!((own.backup, own.restore), (Access::Allowed, Access::Allowed));
        let other = map(&u, Some("HR"));
        assert_eq!(other.backup, Access::Denied { missing: "DATAPUMP_EXP_FULL_DATABASE".into() });
        assert_eq!(other.restore, Access::Denied { missing: "DATAPUMP_IMP_FULL_DATABASE".into() });
        assert_eq!(other.kill_session, Access::Denied { missing: "ALTER SYSTEM".into() });
        assert_eq!(other.create_database, Access::Denied { missing: "CREATE USER".into() });
        assert_eq!(other.create_schema, Access::Denied { missing: "CREATE USER".into() });
        assert_eq!(other.drop_database, Access::Denied { missing: "DROP USER".into() });
        assert!(other.profiler.is_denied());
        assert!(other.manage_security.is_denied());
    }

    #[test]
    fn profiler_by_grants_on_both_views() {
        assert_eq!(map(&g(&[], &[], 2), None).profiler, Access::Allowed);
        assert!(map(&g(&[], &[], 1), None).profiler.is_denied());
        assert_eq!(map(&g(&[], &["SELECT_CATALOG_ROLE"], 0), None).profiler, Access::Allowed);
    }

    #[test]
    fn lists() {
        assert_eq!(list(Some("A,B C".into())), vec!["A".to_string(), "B C".to_string()]);
        assert!(list(None).is_empty());
    }
}

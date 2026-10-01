//! What the login may do (`Session::permissions`), from one query on
//! `EFFECTIVE_PRIVILEGES` (the user's privileges, through its roles too):
//!
//! - backup: BACKUP ADMIN / BACKUP OPERATOR (DATABASE BACKUP ADMIN /
//!   OPERATOR in SYSTEMDB, for a tenant).
//! - restore (`RECOVER DATA FOR <tenant>`, from SYSTEMDB): DATABASE ADMIN or
//!   DATABASE RECOVERY OPERATOR, which only SYSTEMDB grants.
//! - profiler: CATALOG READ (in the MONITORING role) or DATA ADMIN, to read
//!   other connections' statements. INIFILE ADMIN only decides between the
//!   complete and the sampled mode (see `profiler`).
//! - kill (`ALTER SYSTEM DISCONNECT SESSION`): SESSION ADMIN.
//! - create / drop (schemas): CREATE SCHEMA; the schema's owner or DROP on
//!   it. `create_schema` is the same check, though the explorer has no
//!   "Nuevo esquema…" here: HANA's schemas are DBine's databases.
//! - security: USER ADMIN or ROLE ADMIN.
//!
//! Untested against a server: there's no HANA to run against (see
//! `tests/integration.rs`). A check the server refuses leaves everything
//! unknown; only a broken connection is an error.

use crate::{err, int, text, HanaSession};
use dbine_driver::{Access, Permissions, Result};
use hdbconnect_async::HdbValue;

const SQL: &str = "SELECT
  (SELECT STRING_AGG(PRIVILEGE, ',') FROM (SELECT DISTINCT PRIVILEGE FROM SYS.EFFECTIVE_PRIVILEGES
     WHERE USER_NAME = CURRENT_USER AND OBJECT_TYPE = 'SYSTEMPRIVILEGE' AND IS_VALID = 'TRUE'
       AND PRIVILEGE IN ('BACKUP ADMIN', 'BACKUP OPERATOR', 'DATABASE BACKUP ADMIN', 'DATABASE BACKUP OPERATOR',
                         'DATABASE ADMIN', 'DATABASE RECOVERY OPERATOR', 'CATALOG READ', 'DATA ADMIN',
                         'SESSION ADMIN', 'CREATE SCHEMA', 'USER ADMIN', 'ROLE ADMIN'))),
  (SELECT COUNT(*) FROM SYS.SCHEMAS WHERE SCHEMA_NAME = ? AND SCHEMA_OWNER = CURRENT_USER),
  (SELECT COUNT(*) FROM SYS.EFFECTIVE_PRIVILEGES WHERE USER_NAME = CURRENT_USER AND OBJECT_TYPE = 'SCHEMA'
     AND SCHEMA_NAME = ? AND PRIVILEGE IN ('DROP', 'ALL PRIVILEGES') AND IS_VALID = 'TRUE')
FROM DUMMY";

/// The query's answer.
#[derive(Debug, Default)]
pub(crate) struct Grants {
    pub privileges: Vec<String>,
    /// The user owns the explorer's schema, or has DROP on it.
    pub may_drop: bool,
}

/// `database`: the explorer's schema (`None`: the server as a whole).
pub(crate) fn map(g: &Grants, database: Option<&str>) -> Permissions {
    let any = |ps: &[&str]| ps.iter().any(|p| g.privileges.iter().any(|x| x == p));
    Permissions {
        backup: Access::check(
            any(&["BACKUP ADMIN", "BACKUP OPERATOR", "DATABASE BACKUP ADMIN", "DATABASE BACKUP OPERATOR"]),
            "BACKUP ADMIN o BACKUP OPERATOR",
        ),
        restore: Access::check(
            any(&["DATABASE ADMIN", "DATABASE RECOVERY OPERATOR"]),
            "DATABASE ADMIN o DATABASE RECOVERY OPERATOR (en SYSTEMDB)",
        ),
        profiler: Access::check(any(&["CATALOG READ", "DATA ADMIN"]), "CATALOG READ (rol MONITORING)"),
        kill_session: Access::check(any(&["SESSION ADMIN"]), "SESSION ADMIN"),
        create_database: Access::check(any(&["CREATE SCHEMA"]), "CREATE SCHEMA"),
        drop_database: match database.map(str::trim).filter(|d| !d.is_empty()) {
            Some(_) => Access::check(g.may_drop, "DROP sobre el esquema (o ser su dueño)"),
            None => Access::Unknown,
        },
        manage_security: Access::check(any(&["USER ADMIN", "ROLE ADMIN"]), "USER ADMIN o ROLE ADMIN"),
        create_schema: Access::check(any(&["CREATE SCHEMA"]), "CREATE SCHEMA"),
    }
}

fn list(v: Option<String>) -> Vec<String> {
    v.map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

pub(crate) async fn check(s: &HanaSession, database: Option<&str>) -> Result<Permissions> {
    let db = database.map(str::trim).unwrap_or("");
    let rows = async {
        let response = s.conn.prepare_and_execute(SQL, &vec![db, db]).await?;
        response.into_result_set()?.into_rows().await
    }
    .await;
    let row: Vec<HdbValue<'static>> = match rows {
        Ok(rows) => rows.into_iter().next().map(|r| r.into_iter().collect()).unwrap_or_default(),
        Err(e) if e.server_error().is_some() => {
            tracing::debug!("hana: permissions check refused: {e}");
            return Ok(Permissions::default());
        }
        Err(e) => return Err(err(e)),
    };
    let n = |i: usize| row.get(i).and_then(int).unwrap_or(0);
    let g = Grants { privileges: list(row.first().and_then(text)), may_drop: n(1) > 0 || n(2) > 0 };
    Ok(map(&g, database))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(privileges: &[&str], may_drop: bool) -> Grants {
        Grants { privileges: privileges.iter().map(|s| s.to_string()).collect(), may_drop }
    }

    #[test]
    fn system_may_do_everything() {
        let all = g(
            &["BACKUP ADMIN", "DATABASE ADMIN", "CATALOG READ", "SESSION ADMIN", "CREATE SCHEMA", "USER ADMIN", "ROLE ADMIN"],
            true,
        );
        assert_eq!(map(&all, Some("VENTAS")), Permissions::all());
        assert_eq!(map(&all, None).drop_database, Access::Unknown);
    }

    #[test]
    fn a_plain_user_is_denied_what_it_lacks() {
        let p = map(&g(&[], false), Some("VENTAS"));
        assert_eq!(p.backup, Access::Denied { missing: "BACKUP ADMIN o BACKUP OPERATOR".into() });
        assert!(p.restore.is_denied());
        assert_eq!(p.profiler, Access::Denied { missing: "CATALOG READ (rol MONITORING)".into() });
        assert_eq!(p.kill_session, Access::Denied { missing: "SESSION ADMIN".into() });
        assert_eq!(p.create_database, Access::Denied { missing: "CREATE SCHEMA".into() });
        assert_eq!(p.create_schema, Access::Denied { missing: "CREATE SCHEMA".into() });
        assert!(p.drop_database.is_denied());
        assert!(p.manage_security.is_denied());
    }

    #[test]
    fn operators_and_owners() {
        let p = map(&g(&["BACKUP OPERATOR", "DATA ADMIN", "ROLE ADMIN"], true), Some("MIO"));
        assert_eq!((p.backup, p.profiler, p.manage_security, p.drop_database), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!(list(Some("A, B".into())), vec!["A".to_string(), "B".to_string()]);
    }
}

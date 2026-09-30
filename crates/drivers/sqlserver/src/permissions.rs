//! What the login may do (`Session::permissions`), from the server's own
//! answer in one query: `IS_SRVROLEMEMBER` and `HAS_PERMS_BY_NAME` on the
//! server and on the explorer's database (the connection's when `None`).
//!
//! - SQL Server (and Managed Instance): backup = BACKUP DATABASE on the
//!   database (db_owner, db_backupoperator); restore = sysadmin, dbcreator
//!   or the database's owner (RESTORE's own rule); profiler = VIEW SERVER
//!   STATE (without it the DMVs and the event files only show the login's
//!   own session; ALTER ANY EVENT SESSION only decides between the complete
//!   and the sampled mode, see `profiler`); kill = ALTER ANY CONNECTION;
//!   create = CREATE ANY DATABASE; drop = CONTROL on the database;
//!   security = ALTER ANY LOGIN or ALTER ANY USER.
//! - Azure SQL Database: no server scope. Kill = KILL DATABASE CONNECTION,
//!   profiler = VIEW DATABASE STATE, security = ALTER ANY USER. Creating and
//!   dropping databases is decided in `master` (dbmanager), which a
//!   connection to another database can't see: left unknown.
//! - Fabric and Babelfish: left unknown (no HAS_PERMS_BY_NAME to trust).
//!
//! A check the server refuses leaves everything unknown; only a broken
//! connection is an error.

use crate::variant::Variant;
use crate::{err, is_desync, SqlServerSession};
use dbine_driver::{Access, Permissions, Result};
use tiberius::Row;

/// SQL Server and Managed Instance. `@P1`: the database ('' = current).
const SERVER_SQL: &str = "SELECT CAST(IS_SRVROLEMEMBER('sysadmin') AS int),
       CAST(IS_SRVROLEMEMBER('dbcreator') AS int),
       CAST(HAS_PERMS_BY_NAME(t.n, 'DATABASE', 'BACKUP DATABASE') AS int),
       CAST((SELECT CASE WHEN d.owner_sid = SUSER_SID() THEN 1 ELSE 0 END FROM sys.databases d WHERE d.name = t.n) AS int),
       CAST(HAS_PERMS_BY_NAME(t.n, 'DATABASE', 'CONTROL') AS int),
       CAST(HAS_PERMS_BY_NAME(NULL, NULL, 'CREATE ANY DATABASE') AS int),
       CAST(HAS_PERMS_BY_NAME(NULL, NULL, 'ALTER ANY CONNECTION') AS int),
       CAST(HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE') AS int),
       CAST(HAS_PERMS_BY_NAME(NULL, NULL, 'ALTER ANY LOGIN') AS int),
       CAST(HAS_PERMS_BY_NAME(t.n, 'DATABASE', 'ALTER ANY USER') AS int)
  FROM (SELECT COALESCE(NULLIF(@P1, N''), DB_NAME()) AS n) t";

/// Azure SQL Database: the connection's own database only.
const AZURE_SQL: &str = "SELECT CAST(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'KILL DATABASE CONNECTION') AS int),
       CAST(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DATABASE STATE') AS int),
       CAST(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'ALTER ANY USER') AS int)";

/// `Some(true/false)` per column; NULL (the server can't tell) is `None`.
fn flags(r: &Row, n: usize) -> Vec<Option<bool>> {
    (0..n).map(|i| r.try_get::<i32, _>(i).ok().flatten().map(|v| v == 1)).collect()
}

/// Allowed / denied, or unknown when the check gave NULL.
fn access(v: Option<bool>, missing: &str) -> Access {
    v.map_or(Access::Unknown, |ok| Access::check(ok, missing))
}

fn any(v: &[Option<bool>]) -> Option<bool> {
    if v.contains(&Some(true)) {
        Some(true)
    } else if v.iter().all(Option::is_some) {
        Some(false)
    } else {
        None
    }
}

/// SQL Server's flags (in `SERVER_SQL`'s order) to what the UI allows.
/// `drop_target`: the explorer named a database (drop refers to it).
pub(crate) fn server(f: &[Option<bool>], drop_target: bool) -> Permissions {
    let get = |i: usize| f.get(i).copied().flatten();
    let (sysadmin, dbcreator) = (get(0), get(1));
    if sysadmin == Some(true) {
        let mut p = Permissions::all();
        if !drop_target {
            p.drop_database = Access::Unknown;
        }
        return p;
    }
    Permissions {
        backup: access(get(2), "BACKUP DATABASE"),
        restore: access(any(&[sysadmin, dbcreator, get(3)]), "rol sysadmin o dbcreator (o ser el dueño de la base)"),
        profiler: access(get(7), "VIEW SERVER STATE"),
        kill_session: access(get(6), "ALTER ANY CONNECTION"),
        create_database: access(get(5), "CREATE ANY DATABASE"),
        drop_database: if drop_target { access(get(4), "CONTROL sobre la base") } else { Access::Unknown },
        manage_security: access(any(&[get(8), get(9)]), "ALTER ANY LOGIN o ALTER ANY USER"),
    }
}

/// Azure SQL Database's flags (in `AZURE_SQL`'s order).
pub(crate) fn azure(f: &[Option<bool>]) -> Permissions {
    let get = |i: usize| f.get(i).copied().flatten();
    Permissions {
        kill_session: access(get(0), "KILL DATABASE CONNECTION"),
        profiler: access(get(1), "VIEW DATABASE STATE"),
        manage_security: access(get(2), "ALTER ANY USER"),
        ..Default::default()
    }
}

/// The first row of `sql`; `None` when the server refused the query.
async fn first_row(s: &mut SqlServerSession, sql: &str, params: &[&str]) -> Result<Option<Row>> {
    let mut r = s.try_rows(sql, params).await;
    if matches!(&r, Err(e) if is_desync(e)) {
        s.reconnect().await?;
        r = s.try_rows(sql, params).await;
    }
    match r {
        Ok(rows) => Ok(rows.into_iter().next()),
        Err(tiberius::error::Error::Server(e)) => {
            tracing::debug!("sqlserver: permissions check refused: {}", e.message());
            Ok(None)
        }
        Err(e) => Err(err(e)),
    }
}

pub(crate) async fn check(s: &mut SqlServerSession, database: Option<&str>) -> Result<Permissions> {
    let db = database.map(str::trim).unwrap_or("");
    Ok(match s.variant {
        Variant::SqlServer => {
            first_row(s, SERVER_SQL, &[db]).await?.map(|r| server(&flags(&r, 10), !db.is_empty())).unwrap_or_default()
        }
        Variant::AzureSql => first_row(s, AZURE_SQL, &[]).await?.map(|r| azure(&flags(&r, 3))).unwrap_or_default(),
        _ => Permissions::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    #[test]
    fn sysadmin_may_do_everything() {
        let f = [Some(true), Some(false), Some(false), Some(false), Some(false), Some(false), Some(false), Some(false), Some(false), Some(false)];
        assert_eq!(server(&f, true), Permissions::all());
        assert_eq!(server(&f, false).drop_database, Access::Unknown);
    }

    #[test]
    fn a_plain_login_is_denied_what_it_lacks() {
        let no = [Some(false); 10];
        let p = server(&no, true);
        assert!(denied(&p.backup, "BACKUP DATABASE"));
        assert!(denied(&p.restore, "sysadmin o dbcreator"));
        assert!(denied(&p.profiler, "VIEW SERVER STATE"));
        assert!(denied(&p.kill_session, "ALTER ANY CONNECTION"));
        assert!(denied(&p.create_database, "CREATE ANY DATABASE"));
        assert!(denied(&p.drop_database, "CONTROL"));
        assert!(denied(&p.manage_security, "ALTER ANY LOGIN"));
    }

    #[test]
    fn db_owner_backs_up_restores_and_drops_its_database() {
        // Owner of the database, BACKUP / CONTROL / ALTER ANY USER on it.
        let f = [Some(false), Some(false), Some(true), Some(true), Some(true), Some(false), Some(false), Some(false), Some(false), Some(true)];
        let p = server(&f, true);
        assert_eq!((p.backup, p.restore, p.drop_database, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
        assert!(p.create_database.is_denied());
    }

    #[test]
    fn nulls_stay_unknown() {
        let f = [Some(false), Some(false), None, None, None, None, None, None, None, None];
        let p = server(&f, true);
        assert_eq!(p.backup, Access::Unknown);
        assert_eq!(p.restore, Access::Unknown);
        assert_eq!(p.manage_security, Access::Unknown);
        let dbcreator = [Some(false), Some(true), None, None, None, None, None, None, None, None];
        assert_eq!(server(&dbcreator, true).restore, Access::Allowed);
    }

    #[test]
    fn azure_sql_database() {
        let p = azure(&[Some(false), Some(true), None]);
        assert!(denied(&p.kill_session, "KILL DATABASE CONNECTION"));
        assert_eq!(p.profiler, Access::Allowed);
        assert_eq!((p.manage_security, p.backup, p.create_database), (Access::Unknown, Access::Unknown, Access::Unknown));
    }
}

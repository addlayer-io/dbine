//! What the connection may do (`Session::permissions`). SQLite has no
//! logins: the only action DBine offers that needs more than reading is the
//! backup (`VACUUM INTO`), which reads the database and writes a new file.
//! It works from a connection opened read-only, but not with
//! `PRAGMA query_only` on (SQLite refuses it as a write). The destination
//! folder is chosen later, so it can't be checked here. Restore, the
//! profiler, sessions, databases and users don't exist: they stay unknown.

use dbine_driver::{Access, Permissions};
use rusqlite::Connection;

pub(crate) fn decide(query_only: bool) -> Permissions {
    Permissions { backup: Access::check(!query_only, "PRAGMA query_only = OFF (la conexión no permite escrituras)"), ..Default::default() }
}

/// `PRAGMA query_only`; `None` when it can't be read.
pub(crate) fn query_only(c: &Connection) -> Option<bool> {
    c.query_row("PRAGMA query_only", [], |r| r.get::<_, i64>(0)).ok().map(|v| v != 0)
}

pub(crate) fn check(c: &Connection) -> Permissions {
    query_only(c).map(decide).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OpenFlags;

    #[test]
    fn backup_is_allowed_unless_query_only() {
        assert_eq!(decide(false).backup, Access::Allowed);
        assert!(decide(true).backup.is_denied());
        assert_eq!((decide(false).restore, decide(false).drop_database), (Access::Unknown, Access::Unknown));
    }

    #[test]
    fn read_only_files_still_back_up() {
        let dir = std::env::temp_dir().join(format!("dbine-sqlite-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, copy, copy2) = (dir.join("a.db"), dir.join("copy.db"), dir.join("copy2.db"));
        let _ = std::fs::remove_file(&copy);
        let _ = std::fs::remove_file(&copy2);
        Connection::open(&db).unwrap().execute_batch("CREATE TABLE IF NOT EXISTS t (x); INSERT INTO t VALUES (1);").unwrap();
        let mut perm = std::fs::metadata(&db).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&db, perm.clone()).unwrap();

        // Opened read-only: the backup is allowed, and VACUUM INTO does work.
        let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(check(&c).backup, Access::Allowed);
        c.execute_batch(&format!("VACUUM INTO '{}'", copy.display())).unwrap();

        // query_only: denied, and VACUUM INTO is refused.
        c.execute_batch("PRAGMA query_only = ON").unwrap();
        assert!(check(&c).backup.is_denied());
        assert!(c.execute_batch(&format!("VACUUM INTO '{}'", copy2.display())).is_err());

        drop(c);
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&db, perm).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! What the connection may do (`Session::permissions`). DuckDB has no
//! logins: what stops an action is a database opened read-only
//! (`access_mode = 'read_only'`, or an attached catalog with READ_ONLY),
//! which `duckdb_databases().readonly` tells.
//!
//! - backup (`EXPORT DATABASE`) only reads the database: allowed even
//!   read-only (the destination folder is chosen later).
//! - restore (`IMPORT DATABASE`) writes into the target catalog: denied
//!   when it's read-only.
//! - create (`ATTACH` of a new file): denied when the instance was opened
//!   read-only (DuckDB then opens every attached file read-only, and a new
//!   one "does not exist").
//! - drop (`DETACH`, then the file is deleted): `DETACH` works even
//!   read-only, and deleting the file is up to the file system: allowed.
//!
//! The profiler, sessions and users don't exist: they stay unknown.

use dbine_driver::{Access, Permissions};
use duckdb::Connection;

const MISSING: &str = "permiso de escritura sobre el archivo (la base está abierta en solo lectura)";

/// `instance_read_only`: `access_mode` is `read_only`. `target_read_only`:
/// the catalog a restore writes to (`None`: couldn't tell).
pub(crate) fn decide(instance_read_only: Option<bool>, target_read_only: Option<bool>) -> Permissions {
    let writable = |ro: Option<bool>| ro.map_or(Access::Unknown, |ro| Access::check(!ro, MISSING));
    Permissions {
        backup: Access::Allowed,
        restore: writable(target_read_only),
        create_database: writable(instance_read_only),
        drop_database: Access::Allowed,
        ..Default::default()
    }
}

pub(crate) fn check(c: &Connection, database: Option<&str>) -> Permissions {
    let instance = c
        .query_row("SELECT current_setting('access_mode')::VARCHAR", [], |r| r.get::<_, String>(0))
        .ok()
        .map(|m| m.eq_ignore_ascii_case("read_only"));
    let target = match database.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => c.query_row("SELECT readonly FROM duckdb_databases() WHERE database_name = ?1", [d], |r| r.get::<_, bool>(0)),
        None => c.query_row("SELECT readonly FROM duckdb_databases() WHERE database_name = current_database()", [], |r| r.get::<_, bool>(0)),
    }
    .ok();
    decide(instance, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::{AccessMode, Config};

    #[test]
    fn read_only_denies_the_writes() {
        let p = decide(Some(false), Some(false));
        assert_eq!((&p.backup, &p.restore, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        let p = decide(Some(true), Some(true));
        assert_eq!((&p.backup, &p.drop_database), (&Access::Allowed, &Access::Allowed));
        assert!(p.restore.is_denied() && p.create_database.is_denied());
        let p = decide(None, None);
        assert_eq!((&p.restore, &p.create_database), (&Access::Unknown, &Access::Unknown));
        assert_eq!((&p.profiler, &p.manage_security), (&Access::Unknown, &Access::Unknown));
    }

    /// Against real files: what's allowed works, what's denied fails.
    #[test]
    fn matches_what_duckdb_does() {
        crate::loader::ensure_blocking();
        let dir = std::env::temp_dir().join(format!("dbine-duckdb-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.duckdb");
        let lit = |p: &std::path::Path| format!("'{}'", p.display());
        {
            let c = Connection::open(&file).unwrap();
            c.execute_batch("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);").unwrap();
            let p = check(&c, None);
            assert_eq!((&p.restore, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
            c.execute_batch(&format!("ATTACH {} AS other", lit(&dir.join("other.duckdb")))).unwrap();
            assert_eq!(check(&c, Some("other")).restore, Access::Allowed);
        }
        let config = Config::default().access_mode(AccessMode::ReadOnly).unwrap();
        let c = Connection::open_with_flags(&file, config).unwrap();
        let p = check(&c, None);
        assert!(p.restore.is_denied() && p.create_database.is_denied());
        assert_eq!((&p.backup, &p.drop_database), (&Access::Allowed, &Access::Allowed));
        // Backup (EXPORT) and DETACH still work; restore (IMPORT) and ATTACH of a new file don't.
        let export = dir.join("export");
        c.execute_batch(&format!("EXPORT DATABASE {} (FORMAT CSV)", lit(&export))).unwrap();
        assert!(c.execute_batch(&format!("IMPORT DATABASE {}", lit(&export))).is_err());
        assert!(c.execute_batch(&format!("ATTACH {} AS created", lit(&dir.join("created.duckdb")))).is_err());
        c.execute_batch(&format!("ATTACH {} AS other", lit(&dir.join("other.duckdb")))).unwrap();
        assert!(check(&c, Some("other")).restore.is_denied());
        c.execute_batch("DETACH DATABASE other").unwrap();
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

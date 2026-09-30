//! What the user may do (`Session::permissions`), from `CHECK GRANT`
//! (ClickHouse 24.5+), which answers for the current user with its roles,
//! wildcards and partial revokes, and also with the session's `readonly`
//! setting (with `readonly` > 0 every write privilege checks as 0).
//!
//! DBine's read-only mode isn't looked at: this reports what the server
//! grants the user, and the read-only mode blocks the writes on its own.
//! Since that mode sends `readonly = 1` with every statement, the checks
//! run without it, so `readonly` is the user's profile's.
//!
//! - backup: BACKUP on the database, and WRITE on some destination (DISK,
//!   FILE or S3; the source privileges of recent versions: older ones don't
//!   parse the check and it's skipped).
//! - restore: CREATE TABLE and INSERT on the database, and READ on some
//!   source.
//! - profiler: SELECT on `system.query_log` (complete mode) or on
//!   `system.processes` (the sampled fallback).
//! - create: CREATE DATABASE on `*.*`. Without it, a grant on some
//!   specific names (in `SHOW GRANTS FINAL`, which merges the roles') may
//!   still create those: left unknown then. Drop: DROP DATABASE on the
//!   database.
//! - security: CREATE USER, CREATE ROLE or ROLE ADMIN (parts of ACCESS
//!   MANAGEMENT).
//!
//! ClickHouse has no "end a session" action in DBine: kill stays unknown.
//! A server without `CHECK GRANT` (older, or Timeplus Proton) leaves
//! everything unknown; only an unreachable server is an error.

use crate::schema::q;
use crate::{text, ClickHouseSession};
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::Value;

/// `Some(true/false)` per check; `None`: the server didn't answer it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Checks {
    pub backup: Option<bool>,
    /// WRITE ON DISK / FILE / S3.
    pub write_dest: Vec<Option<bool>>,
    /// CREATE TABLE and INSERT on the database.
    pub restore: Option<bool>,
    /// READ ON DISK / FILE / S3.
    pub read_src: Vec<Option<bool>>,
    /// SELECT on system.query_log, on system.processes.
    pub profiler: Vec<Option<bool>>,
    pub create_database: Option<bool>,
    /// CREATE DATABASE granted on some specific names (`None`: couldn't read
    /// the grants).
    pub create_database_named: Option<bool>,
    pub drop_database: Option<bool>,
    /// CREATE USER, CREATE ROLE, ROLE ADMIN.
    pub security: Vec<Option<bool>>,
    /// The user's `readonly` setting (> 0 refuses every write).
    pub readonly: Option<i64>,
}

/// Any true → true; otherwise false only when every answer came back.
fn any(v: &[Option<bool>]) -> Option<bool> {
    if v.contains(&Some(true)) {
        Some(true)
    } else if !v.is_empty() && v.iter().all(Option::is_some) {
        Some(false)
    } else {
        None
    }
}

/// Like `any`, but a destination nobody could check doesn't block (older
/// servers have no READ / WRITE source privileges).
fn any_or_skip(v: &[Option<bool>]) -> Option<bool> {
    if v.iter().all(Option::is_none) {
        Some(true)
    } else {
        any(v)
    }
}

pub(crate) fn decide(c: &Checks) -> Permissions {
    let readonly = c.readonly.is_some_and(|r| r > 0);
    let write_missing = |privilege: &str| -> String {
        if readonly {
            "readonly = 0 (el perfil del usuario es de solo lectura)".into()
        } else {
            privilege.into()
        }
    };
    let access = |v: Option<bool>, missing: String| v.map_or(Access::Unknown, |ok| Access::check(ok, missing));
    let backup = match (c.backup, any_or_skip(&c.write_dest)) {
        (Some(false), _) => Access::check(false, "BACKUP"),
        (Some(true), Some(false)) => Access::check(false, "WRITE ON DISK (o FILE / S3)"),
        (Some(true), Some(true)) => Access::Allowed,
        _ => Access::Unknown,
    };
    let restore = match (c.restore, any_or_skip(&c.read_src)) {
        (Some(false), _) => Access::check(false, write_missing("CREATE TABLE e INSERT sobre la base")),
        (Some(true), Some(false)) => Access::check(false, "READ ON DISK (o FILE / S3)"),
        (Some(true), Some(true)) => Access::Allowed,
        _ => Access::Unknown,
    };
    Permissions {
        backup,
        restore,
        profiler: access(any(&c.profiler), "SELECT ON system.query_log (o system.processes)".into()),
        create_database: match (c.create_database, c.create_database_named) {
            // Only named databases: those can still be created.
            (Some(false), named) if !readonly && named != Some(false) => Access::Unknown,
            (v, _) => access(v, write_missing("CREATE DATABASE")),
        },
        drop_database: access(c.drop_database, write_missing("DROP DATABASE")),
        manage_security: access(any(&c.security), write_missing("ACCESS MANAGEMENT")),
        ..Default::default()
    }
}

/// A `SHOW GRANTS` line grants CREATE DATABASE (itself, or through CREATE
/// or ALL) on something.
pub(crate) fn grants_create_database(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix("GRANT ") else { return false };
    let Some((privileges, _)) = rest.split_once(" ON ") else { return false };
    privileges.split(", ").map(|p| p.split('(').next().unwrap_or(p).trim()).any(|p| {
        p.eq_ignore_ascii_case("CREATE DATABASE") || p.eq_ignore_ascii_case("CREATE") || p.eq_ignore_ascii_case("ALL")
    })
}

fn truthy(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64(),
        other => text(other).trim().parse().ok(),
    }
}

impl ClickHouseSession {
    /// The first cell of `sql` as a number; `None` when the server refused
    /// it. An unreachable server is the error.
    async fn permission_value(&self, sql: String) -> Result<Option<i64>> {
        match self.rows(&sql, &[]).await {
            Ok(rows) => Ok(rows.first().and_then(|r| r.first()).and_then(truthy)),
            Err(e @ Error::Connect(_)) => Err(e),
            Err(e) => {
                tracing::debug!("clickhouse: permissions check refused: {e}");
                Ok(None)
            }
        }
    }

    async fn grant(&self, what: &str) -> Result<Option<bool>> {
        Ok(self.permission_value(format!("CHECK GRANT {what}")).await?.map(|v| v == 1))
    }

    async fn grants_of(&self, whats: &[String]) -> Result<Vec<Option<bool>>> {
        let all = futures::future::join_all(whats.iter().map(|w| self.grant(w))).await;
        all.into_iter().collect()
    }
}

/// The session without DBine's read-only mode, for the checks below only
/// (they never write): with `readonly = 1` every write privilege would
/// check as 0 whatever the user holds.
fn without_read_only(s: &ClickHouseSession) -> ClickHouseSession {
    ClickHouseSession {
        conn: s.conn.clone(),
        flavor: s.flavor,
        database: s.database.clone(),
        read_only: false,
        session_id: s.session_id.clone(),
        in_flight: s.in_flight.clone(),
        rt: s.rt.clone(),
        profiler: None,
    }
}

pub(crate) async fn check(s: &ClickHouseSession, database: Option<&str>) -> Result<Permissions> {
    let owned = s.read_only.then(|| without_read_only(s));
    let s = owned.as_ref().unwrap_or(s);
    // A server without CHECK GRANT refuses the first one: nothing to learn.
    let Some(create_database) = s.grant("CREATE DATABASE ON *.*").await? else {
        return Ok(Permissions::default());
    };
    let create_database_named = if create_database {
        Some(true)
    } else {
        match s.rows("SHOW GRANTS FINAL", &[]).await {
            Ok(rows) => Some(rows.iter().filter_map(|r| r.first()).any(|v| grants_create_database(&text(v)))),
            Err(e @ Error::Connect(_)) => return Err(e),
            Err(_) => None,
        }
    };
    let db = database.map(str::trim).filter(|d| !d.is_empty()).map(|d| format!("{}.*", q(d)));
    let sources = |op: &str| ["DISK", "FILE", "S3"].map(|src| format!("{op} ON {src}")).to_vec();
    let on_db = |what: &str| db.as_ref().map(|db| format!("{what} ON {db}"));
    let one = |w: Option<String>| async move {
        match w {
            Some(w) => s.grant(&w).await,
            None => Ok(None),
        }
    };
    let (backup, restore, drop_database) =
        futures::try_join!(one(on_db("BACKUP")), one(on_db("CREATE TABLE, INSERT")), one(on_db("DROP DATABASE")))?;
    let (writes, reads) = (sources("WRITE"), sources("READ"));
    let logs = ["SELECT ON system.query_log".to_string(), "SELECT ON system.processes".into()];
    let users = ["CREATE USER ON *.*".to_string(), "CREATE ROLE ON *.*".into(), "ROLE ADMIN ON *.*".into()];
    let (write_dest, read_src, profiler, security, readonly) = futures::try_join!(
        s.grants_of(&writes),
        s.grants_of(&reads),
        s.grants_of(&logs),
        s.grants_of(&users),
        s.permission_value("SELECT getSetting('readonly')".into()),
    )?;
    Ok(decide(&Checks {
        backup,
        write_dest,
        restore,
        read_src,
        profiler,
        create_database: Some(create_database),
        create_database_named,
        drop_database,
        security,
        readonly,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn all(v: bool) -> Checks {
        Checks {
            backup: Some(v),
            write_dest: vec![Some(v); 3],
            restore: Some(v),
            read_src: vec![Some(v); 3],
            profiler: vec![Some(v); 2],
            create_database: Some(v),
            create_database_named: Some(v),
            drop_database: Some(v),
            security: vec![Some(v); 3],
            readonly: Some(0),
        }
    }

    #[test]
    fn an_admin_may_do_everything_offered() {
        let p = decide(&all(true));
        assert_eq!((&p.backup, &p.restore, &p.profiler), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(p.kill_session, Access::Unknown);
    }

    #[test]
    fn a_plain_user_is_denied_by_privilege_name() {
        let p = decide(&all(false));
        assert!(denied(&p.backup, "BACKUP"));
        assert!(denied(&p.restore, "CREATE TABLE e INSERT"));
        assert!(denied(&p.profiler, "system.query_log"));
        assert!(denied(&p.create_database, "CREATE DATABASE"));
        assert!(denied(&p.drop_database, "DROP DATABASE"));
        assert!(denied(&p.manage_security, "ACCESS MANAGEMENT"));
    }

    #[test]
    fn backup_needs_a_destination_and_restore_a_source() {
        let c = Checks { write_dest: vec![Some(false), Some(false), Some(false)], read_src: vec![Some(false), Some(true), Some(false)], ..all(true) };
        let p = decide(&c);
        assert!(denied(&p.backup, "WRITE ON DISK"));
        assert_eq!(p.restore, Access::Allowed);
        // Servers without source privileges don't parse the check: skipped.
        let c = Checks { write_dest: vec![None; 3], read_src: vec![None; 3], ..all(true) };
        assert_eq!((decide(&c).backup, decide(&c).restore), (Access::Allowed, Access::Allowed));
    }

    #[test]
    fn readonly_names_the_setting() {
        let c = Checks { readonly: Some(1), create_database: Some(false), drop_database: Some(false), create_database_named: None, ..all(true) };
        let p = decide(&c);
        assert!(denied(&p.create_database, "readonly = 0"));
        assert!(denied(&p.drop_database, "perfil"));
        assert!(!denied(&p.drop_database, "conexión"));
    }

    #[test]
    fn unanswered_checks_stay_unknown() {
        let c = Checks { backup: None, restore: None, profiler: vec![Some(false), None], drop_database: None, security: vec![None; 3], ..all(false) };
        let p = decide(&c);
        assert_eq!((&p.backup, &p.restore, &p.profiler), (&Access::Unknown, &Access::Unknown, &Access::Unknown));
        assert_eq!((&p.drop_database, &p.manage_security), (&Access::Unknown, &Access::Unknown));
        assert!(p.create_database.is_denied());
    }

    #[test]
    fn create_database_on_named_databases_is_unknown() {
        let c = Checks { create_database_named: Some(true), ..all(false) };
        assert_eq!(decide(&c).create_database, Access::Unknown);
        let c = Checks { create_database_named: None, ..all(false) };
        assert_eq!(decide(&c).create_database, Access::Unknown);
        assert!(grants_create_database("GRANT CREATE DATABASE ON sales.* TO ana"));
        assert!(grants_create_database("GRANT SELECT, CREATE ON db.* TO ana"));
        assert!(grants_create_database("GRANT ALL ON db.* TO ana WITH GRANT OPTION"));
        assert!(!grants_create_database("GRANT SELECT(a, b), CREATE TABLE ON db.t TO ana"));
        assert!(!grants_create_database("REVOKE CREATE DATABASE ON db.* FROM ana"));
        assert!(!grants_create_database("GRANT reader TO ana"));
    }

    #[test]
    fn values_as_the_server_sends_them() {
        assert_eq!(truthy(&serde_json::json!(1)), Some(1));
        assert_eq!(truthy(&serde_json::json!("0")), Some(0));
        assert_eq!(truthy(&serde_json::json!(true)), Some(1));
        assert_eq!(truthy(&Value::Null), None);
    }
}

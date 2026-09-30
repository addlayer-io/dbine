//! What the login may do (`Session::permissions`), in one round trip.
//!
//! - MySQL (and Aurora / Cloud SQL), MariaDB and TiDB: `SHOW GRANTS`
//!   without `FOR`, which lists the account's grants and those of its
//!   active roles (MySQL and TiDB expand the active roles, MariaDB the
//!   current role). information_schema's USER_PRIVILEGES doesn't: it only
//!   has the account's own rows. Global (`*.*`) and database (`db`.*)
//!   grants are read, with MySQL's partial revokes; table and routine
//!   grants don't matter here.
//!   - backup: MySQL's CLONE needs BACKUP_ADMIN; TiDB's BACKUP needs
//!     BACKUP_ADMIN or SUPER and its RESTORE, RESTORE_ADMIN or SUPER.
//!     MariaDB and the managed services have no backup by SQL.
//!   - profiler: Performance Schema (SELECT on it) sees every connection;
//!     without it the processlist only shows others' with PROCESS. TiDB's
//!     slow query log and processlist need PROCESS.
//!   - kill: CONNECTION_ADMIN (MariaDB: CONNECTION ADMIN) or SUPER.
//!   - create database: global CREATE; with CREATE only on some database
//!     patterns it depends on the name, so it's left unknown.
//!   - drop database: DROP, global or on that database.
//!   - users and roles: CREATE USER.
//!   - global ALL PRIVILEGES allows everything.
//! - StarRocks: privileges come from roles (`CURRENT_ROLE()`); only the
//!   built-in ones are read, and only to allow: root everything, db_admin
//!   creating and dropping databases, user_admin managing users.
//! - OceanBase, SingleStore, Doris / VeloDB, Databend, Manticore and
//!   GreptimeDB: left unknown.
//!
//! A check the server refuses leaves everything unknown; only a broken
//! connection is an error.

use crate::session::{at, MySqlSession};
use crate::{err, Variant};
use dbine_driver::{Access, Permissions, Result};
use mysql_async::prelude::Queryable;
use mysql_async::Row;
use std::collections::HashSet;

/// Global and database-level grants from `SHOW GRANTS` lines. Privilege
/// names are upper case with `_` for spaces (`CREATE_USER`,
/// `CONNECTION_ADMIN`, `ALL_PRIVILEGES`), so MariaDB's and MySQL's match.
#[derive(Debug, Default)]
pub(crate) struct Grants {
    global: HashSet<String>,
    /// (database pattern, privileges): `_` and `%` are wildcards.
    schemas: Vec<(String, HashSet<String>)>,
    /// MySQL's partial revokes (`REVOKE … ON db.* FROM`).
    revoked: Vec<(String, HashSet<String>)>,
}

impl Grants {
    pub(crate) fn parse<S: AsRef<str>>(lines: &[S]) -> Grants {
        let mut g = Grants::default();
        for line in lines {
            let Some((revoke, privs, target)) = parse_line(line.as_ref()) else { continue };
            match (revoke, target) {
                (false, None) => g.global.extend(privs),
                (false, Some(db)) => g.schemas.push((db, privs)),
                (true, Some(db)) => g.revoked.push((db, privs)),
                (true, None) => {}
            }
        }
        g
    }

    fn global(&self, p: &str) -> bool {
        self.global.contains("ALL_PRIVILEGES") || self.global.contains(p)
    }

    /// `p` on database `db`: global, or on a pattern that matches it, and
    /// not partially revoked there.
    fn on(&self, db: &str, p: &str) -> bool {
        let has = |set: &HashSet<String>| set.contains(p) || set.contains("ALL_PRIVILEGES");
        let granted = self.global(p) || self.schemas.iter().any(|(pat, set)| has(set) && like(pat, db));
        granted && !self.revoked.iter().any(|(name, set)| has(set) && name == db)
    }

    /// `p` on some database only.
    fn on_some(&self, p: &str) -> bool {
        self.schemas.iter().any(|(_, set)| set.contains(p) || set.contains("ALL_PRIVILEGES"))
    }
}

/// `GRANT privs ON target …` / `REVOKE privs ON target …`: whether it's a
/// revoke, the privileges and the database (`None`: `*.*`). `None` for
/// role grants, PROXY and table or routine grants.
fn parse_line(line: &str) -> Option<(bool, HashSet<String>, Option<String>)> {
    let line = line.trim();
    let (revoke, rest) = if let Some(r) = strip_word(line, "GRANT ") {
        (false, r)
    } else {
        (true, strip_word(line, "REVOKE ")?)
    };
    let on = rest.find(" ON ")?;
    let privs = privileges(&rest[..on]);
    let target = rest[on + 4..].trim_start();
    if target.starts_with("*.*") {
        return Some((revoke, privs, None));
    }
    let (db, after) = ident(target)?;
    after.starts_with(".*").then_some((revoke, privs, Some(db)))
}

fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    s.get(..word.len()).filter(|p| p.eq_ignore_ascii_case(word)).map(|_| &s[word.len()..])
}

/// `SELECT (a, b), CREATE USER,DROP` into `{SELECT, CREATE_USER, DROP}`.
fn privileges(list: &str) -> HashSet<String> {
    let mut plain = String::new();
    let mut depth = 0;
    for c in list.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    plain
        .split(',')
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join("_").to_ascii_uppercase())
        .map(|p| if p == "ALL" { "ALL_PRIVILEGES".to_string() } else { p })
        .filter(|p| !p.is_empty())
        .collect()
}

/// A database name, backquoted (`` `a``b` ``) or bare, and what follows.
fn ident(s: &str) -> Option<(String, &str)> {
    if let Some(rest) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '`' {
                if matches!(chars.peek(), Some((_, '`'))) {
                    chars.next();
                    name.push('`');
                } else {
                    return Some((name, &rest[i + 1..]));
                }
            } else {
                name.push(c);
            }
        }
        None
    } else {
        let end = s.find(|c: char| c == '.' || c.is_whitespace())?;
        (end > 0).then(|| (s[..end].to_string(), &s[end..]))
    }
}

/// SQL LIKE as database grants use it: `%`, `_` and `\` to escape.
fn like(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('%') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('_') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('\\') if p.len() > 1 => n.first() == Some(&p[1]) && go(&p[2..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    go(&p, &n)
}

/// The grants of MySQL, MariaDB and TiDB to what the UI allows.
/// `product`: what the user picked (backups differ for managed services).
pub(crate) fn from_grants(product: Variant, g: &Grants, database: Option<&str>) -> Permissions {
    let v = product.base();
    let has = |p: &str| g.global(p);
    let sup = has("SUPER");
    let mariadb = v == Variant::MariaDb;
    let backup = match product {
        Variant::MySql => Access::check(has("BACKUP_ADMIN"), "BACKUP_ADMIN"),
        Variant::TiDb => Access::check(has("BACKUP_ADMIN") || sup, "BACKUP_ADMIN o SUPER"),
        _ => Access::Unknown,
    };
    let restore = match product {
        Variant::TiDb => Access::check(has("RESTORE_ADMIN") || sup, "RESTORE_ADMIN o SUPER"),
        _ => Access::Unknown,
    };
    let profiler = if v == Variant::TiDb {
        Access::check(has("PROCESS"), "PROCESS")
    } else {
        Access::check(has("PROCESS") || g.on("performance_schema", "SELECT"), "PROCESS (o SELECT sobre performance_schema)")
    };
    let kill = if mariadb { "CONNECTION ADMIN o SUPER" } else { "CONNECTION_ADMIN o SUPER" };
    let create_database = if has("CREATE") {
        Access::Allowed
    } else if g.on_some("CREATE") {
        // Allowed for the names its database grants match.
        Access::Unknown
    } else {
        Access::check(false, "CREATE")
    };
    Permissions {
        backup,
        restore,
        profiler,
        kill_session: Access::check(has("CONNECTION_ADMIN") || sup, kill),
        create_database,
        drop_database: database.map_or(Access::Unknown, |db| Access::check(g.on(db, "DROP"), "DROP sobre la base")),
        manage_security: Access::check(has("CREATE_USER"), "CREATE USER"),
    }
}

/// StarRocks' active roles (`root, db_admin`), allowing what the built-in
/// ones grant. Custom roles aren't read: what they don't cover stays unknown.
pub(crate) fn from_starrocks_roles(roles: &str, database: Option<&str>) -> Permissions {
    let roles: Vec<String> =
        roles.split(',').map(|r| r.trim().trim_matches(|c| c == '\'' || c == '`').to_ascii_lowercase()).collect();
    let is = |r: &str| roles.iter().any(|x| x == r);
    let mut p = Permissions::default();
    if is("root") {
        p = Permissions { kill_session: Access::Unknown, ..Permissions::all() };
    } else {
        if is("db_admin") {
            p.create_database = Access::Allowed;
            p.drop_database = Access::Allowed;
        }
        if is("user_admin") {
            p.manage_security = Access::Allowed;
        }
    }
    if database.is_none() {
        p.drop_database = Access::Unknown;
    }
    p
}

/// The first column of `sql`'s rows; `None` when the server refused it.
async fn first_column(s: &mut MySqlSession, sql: &str) -> Result<Option<Vec<String>>> {
    match s.conn.query::<Row, _>(sql).await {
        Ok(rows) => Ok(Some(rows.iter().filter_map(|r| at(r, 0)).collect())),
        Err(e @ (mysql_async::Error::Io(_) | mysql_async::Error::Driver(mysql_async::DriverError::ConnectionClosed))) => {
            Err(err(e))
        }
        Err(e) => {
            tracing::debug!("{:?}: permissions check refused: {e}", s.variant);
            Ok(None)
        }
    }
}

pub(crate) async fn check(s: &mut MySqlSession, database: Option<&str>) -> Result<Permissions> {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    Ok(match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::TiDb => first_column(s, "SHOW GRANTS")
            .await?
            .map(|lines| from_grants(s.product, &Grants::parse(&lines), database))
            .unwrap_or_default(),
        Variant::StarRocks => first_column(s, "SELECT CURRENT_ROLE()")
            .await?
            .and_then(|r| r.into_iter().next())
            .map(|roles| from_starrocks_roles(&roles, database))
            .unwrap_or_default(),
        _ => Permissions::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    const MYSQL_ROOT: [&str; 3] = [
        "GRANT SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, RELOAD, SHUTDOWN, PROCESS, FILE, SUPER, CREATE USER, CREATE ROLE ON *.* TO `root`@`localhost` WITH GRANT OPTION",
        "GRANT BACKUP_ADMIN,CLONE_ADMIN,CONNECTION_ADMIN,ROLE_ADMIN,SYSTEM_VARIABLES_ADMIN ON *.* TO `root`@`localhost` WITH GRANT OPTION",
        "GRANT PROXY ON ``@`` TO `root`@`localhost` WITH GRANT OPTION",
    ];

    #[test]
    fn mysql_root_may_do_what_mysql_offers() {
        let p = from_grants(Variant::MySql, &Grants::parse(&MYSQL_ROOT), Some("ventas"));
        assert_eq!(
            (&p.backup, &p.profiler, &p.kill_session, &p.create_database, &p.drop_database, &p.manage_security),
            (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed)
        );
        // No restore by SQL on MySQL.
        assert_eq!(p.restore, Access::Unknown);
        assert_eq!(from_grants(Variant::MySql, &Grants::parse(&MYSQL_ROOT), None).drop_database, Access::Unknown);
    }

    #[test]
    fn a_plain_account_is_denied_what_it_lacks() {
        let g = Grants::parse(&["GRANT USAGE ON *.* TO `ana`@`%`", "GRANT SELECT, INSERT ON `ventas`.* TO `ana`@`%`"]);
        let p = from_grants(Variant::MySql, &g, Some("ventas"));
        assert!(denied(&p.backup, "BACKUP_ADMIN"));
        assert!(denied(&p.profiler, "PROCESS"));
        assert!(denied(&p.kill_session, "CONNECTION_ADMIN"));
        assert!(denied(&p.create_database, "CREATE"));
        assert!(denied(&p.drop_database, "DROP"));
        assert!(denied(&p.manage_security, "CREATE USER"));
    }

    #[test]
    fn database_grants_and_patterns() {
        let g = Grants::parse(&[
            "GRANT CREATE, DROP ON `dbine\\_x`.* TO `ana`@`%`",
            "GRANT ALL PRIVILEGES ON `app%`.* TO `ana`@`%`",
            "GRANT SELECT ON `performance_schema`.* TO `ana`@`%`",
            "GRANT UPDATE ON `ventas`.`facturas` TO `ana`@`%`",
        ]);
        let drop = |db| from_grants(Variant::MySql, &g, Some(db)).drop_database;
        assert_eq!(drop("dbine_x"), Access::Allowed);
        assert!(drop("dbineAx").is_denied(), "escaped _ is literal");
        assert_eq!(drop("app_1"), Access::Allowed);
        assert!(drop("ventas").is_denied());
        let p = from_grants(Variant::MySql, &g, None);
        // CREATE on some names only: depends on the name.
        assert_eq!(p.create_database, Access::Unknown);
        assert_eq!(p.profiler, Access::Allowed);
    }

    #[test]
    fn partial_revokes() {
        let g = Grants::parse(&["GRANT DROP ON *.* TO `ana`@`%`", "REVOKE DROP ON `mysql`.* FROM `ana`@`%`"]);
        assert!(from_grants(Variant::MySql, &g, Some("mysql")).drop_database.is_denied());
        assert_eq!(from_grants(Variant::MySql, &g, Some("ventas")).drop_database, Access::Allowed);
    }

    #[test]
    fn mariadb_names_with_spaces() {
        let g = Grants::parse(&[
            "GRANT `lector` TO `ana`@`%`",
            "GRANT USAGE ON *.* TO `ana`@`%` IDENTIFIED BY PASSWORD '*3B75'",
            "GRANT PROCESS, CONNECTION ADMIN, CREATE USER ON *.* TO `lector`",
        ]);
        let p = from_grants(Variant::MariaDb, &g, Some("x"));
        assert_eq!((&p.kill_session, &p.profiler, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
        let none = from_grants(Variant::MariaDb, &Grants::parse(&["GRANT USAGE ON *.* TO `ana`@`%`"]), None);
        assert!(denied(&none.kill_session, "CONNECTION ADMIN o SUPER"));
    }

    #[test]
    fn all_privileges_allows_everything() {
        for v in [Variant::TiDb, Variant::MariaDb] {
            let p = from_grants(v, &Grants::parse(&["GRANT ALL PRIVILEGES ON *.* TO 'root'@'%' WITH GRANT OPTION"]), Some("d"));
            assert!(!p.kill_session.is_denied() && !p.drop_database.is_denied() && !p.manage_security.is_denied());
            assert_eq!(p.create_database, Access::Allowed);
        }
        let p = from_grants(Variant::TiDb, &Grants::parse(&["GRANT ALL ON *.* TO 'root'@'%'"]), None);
        assert_eq!((&p.backup, &p.restore), (&Access::Allowed, &Access::Allowed));
    }

    #[test]
    fn tidb_backup_restore_and_kill() {
        let g = Grants::parse(&["GRANT PROCESS ON *.* TO 'ana'@'%'", "GRANT CREATE,DROP ON `x`.* TO 'ana'@'%'", "GRANT BACKUP_ADMIN ON *.* TO 'ana'@'%'"]);
        let p = from_grants(Variant::TiDb, &g, Some("x"));
        assert_eq!((&p.backup, &p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert!(denied(&p.restore, "RESTORE_ADMIN"));
        assert!(denied(&p.kill_session, "CONNECTION_ADMIN o SUPER"));
        let sup = from_grants(Variant::TiDb, &Grants::parse(&["GRANT SUPER ON *.* TO 'ana'@'%'"]), None);
        assert_eq!((&sup.backup, &sup.restore, &sup.kill_session), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    }

    #[test]
    fn managed_services_have_no_backup_by_sql() {
        let p = from_grants(Variant::AuroraMySql, &Grants::parse(&MYSQL_ROOT), None);
        assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
        assert_eq!(p.kill_session, Access::Allowed);
    }

    #[test]
    fn quoted_names() {
        assert_eq!(parse_line("GRANT DROP ON `a``b`.* TO `u`@`%`").unwrap().2.as_deref(), Some("a`b"));
        assert!(parse_line("GRANT EXECUTE ON FUNCTION `d`.`f` TO `u`@`%`").is_none());
        assert!(parse_line("GRANT `r`@`%` TO `u`@`%`").is_none());
        assert!(parse_line("GRANT SELECT (`a`, `b`) ON `d`.`t` TO `u`@`%`").is_none());
        assert_eq!(privileges("SELECT (a, b), create user,DROP").len(), 3);
    }

    #[test]
    fn starrocks_roles() {
        let root = from_starrocks_roles("root", Some("d"));
        assert_eq!((&root.backup, &root.drop_database, &root.kill_session), (&Access::Allowed, &Access::Allowed, &Access::Unknown));
        let p = from_starrocks_roles("public, db_admin", None);
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Unknown, &Access::Unknown));
        assert_eq!(from_starrocks_roles("public", Some("d")), Permissions::default());
    }
}

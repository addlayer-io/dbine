//! Users, roles and permissions (docs/users-and-permissions.md) through
//! ClickHouse's SQL access control (`system.users`, `system.roles`,
//! `system.role_grants`, `system.grants`), which Timeplus Proton shares.
//!
//! Users defined in the server's configuration files (`users.xml`) are
//! read-only from SQL: they're marked as system. ClickHouse has no account
//! lock; disabling a user means `HOST NONE` (it can't sign in from
//! anywhere), and enabling it `HOST ANY`.
//!
//! What can be granted comes from `system.privileges`, so it follows the
//! server's version.

use crate::schema::{q, string_literal as lit};
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::{
    Error, Grant, ObjectRef, Principal, PrincipalKind, Privilege, Result, SecurityAction,
    SecuritySpec,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

pub(crate) fn spec(flavor: Flavor) -> SecuritySpec {
    let mut object_kinds = vec![
        "",
        "schema",
        "table",
        "view",
        "materialized_view",
        "dictionary",
    ];
    if flavor == Flavor::Timeplus {
        object_kinds.insert(2, "stream");
    }
    SecuritySpec {
        privileges: vec![
            "SELECT",
            "INSERT",
            "ALTER",
            "CREATE",
            "DROP",
            "TRUNCATE",
            "OPTIMIZE",
            "SHOW",
            "dictGet",
            "ALTER UPDATE",
            "ALTER DELETE",
            "CREATE TABLE",
            "CREATE VIEW",
            "CREATE DICTIONARY",
            "DROP TABLE",
            "KILL QUERY",
            "SYSTEM",
            "INTROSPECTION",
            "SOURCES",
            "ACCESS MANAGEMENT",
            "ALL",
        ],
        object_kinds,
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

const USERS: &str =
    "SELECT name, storage, toString(auth_type), toString(host_ip), toString(host_names),
       toString(host_names_regexp), toString(host_names_like), default_database
  FROM system.users ORDER BY name";
const ROLES: &str = "SELECT name, storage FROM system.roles ORDER BY name";
const ROLE_GRANTS: &str = "SELECT user_name, role_name, granted_role_name FROM system.role_grants";
/// Who has the global ROLE ADMIN / ACCESS MANAGEMENT: administrators.
const ADMINS: &str = "SELECT DISTINCT ifNull(user_name, role_name) FROM system.grants
 WHERE access_type IN ('ALL', 'ACCESS MANAGEMENT', 'ROLE ADMIN') AND database IS NULL AND is_partial_revoke = 0";
const GRANTS: &str = "SELECT access_type, database, table, column, is_partial_revoke, grant_option
  FROM system.grants WHERE user_name = {p:String} OR role_name = {p:String}
 ORDER BY database, table, access_type, column";
const MEMBER_OF: &str = "SELECT granted_role_name FROM system.role_grants WHERE user_name = {p:String} OR role_name = {p:String}";

fn cell(r: &[Value], i: usize) -> Option<String> {
    r.get(i).map(text).filter(|s| !s.is_empty())
}

fn flag(r: &[Value], i: usize) -> bool {
    matches!(cell(r, i).as_deref(), Some("1" | "true"))
}

/// Defined with SQL (`local_directory`, `replicated`, `metastore`…), not
/// in the configuration files.
fn from_sql(storage: &str) -> bool {
    !(storage.contains("xml") || storage == "local_api" || storage.contains("ldap"))
}

pub(crate) async fn principals(s: &mut ClickHouseSession) -> Result<Vec<Principal>> {
    let users = s.rows(USERS, &[]).await?;
    let roles = s.rows(ROLES, &[]).await?;
    let admins: HashSet<String> = s
        .rows(ADMINS, &[])
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| cell(r, 0))
        .collect();
    let mut out = Vec::new();
    for r in &users {
        let name = cell(r, 0).unwrap_or_default();
        let storage = cell(r, 1).unwrap_or_default();
        let empty = |i| matches!(cell(r, i).as_deref(), None | Some("[]"));
        let mut details = vec![("Almacenamiento".into(), storage.clone())];
        if let Some(a) = cell(r, 2).filter(|a| a != "[]") {
            details.push((
                "Autenticación".into(),
                a.trim_matches(|c| c == '[' || c == ']').replace('\'', ""),
            ));
        }
        if let Some(d) = cell(r, 7) {
            details.push(("Base predeterminada".into(), d));
        }
        out.push(Principal {
            superuser: Some(admins.contains(&name)),
            // HOST NONE: every host list empty.
            disabled: from_sql(&storage).then(|| (3..=6).all(empty)),
            can_login: Some(true),
            kind: PrincipalKind::User,
            member_of: Vec::new(),
            details,
            system: !from_sql(&storage),
            name,
        });
    }
    for r in &roles {
        let name = cell(r, 0).unwrap_or_default();
        let storage = cell(r, 1).unwrap_or_default();
        out.push(Principal {
            superuser: Some(admins.contains(&name)),
            kind: PrincipalKind::Role,
            details: vec![("Almacenamiento".into(), storage.clone())],
            system: !from_sql(&storage),
            name,
            ..Default::default()
        });
    }
    for r in s.rows(ROLE_GRANTS, &[]).await.unwrap_or_default() {
        let (Some(member), Some(role)) = (cell(&r, 0).or_else(|| cell(&r, 1)), cell(&r, 2)) else {
            continue;
        };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            p.member_of.push(role);
        }
    }
    Ok(out)
}

pub(crate) async fn grants(s: &mut ClickHouseSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> =
        VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 64 {
            continue;
        }
        for r in s.rows(GRANTS, &[("p", &name)]).await? {
            let privilege = cell(&r, 0).unwrap_or_default();
            let (db, table, column) = (cell(&r, 1), cell(&r, 2), cell(&r, 3));
            let (object, object_kind) = match (db, table) {
                (None, _) => (None, None),
                (Some(db), None) => (Some(db), Some("schema".to_string())),
                (Some(db), Some(t)) => (Some(format!("{db}.{t}")), Some("table".to_string())),
            };
            out.push(Grant {
                privilege: match column {
                    Some(c) => format!("{privilege} ({c})"),
                    None => privilege,
                },
                object,
                object_kind,
                grantable: flag(&r, 5),
                // A partial revoke takes a privilege away from a wider grant.
                denied: flag(&r, 4),
                via: via.clone(),
            });
        }
        for r in s.rows(MEMBER_OF, &[("p", &name)]).await? {
            if let Some(role) = cell(&r, 0) {
                let v = via.clone().unwrap_or_else(|| role.clone());
                queue.push_back((role, Some(v)));
            }
        }
    }
    Ok(out)
}

// -- what can be granted -----------------------------------------------------

/// Every privilege the server knows: its level and the group it belongs to
/// (`ALTER UPDATE` ⊂ `ALTER TABLE` ⊂ `ALTER` ⊂ `ALL`).
const PRIVILEGES: &str = "SELECT privilege, level, parent_group FROM system.privileges";

/// Where `system.privileges` can't be read: the hierarchy of
/// https://clickhouse.com/docs/sql-reference/statements/grant#privileges
/// that every server from 22.8 on accepts, as (privilege, level, group);
/// a group without a level of its own has "".
const DOCUMENTED: [(&str, &str, &str); 144] = [
    ("SHOW DATABASES", "DATABASE", "SHOW"),
    ("SHOW TABLES", "TABLE", "SHOW"),
    ("SHOW COLUMNS", "COLUMN", "SHOW"),
    ("SHOW DICTIONARIES", "DICTIONARY", "SHOW"),
    ("SHOW", "", "ALL"),
    ("SELECT", "COLUMN", "ALL"),
    ("INSERT", "COLUMN", "ALL"),
    ("ALTER UPDATE", "COLUMN", "ALTER TABLE"),
    ("ALTER DELETE", "COLUMN", "ALTER TABLE"),
    ("ALTER ADD COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER MODIFY COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER DROP COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER COMMENT COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER CLEAR COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER RENAME COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER MATERIALIZE COLUMN", "COLUMN", "ALTER COLUMN"),
    ("ALTER COLUMN", "", "ALTER TABLE"),
    ("ALTER MODIFY COMMENT", "TABLE", "ALTER TABLE"),
    ("ALTER ORDER BY", "TABLE", "ALTER INDEX"),
    ("ALTER SAMPLE BY", "TABLE", "ALTER INDEX"),
    ("ALTER ADD INDEX", "TABLE", "ALTER INDEX"),
    ("ALTER DROP INDEX", "TABLE", "ALTER INDEX"),
    ("ALTER MATERIALIZE INDEX", "TABLE", "ALTER INDEX"),
    ("ALTER CLEAR INDEX", "TABLE", "ALTER INDEX"),
    ("ALTER INDEX", "", "ALTER TABLE"),
    ("ALTER ADD PROJECTION", "TABLE", "ALTER PROJECTION"),
    ("ALTER DROP PROJECTION", "TABLE", "ALTER PROJECTION"),
    ("ALTER MATERIALIZE PROJECTION", "TABLE", "ALTER PROJECTION"),
    ("ALTER CLEAR PROJECTION", "TABLE", "ALTER PROJECTION"),
    ("ALTER PROJECTION", "", "ALTER TABLE"),
    ("ALTER ADD CONSTRAINT", "TABLE", "ALTER CONSTRAINT"),
    ("ALTER DROP CONSTRAINT", "TABLE", "ALTER CONSTRAINT"),
    ("ALTER CONSTRAINT", "", "ALTER TABLE"),
    ("ALTER TTL", "TABLE", "ALTER TABLE"),
    ("ALTER MATERIALIZE TTL", "TABLE", "ALTER TABLE"),
    ("ALTER SETTINGS", "TABLE", "ALTER TABLE"),
    ("ALTER MOVE PARTITION", "TABLE", "ALTER TABLE"),
    ("ALTER FETCH PARTITION", "TABLE", "ALTER TABLE"),
    ("ALTER FREEZE PARTITION", "TABLE", "ALTER TABLE"),
    ("ALTER DATABASE SETTINGS", "DATABASE", "ALTER DATABASE"),
    ("ALTER TABLE", "", "ALTER"),
    ("ALTER DATABASE", "", "ALTER"),
    ("ALTER VIEW MODIFY QUERY", "VIEW", "ALTER VIEW"),
    ("ALTER VIEW", "", "ALTER"),
    ("ALTER", "", "ALL"),
    ("CREATE DATABASE", "DATABASE", "CREATE"),
    ("CREATE TABLE", "TABLE", "CREATE"),
    ("CREATE VIEW", "VIEW", "CREATE"),
    ("CREATE DICTIONARY", "DICTIONARY", "CREATE"),
    ("CREATE TEMPORARY TABLE", "GLOBAL", "CREATE"),
    ("CREATE FUNCTION", "GLOBAL", "CREATE"),
    ("CREATE", "", "ALL"),
    ("DROP DATABASE", "DATABASE", "DROP"),
    ("DROP TABLE", "TABLE", "DROP"),
    ("DROP VIEW", "VIEW", "DROP"),
    ("DROP DICTIONARY", "DICTIONARY", "DROP"),
    ("DROP FUNCTION", "GLOBAL", "DROP"),
    ("DROP", "", "ALL"),
    ("TRUNCATE", "TABLE", "ALL"),
    ("OPTIMIZE", "TABLE", "ALL"),
    ("BACKUP", "TABLE", "ALL"),
    ("KILL QUERY", "GLOBAL", "ALL"),
    ("KILL TRANSACTION", "GLOBAL", "ALL"),
    ("MOVE PARTITION BETWEEN SHARDS", "GLOBAL", "ALL"),
    ("CREATE USER", "GLOBAL", "ACCESS MANAGEMENT"),
    ("ALTER USER", "GLOBAL", "ACCESS MANAGEMENT"),
    ("DROP USER", "GLOBAL", "ACCESS MANAGEMENT"),
    ("CREATE ROLE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("ALTER ROLE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("DROP ROLE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("ROLE ADMIN", "GLOBAL", "ACCESS MANAGEMENT"),
    ("CREATE ROW POLICY", "TABLE", "ACCESS MANAGEMENT"),
    ("ALTER ROW POLICY", "TABLE", "ACCESS MANAGEMENT"),
    ("DROP ROW POLICY", "TABLE", "ACCESS MANAGEMENT"),
    ("CREATE QUOTA", "GLOBAL", "ACCESS MANAGEMENT"),
    ("ALTER QUOTA", "GLOBAL", "ACCESS MANAGEMENT"),
    ("DROP QUOTA", "GLOBAL", "ACCESS MANAGEMENT"),
    ("CREATE SETTINGS PROFILE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("ALTER SETTINGS PROFILE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("DROP SETTINGS PROFILE", "GLOBAL", "ACCESS MANAGEMENT"),
    ("SHOW USERS", "GLOBAL", "SHOW ACCESS"),
    ("SHOW ROLES", "GLOBAL", "SHOW ACCESS"),
    ("SHOW ROW POLICIES", "TABLE", "SHOW ACCESS"),
    ("SHOW QUOTAS", "GLOBAL", "SHOW ACCESS"),
    ("SHOW SETTINGS PROFILES", "GLOBAL", "SHOW ACCESS"),
    ("SHOW ACCESS", "", "ACCESS MANAGEMENT"),
    ("ACCESS MANAGEMENT", "", "ALL"),
    ("SYSTEM SHUTDOWN", "GLOBAL", "SYSTEM"),
    ("SYSTEM DROP DNS CACHE", "GLOBAL", "SYSTEM DROP CACHE"),
    ("SYSTEM DROP MARK CACHE", "GLOBAL", "SYSTEM DROP CACHE"),
    (
        "SYSTEM DROP UNCOMPRESSED CACHE",
        "GLOBAL",
        "SYSTEM DROP CACHE",
    ),
    ("SYSTEM DROP MMAP CACHE", "GLOBAL", "SYSTEM DROP CACHE"),
    (
        "SYSTEM DROP COMPILED EXPRESSION CACHE",
        "GLOBAL",
        "SYSTEM DROP CACHE",
    ),
    (
        "SYSTEM DROP FILESYSTEM CACHE",
        "GLOBAL",
        "SYSTEM DROP CACHE",
    ),
    ("SYSTEM DROP SCHEMA CACHE", "GLOBAL", "SYSTEM DROP CACHE"),
    ("SYSTEM DROP CACHE", "", "SYSTEM"),
    ("SYSTEM RELOAD CONFIG", "GLOBAL", "SYSTEM RELOAD"),
    ("SYSTEM RELOAD DICTIONARY", "GLOBAL", "SYSTEM RELOAD"),
    ("SYSTEM RELOAD MODEL", "GLOBAL", "SYSTEM RELOAD"),
    ("SYSTEM RELOAD FUNCTION", "GLOBAL", "SYSTEM RELOAD"),
    (
        "SYSTEM RELOAD EMBEDDED DICTIONARIES",
        "GLOBAL",
        "SYSTEM RELOAD",
    ),
    ("SYSTEM RELOAD", "", "SYSTEM"),
    ("SYSTEM RESTART DISK", "GLOBAL", "SYSTEM"),
    ("SYSTEM MERGES", "TABLE", "SYSTEM"),
    ("SYSTEM TTL MERGES", "TABLE", "SYSTEM"),
    ("SYSTEM FETCHES", "TABLE", "SYSTEM"),
    ("SYSTEM MOVES", "TABLE", "SYSTEM"),
    ("SYSTEM DISTRIBUTED SENDS", "TABLE", "SYSTEM SENDS"),
    ("SYSTEM REPLICATED SENDS", "TABLE", "SYSTEM SENDS"),
    ("SYSTEM SENDS", "", "SYSTEM"),
    ("SYSTEM REPLICATION QUEUES", "TABLE", "SYSTEM"),
    ("SYSTEM DROP REPLICA", "TABLE", "SYSTEM"),
    ("SYSTEM SYNC REPLICA", "TABLE", "SYSTEM"),
    ("SYSTEM RESTART REPLICA", "TABLE", "SYSTEM"),
    ("SYSTEM RESTORE REPLICA", "TABLE", "SYSTEM"),
    ("SYSTEM SYNC DATABASE REPLICA", "DATABASE", "SYSTEM"),
    ("SYSTEM SYNC TRANSACTION LOG", "GLOBAL", "SYSTEM"),
    ("SYSTEM FLUSH DISTRIBUTED", "TABLE", "SYSTEM FLUSH"),
    ("SYSTEM FLUSH LOGS", "GLOBAL", "SYSTEM FLUSH"),
    ("SYSTEM FLUSH", "", "SYSTEM"),
    ("SYSTEM THREAD FUZZER", "GLOBAL", "SYSTEM"),
    ("SYSTEM UNFREEZE", "GLOBAL", "SYSTEM"),
    ("SYSTEM", "", "ALL"),
    ("dictGet", "DICTIONARY", "ALL"),
    ("addressToLine", "GLOBAL", "INTROSPECTION"),
    ("addressToLineWithInlines", "GLOBAL", "INTROSPECTION"),
    ("addressToSymbol", "GLOBAL", "INTROSPECTION"),
    ("demangle", "GLOBAL", "INTROSPECTION"),
    ("INTROSPECTION", "", "ALL"),
    ("CLUSTER", "GLOBAL", "ALL"),
    ("FILE", "GLOBAL", "ALL"),
    ("URL", "GLOBAL", "ALL"),
    ("REMOTE", "GLOBAL", "ALL"),
    ("MONGO", "GLOBAL", "ALL"),
    ("MYSQL", "GLOBAL", "ALL"),
    ("POSTGRES", "GLOBAL", "ALL"),
    ("SQLITE", "GLOBAL", "ALL"),
    ("ODBC", "GLOBAL", "ALL"),
    ("JDBC", "GLOBAL", "ALL"),
    ("HDFS", "GLOBAL", "ALL"),
    ("S3", "GLOBAL", "ALL"),
    ("HIVE", "GLOBAL", "ALL"),
    ("SOURCES", "GLOBAL", "ALL"),
    ("ALL", "", ""),
];

/// What a privilege of this level can be granted on: its own kind and the
/// wider ones (`*.*`, `db.*`). A grant doesn't tell a table from a view, so
/// table and column privileges go on every table-like kind. The levels
/// DBine has no kind for (named collections, users, table engines,
/// sources, definers) are granted `ON *.*`: on every one of them.
fn level_kinds(level: &str, flavor: Flavor) -> &'static [&'static str] {
    match (level, flavor) {
        ("DATABASE", _) => &["", "schema"],
        ("TABLE" | "COLUMN", Flavor::Timeplus) => {
            &["", "schema", "stream", "table", "view", "materialized_view"]
        }
        ("TABLE" | "COLUMN", _) => &["", "schema", "table", "view", "materialized_view"],
        ("VIEW", _) => &["", "schema", "view", "materialized_view"],
        ("DICTIONARY", _) => &["", "schema", "dictionary"],
        _ => &[""],
    }
}

/// The privileges in the rows' order, from (privilege, level, group): a
/// group goes wherever one of its members does (`GRANT SYSTEM ON db.t`
/// grants the table-level SYSTEM ones), anything at least on `*.*` (a
/// group without members, like 22.8's SHOW CACHES), and NONE (`USAGE`)
/// grants nothing. Only names the GRANT script takes are kept.
fn from_rows(rows: &[(String, String, String)], flavor: Flavor) -> Vec<Privilege> {
    let group: HashMap<&str, &str> = rows
        .iter()
        .map(|(n, _, g)| (n.as_str(), g.as_str()))
        .collect();
    let mut on: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (name, level, _) in rows.iter().filter(|(_, l, _)| !l.is_empty()) {
        let kinds = level_kinds(level, flavor);
        let mut n = name.as_str();
        // The privilege and its groups up to ALL (bounded, should the
        // catalog ever loop).
        for _ in 0..rows.len() {
            on.entry(n).or_default().extend(kinds);
            match group.get(n) {
                Some(g) if !g.is_empty() => n = g,
                _ => break,
            }
        }
    }
    let order = spec(flavor).object_kinds;
    rows.iter()
        .filter(|(n, ..)| n != "NONE" && privileges(std::slice::from_ref(n)).is_ok())
        .map(|(n, ..)| {
            let kinds = on.get(n.as_str());
            Privilege {
                name: n.clone(),
                on: order
                    .iter()
                    .filter(|k| k.is_empty() || kinds.is_some_and(|s| s.contains(*k)))
                    .map(|k| k.to_string())
                    .collect(),
            }
        })
        .collect()
}

fn documented(flavor: Flavor) -> Vec<Privilege> {
    let rows: Vec<(String, String, String)> = DOCUMENTED
        .iter()
        .map(|(n, l, g)| (n.to_string(), l.to_string(), g.to_string()))
        .collect();
    from_rows(&rows, flavor)
}

/// What the server can grant, from its catalog; the documented hierarchy
/// where the catalog can't be read (an older server, a login without
/// access to `system`).
pub(crate) async fn privileges_of(s: &mut ClickHouseSession) -> Vec<Privilege> {
    let rows: Vec<(String, String, String)> = s
        .rows(PRIVILEGES, &[])
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            Some((
                cell(r, 0)?,
                cell(r, 1).unwrap_or_default(),
                cell(r, 2).unwrap_or_default(),
            ))
        })
        .collect();
    match from_rows(&rows, s.flavor) {
        p if p.is_empty() => documented(s.flavor),
        p => p,
    }
}

// -- scripts -----------------------------------------------------------------

/// What a privilege applies to: everything, a database or a table.
fn on(object: &Option<ObjectRef>) -> String {
    match object {
        None => "*.*".into(),
        Some(o) if o.kind == "schema" || o.kind == "database" => format!("{}.*", q(&o.name)),
        Some(o) => match o.schema() {
            Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
            None => q(&o.name),
        },
    }
}

/// Privilege names: a letter, then letters, digits (`S3`), spaces and
/// underscores, optionally with a column list (`SELECT (a, b)`).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de ClickHouse"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (
                n.trim(),
                Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?),
            ),
            None => (x.trim(), None),
        };
        if !name.starts_with(|c: char| c.is_ascii_alphabetic())
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_')
        {
            return Err(bad(x));
        }
        match cols {
            Some(c) => {
                let cols: Vec<String> = c
                    .split(',')
                    .map(|c| c.trim().trim_matches('`'))
                    .filter(|c| !c.is_empty())
                    .map(q)
                    .collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name}({})", cols.join(", ")));
            }
            None => out.push(name.to_string()),
        }
    }
    Ok(out.join(", "))
}

pub(crate) fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password
                .as_deref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!(
                "CREATE USER {} IDENTIFIED WITH sha256_password BY {};",
                q(name),
                lit(pw)
            )
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop {
            name,
            kind: PrincipalKind::Role,
        } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop {
            name,
            kind: PrincipalKind::User,
        } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password } => {
            format!(
                "ALTER USER {} IDENTIFIED WITH sha256_password BY {};",
                q(name),
                lit(password)
            )
        }
        SecurityAction::SetLogin {
            name,
            enabled: false,
        } => {
            format!("-- ClickHouse no bloquea cuentas: sin hosts permitidos, no puede ingresar.\nALTER USER {} HOST NONE;", q(name))
        }
        SecurityAction::SetLogin {
            name,
            enabled: true,
        } => {
            format!("-- Permite el ingreso desde cualquier host (reemplaza la lista anterior).\nALTER USER {} HOST ANY;", q(name))
        }
        SecurityAction::Grant {
            privileges: p,
            object,
            to,
            grantable,
        } => {
            format!(
                "GRANT {} ON {} TO {}{};",
                privileges(p)?,
                on(object),
                q(to),
                if *grantable { " WITH GRANT OPTION" } else { "" }
            )
        }
        SecurityAction::Revoke {
            privileges: p,
            object,
            from,
        } => format!(
            "REVOKE {} ON {} FROM {};",
            privileges(p)?,
            on(object),
            q(from)
        ),
        SecurityAction::AddMember { role, member } => {
            format!("GRANT {} TO {};", q(role), q(member))
        }
        SecurityAction::RemoveMember { role, member } => {
            format!("REVOKE {} FROM {};", q(role), q(member))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser {
                name: "ana`x".into(),
                password: Some("p'w".into())
            }),
            "CREATE USER `ana\\`x` IDENTIFIED WITH sha256_password BY 'p\\'w';"
        );
        assert!(script(&SecurityAction::CreateUser {
            name: "ana".into(),
            password: None
        })
        .is_err());
        assert_eq!(
            s(SecurityAction::CreateRole {
                name: "lectores".into()
            }),
            "CREATE ROLE `lectores`;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["SELECT (id, total)".into(), "INSERT".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: "facturas".into() }),
                to: "ana".into(),
                grantable: true,
            }),
            "GRANT SELECT(`id`, `total`), INSERT ON `ventas`.`facturas` TO `ana` WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["dictGet".into()],
                object: Some(ObjectRef {
                    kind: "schema".into(),
                    schema: None,
                    name: "d".into()
                }),
                from: "r".into()
            }),
            "REVOKE dictGet ON `d`.* FROM `r`;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["SHOW".into()],
                object: None,
                to: "r".into(),
                grantable: false
            }),
            "GRANT SHOW ON *.* TO `r`;"
        );
        assert!(s(SecurityAction::SetLogin {
            name: "ana".into(),
            enabled: false
        })
        .ends_with("ALTER USER `ana` HOST NONE;"));
        assert_eq!(
            s(SecurityAction::AddMember {
                role: "r".into(),
                member: "ana".into()
            }),
            "GRANT `r` TO `ana`;"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember {
                role: "r".into(),
                member: "ana".into()
            }),
            "REVOKE `r` FROM `ana`;"
        );
        assert!(script(&SecurityAction::Grant {
            privileges: vec!["SELECT; DROP TABLE x".into()],
            object: None,
            to: "a".into(),
            grantable: false
        })
        .is_err());
    }

    fn rows(r: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        r.iter()
            .map(|(n, l, g)| (n.to_string(), l.to_string(), g.to_string()))
            .collect()
    }

    #[test]
    fn maps_the_catalog() {
        let catalog = rows(&[
            ("SHOW DATABASES", "DATABASE", "SHOW"),
            ("SHOW TABLES", "TABLE", "SHOW"),
            ("SHOW", "", "ALL"),
            ("SELECT", "COLUMN", "ALL"),
            ("ALTER VIEW MODIFY QUERY", "VIEW", "ALTER VIEW"),
            ("ALTER VIEW", "", "ALL"),
            ("dictGet", "DICTIONARY", "ALL"),
            ("KILL QUERY", "GLOBAL", "ALL"),
            ("NAMED COLLECTION", "NAMED_COLLECTION", "ALL"),
            ("S3", "GLOBAL", "SOURCES"),
            ("SOURCES", "", "ALL"),
            ("SHOW CACHES", "", "ALL"),
            ("ALL", "", ""),
            ("NONE", "", ""),
            ("BAD-NAME", "GLOBAL", "ALL"),
        ]);
        let p = from_rows(&catalog, Flavor::ClickHouse);
        let on = |n: &str| p.iter().find(|x| x.name == n).map(|x| x.on.join(","));
        assert_eq!(on("SHOW DATABASES").as_deref(), Some(",schema"));
        assert_eq!(
            on("SELECT").as_deref(),
            Some(",schema,table,view,materialized_view")
        );
        assert_eq!(
            on("ALTER VIEW MODIFY QUERY").as_deref(),
            Some(",schema,view,materialized_view")
        );
        assert_eq!(on("dictGet").as_deref(), Some(",schema,dictionary"));
        assert_eq!(on("KILL QUERY").as_deref(), Some(""));
        assert_eq!(on("NAMED COLLECTION").as_deref(), Some(""));
        // A group: wherever its members go.
        assert_eq!(
            on("SHOW").as_deref(),
            Some(",schema,table,view,materialized_view")
        );
        assert_eq!(on("SOURCES").as_deref(), Some(""));
        assert_eq!(
            on("ALL").as_deref(),
            Some(",schema,table,view,materialized_view,dictionary")
        );
        assert_eq!(on("S3").as_deref(), Some(""));
        assert_eq!(on("SHOW CACHES").as_deref(), Some(""));
        assert_eq!(on("NONE"), None);
        assert_eq!(on("BAD-NAME"), None);
        assert_eq!(p[0].name, "SHOW DATABASES");
        let timeplus = from_rows(&catalog, Flavor::Timeplus);
        let select = timeplus.iter().find(|x| x.name == "SELECT").unwrap();
        assert_eq!(
            select.on,
            ["", "schema", "stream", "table", "view", "materialized_view"]
        );
    }

    #[test]
    fn every_documented_privilege_has_a_script() {
        for flavor in [Flavor::ClickHouse, Flavor::Timeplus] {
            let kinds = spec(flavor).object_kinds;
            let all = documented(flavor);
            assert_eq!(all.len(), DOCUMENTED.len());
            for p in all {
                assert!(!p.on.is_empty(), "{}", p.name);
                for k in &p.on {
                    assert!(kinds.contains(&k.as_str()), "{} on {k}", p.name);
                    let object = match k.as_str() {
                        "" => None,
                        "schema" => Some(ObjectRef {
                            kind: k.clone(),
                            schema: None,
                            name: "d".into(),
                        }),
                        _ => Some(ObjectRef {
                            kind: k.clone(),
                            schema: Some("d".into()),
                            name: "t".into(),
                        }),
                    };
                    let sql = script(&SecurityAction::Grant {
                        privileges: vec![p.name.clone()],
                        object,
                        to: "r".into(),
                        grantable: false,
                    })
                    .unwrap_or_else(|e| panic!("{} on {k}: {e}", p.name));
                    assert!(sql.starts_with(&format!("GRANT {} ON ", p.name)), "{sql}");
                }
            }
        }
        assert!(privileges(&["S3".into(), "SYSTEM DROP S3 CLIENT CACHE".into()]).is_ok());
        for bad in ["3D", "S3; DROP USER x", "S-3", ""] {
            assert!(privileges(&[bad.into()]).is_err(), "{bad}");
        }
    }
}
